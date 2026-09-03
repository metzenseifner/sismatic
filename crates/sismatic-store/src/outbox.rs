use std::sync::Arc;
use std::time::Duration;

use sismatic_api_types::{
    Acceptance, Barrier, BatchId, DeviceId, GroupId, Intent, Phase, ReadingValue, RecordingPhase,
    RecordingState, Rejection, Timestamp, WritingId, WritingRecord,
};

use crate::{ReadError, WriteError};

pub type DynWritingSubmit = Arc<dyn WritingSubmit>;
pub type DynWritingLog = Arc<dyn WritingLog>;
pub type DynWritingDrain = Arc<dyn WritingDrain>;

/// Everything one submission carries, grouped because the values are only
/// meaningful together and an adapter needs all of them inside one critical
/// section. Passing them as arguments would let a caller reorder the `String`s
/// silently.
///
/// `targets` is a list rather than a single id because a group-addressed
/// request is admitted across every member *at once*. A device-addressed
/// request is the one-element case, so there is one code path and no branch on
/// "is this a group" inside the critical section — which matters, because that
/// branch would be the place a group's atomicity quietly stopped holding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    /// One id per target, in the same order, minted by the caller so a retry
    /// can carry the same ids. An adapter must reject a length mismatch rather
    /// than zipping the shorter of the two.
    pub ids: Vec<WritingId>,
    pub targets: Vec<DeviceId>,
    /// The group this request was addressed to, when it was addressed to one.
    ///
    /// Carried alongside the expanded `targets` rather than in place of them,
    /// because the two are needed for different things: the members are what
    /// admission is decided against, and the group is what the *expectation* is
    /// filed under (see [`group`](crate::group)). Recording it here rather than
    /// through a second call is what makes the expectation atomic with the
    /// admission it describes — a submission the table refuses must leave no
    /// trace, including no claim about what the device group was going to be.
    ///
    /// `None` for a device-addressed request. A device is not a device group,
    /// and a single device's own phase already says what it was told.
    pub group: Option<GroupId>,
    /// `Some` when the members must act together — a rendezvous is armed and no
    /// row is dispatched until every one is ready. `None` for a single device,
    /// and for a group write that gains nothing from acting in unison.
    pub batch: Option<BatchId>,
    /// What to do if the rendezvous does not fill in time. Read only when
    /// `batch` is `Some`.
    pub barrier: Option<BarrierPolicy>,
    pub intent: Intent,
    pub at: Timestamp,
    /// When present, a repeat submission with the same `(device, key)` returns
    /// the original writing instead of appending a second one.
    pub idempotency_key: Option<String>,
}

/// The barrier, as the outbox needs it: a policy and the bound it applies at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarrierPolicy {
    pub timeout: Duration,
    pub on_timeout: Barrier,
}

/// Why a submission did not become a queued writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    /// The admission table refused it.
    ///
    /// Carries the phase that refused *and* the device whose phase it was: a
    /// group submission is admitted across every member, so "already
    /// recording" is not actionable without knowing which member said it.
    Rejected {
        device: DeviceId,
        rejection: Rejection,
        phase: Phase,
    },
    /// The caller handed a `Submission` that does not describe anything: no
    /// targets, or a different number of ids than targets. A programming error
    /// rather than a storage failure, kept distinct so it cannot be read as a
    /// transient backend problem and retried.
    Malformed(String),
    /// The storage backend failed.
    Backend(String),
}

/// Appending intent. The only write capability the HTTP layer is given.
#[async_trait::async_trait]
pub trait WritingSubmit: Send + Sync {
    /// Admit `submission` against the device's current phase and, if admitted,
    /// append it to that device's queue.
    ///
    /// **Atomicity is part of this contract**, and for a group it is atomicity
    /// across *every* target. The admission decision and the append are one
    /// unit: an implementation that reads one member's phase, releases its
    /// lock, and then appends does not satisfy this trait, and neither does one
    /// that admits member A, finds member B refuses, and leaves A's row behind.
    /// A refused submission records nothing at all.
    ///
    /// When [`Submission::group`] is set, "records nothing at all" includes the
    /// group expectation [`expects`](crate::group::expects) derives from the
    /// intent: it is written inside the same unit, so a group that was refused
    /// never claims to have been asked for something.
    async fn submit(&self, submission: Submission) -> Result<Acceptance, SubmitError>;
}

/// Reading what was submitted. Absence is never an error, matching
/// [`ReadStore`](crate::ReadStore).
#[async_trait::async_trait]
pub trait WritingLog: Send + Sync {
    async fn writing(&self, id: WritingId) -> Result<Option<WritingRecord>, ReadError>;

    /// Every writing recorded for `device`, newest first.
    async fn writings_for(&self, device: DeviceId) -> Result<Vec<WritingRecord>, ReadError>;

    /// The device's phase and epoch. An unknown device reports
    /// `Phase::Idle` at epoch 0, for the same reason `latest` reports `None`:
    /// this port holds what was submitted and no catalog of what exists.
    async fn phase(&self, device: DeviceId) -> Result<RecordingPhase, ReadError>;
}

/// Fold the device's observed state into the device's phase state
/// to avoid the residual race (an operator started a recording from the front panel)
///
/// without this, the device's phase would be "Idle" while the device is actually recording.
pub const fn reconcile(phase: Phase, observed: RecordingState) -> Phase {
    match (phase, observed) {
        (p, RecordingState::Unknown) => p,
        (_, RecordingState::Stopped) => Phase::Idle,
        (_, RecordingState::Started) => Phase::Recording,
        (_, RecordingState::Paused) => Phase::Paused,
    }
}

/// What was claimed and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Succeeded(ReadingValue),
    /// Retryable if `attempts < max_attempts`; the adapter decides.
    Failed(String),
}

/// What a claim yielded.
///
/// Two cases rather than always a `Vec`, because the caller does genuinely
/// different things with them: a lone writing goes to `Device::run`, a batch
/// goes to `DeviceGroup::run` so the members act in unison. Collapsing them
/// into a one-element list would put the "is this really a group" branch back
/// in the relay, decided by a length rather than by the outbox that armed the
/// rendezvous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// One writing for the device that asked. The ordinary path.
    One(WritingRecord),
    /// A whole batch, handed to the member whose arrival completed the barrier
    /// (or whose tick found it timed out under [`Barrier::DispatchReady`]).
    ///
    /// Exactly one member's task ever receives this for a given batch — the
    /// others find their row already claimed and move on — so the group run
    /// happens once, from one task, and the members go out together.
    Batch {
        id: BatchId,
        /// One record per member being dispatched, ordered by device id.
        ///
        /// Sorted rather than in the group's configured order, because the
        /// barrier tracks membership as a set. Nothing downstream depends on
        /// the order — `DeviceGroup::run` spawns every member's exchange before
        /// awaiting any, so the members act in unison regardless — and a stable
        /// order makes a batch's records comparable between runs.
        ///
        /// Under `DispatchReady` after a timeout this is a *subset* of the
        /// batch: the members that never arrived are not included.
        records: Vec<WritingRecord>,
    },
}

/// Draining the queue. Held only by `sismatic-intent-relay`.
#[async_trait::async_trait]
pub trait WritingDrain: Send + Sync {
    /// Take the oldest claimable writing for `device`, mark it `InFlight`, and
    /// return it.
    ///
    /// FIFO order per device is part of this contract: a metadata write
    /// submitted before a start must be claimed before it.
    ///
    /// A batched row at the head of a device's queue is *not* claimable until
    /// every sibling has reached the head of its own queue. Until then this
    /// answers `None` — and it must leave that row where it is rather than
    /// setting it aside, because the writing behind it would otherwise
    /// overtake, which is the exact reordering per-device FIFO exists to
    /// prevent. When the last member arrives, that member's call returns
    /// [`Claim::Batch`] carrying every row.
    async fn claim_next(
        &self,
        device: DeviceId,
        at: Timestamp,
    ) -> Result<Option<Claim>, WriteError>;

    /// Record what happened. A `Failed` outcome on a writing whose recorded
    /// transition moved the phase rolls that phase back, so a start that never
    /// reached the device does not freeze metadata forever.
    async fn settle(
        &self,
        id: WritingId,
        outcome: Outcome,
        at: Timestamp,
    ) -> Result<(), WriteError>;

    /// Fold a state the device actually reported into the phase. This is how a
    /// recording started from the device's front panel becomes visible to the
    /// admission table.
    async fn observe(&self, device: DeviceId, observed: RecordingState) -> Result<(), WriteError>;

    /// Writings left `InFlight` by a previous process. Returned so the relay
    /// can decide per writing whether to replay; see the recovery section.
    async fn in_flight(&self, device: DeviceId) -> Result<Vec<WritingRecord>, WriteError>;
}

/// An [`Intent`] with its payload stripped. The admission table depends on the
/// verb alone, and a `Copy` argument is what lets that table be a `const fn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Start,
    Stop,
    Pause,
    WriteMetadata,
    WriteSetting,
}

impl Verb {
    /// Total and wildcard-free: a new [`Intent`] variant stops this compiling
    /// until it has been classified.
    pub fn of(intent: &Intent) -> Self {
        match intent {
            Intent::StartRecording => Verb::Start,
            Intent::StopRecording => Verb::Stop,
            Intent::PauseRecording => Verb::Pause,
            Intent::SetMetadata { .. } => Verb::WriteMetadata,
            Intent::SetSetting { .. } => Verb::WriteSetting,
        }
    }
}

/// The admission decision, as one total function over `Phase × Verb`.
///
/// Fifteen cases, all written out, so the policy reads as a table rather than
/// being inferred from control flow — and is testable without a device, a
/// store, a clock, or a task.
///
/// `Ok(phase)` is the phase the device is now *intended* to be in. A verb that
/// does not move the recording returns the phase it was given.
pub const fn admit(phase: Phase, verb: Verb) -> Result<Phase, Rejection> {
    match (phase, verb) {
        (Phase::Idle, Verb::Start) => Ok(Phase::Recording),
        (Phase::Idle, Verb::Stop) => Err(Rejection::NotRecording),
        (Phase::Idle, Verb::Pause) => Err(Rejection::NotRecording),
        (Phase::Idle, Verb::WriteMetadata) => Ok(Phase::Idle),
        (Phase::Idle, Verb::WriteSetting) => Ok(Phase::Idle),

        (Phase::Recording, Verb::Start) => Err(Rejection::AlreadyRecording),
        (Phase::Recording, Verb::Stop) => Ok(Phase::Idle),
        (Phase::Recording, Verb::Pause) => Ok(Phase::Paused),
        (Phase::Recording, Verb::WriteMetadata) => Err(Rejection::MetadataFrozen),
        (Phase::Recording, Verb::WriteSetting) => Ok(Phase::Recording),

        // A paused recording is still a recording: `RecordingState::is_recording`
        // in core returns true for `Paused`, and the metadata of the take in
        // progress is no more writable than it was a moment ago.
        (Phase::Paused, Verb::Start) => Ok(Phase::Recording),
        (Phase::Paused, Verb::Stop) => Ok(Phase::Idle),
        (Phase::Paused, Verb::Pause) => Err(Rejection::AlreadyPaused),
        (Phase::Paused, Verb::WriteMetadata) => Err(Rejection::MetadataFrozen),
        (Phase::Paused, Verb::WriteSetting) => Ok(Phase::Paused),
    }
}

/// Whether this transition begins a new recording, which is the event that
/// seals the previous recording's metadata.
///
/// `Paused -> Recording` is a *resume* and does not open a new take, so it does
/// not advance the counter.
pub const fn opens_recording(before: Phase, after: Phase) -> bool {
    matches!((before, after), (Phase::Idle, Phase::Recording))
}

/// Undo the phase change a verb caused, for a writing that failed terminally.
///
/// Conservative by construction: it only moves back along the three transitions
/// a failure can invalidate, and leaves every other phase alone. The point is
/// that a start which never reached the device must stop freezing metadata —
/// without this, one unreachable recorder makes its own metadata permanently
/// unwritable. Anything this gets wrong is corrected by the next [`reconcile`]
/// against a state the device actually reported.
///
/// Not the inverse of [`admit`], and cannot be: `admit` maps two phases onto
/// `Recording` (a start from idle and a resume from paused), so the phase alone
/// does not say which one to undo. Rolling `Recording` back to `Idle` for a
/// `Start` is right for the first and wrong for the second, and it is the safe
/// direction of wrong — it unfreezes metadata that a failed writing should not
/// have frozen, rather than freezing metadata nothing is recording.
pub const fn rollback(phase: Phase, verb: Verb) -> Phase {
    match (phase, verb) {
        (Phase::Recording, Verb::Start) => Phase::Idle,
        (Phase::Idle, Verb::Stop) => Phase::Recording,
        (Phase::Paused, Verb::Pause) => Phase::Recording,
        (other, _) => other,
    }
}

/// The epoch a writing admitted from `before` into `after` belongs to.
///
/// A metadata write admitted while idle is preparing the *next* recording, so
/// it is stamped `epoch + 1` — the same number the `Start` that follows it
/// will carry. That is what makes "these fields belong to that take" a
/// checkable statement rather than an assumption about ordering.
pub const fn epoch_of(before: Phase, after: Phase, epoch: u64) -> u64 {
    match (before, after) {
        (Phase::Idle, _) => epoch + 1,
        _ => epoch,
    }
}

#[test]
fn the_admission_policy_is_a_table() {
    use Phase::*;
    use Rejection::*;
    use Verb::*;

    assert_eq!(admit(Idle, Start), Ok(Recording));
    assert_eq!(admit(Idle, Stop), Err(NotRecording));
    assert_eq!(admit(Idle, Pause), Err(NotRecording));
    assert_eq!(admit(Idle, WriteMetadata), Ok(Idle));
    assert_eq!(admit(Idle, WriteSetting), Ok(Idle));
    assert_eq!(admit(Recording, Start), Err(AlreadyRecording));
    assert_eq!(admit(Recording, Stop), Ok(Idle));
    assert_eq!(admit(Recording, Pause), Ok(Paused));
    assert_eq!(admit(Recording, WriteMetadata), Err(MetadataFrozen));
    assert_eq!(admit(Recording, WriteSetting), Ok(Recording));
    assert_eq!(admit(Paused, Start), Ok(Recording));
    assert_eq!(admit(Paused, Stop), Ok(Idle));
    assert_eq!(admit(Paused, Pause), Err(AlreadyPaused));
    assert_eq!(admit(Paused, WriteMetadata), Err(MetadataFrozen));
    assert_eq!(admit(Paused, WriteSetting), Ok(Paused));
}

/// The requirement, stated once against the whole domain rather than per case.
#[test]
fn metadata_is_writable_exactly_when_no_recording_is_in_progress() {
    for phase in [Phase::Idle, Phase::Recording, Phase::Paused] {
        assert_eq!(
            admit(phase, Verb::WriteMetadata).is_ok(),
            phase == Phase::Idle,
            "metadata admissibility disagreed with the phase at {phase:?}"
        );
    }
}

/// The failure this exists to prevent: a start that never reached the device
/// leaving that device's metadata frozen for the life of the process.
#[test]
fn a_failed_start_stops_freezing_metadata() {
    let after_failure = rollback(Phase::Recording, Verb::Start);
    assert_eq!(after_failure, Phase::Idle);
    assert_eq!(admit(after_failure, Verb::WriteMetadata), Ok(Phase::Idle));
}

/// A write never moved the phase, so a failed one has nothing to undo. Stated
/// because the tempting simplification — roll back whenever anything fails —
/// would unfreeze a live recording on a failed metadata write.
#[test]
fn rolling_back_a_verb_that_moves_nothing_is_the_identity() {
    for phase in [Phase::Idle, Phase::Recording, Phase::Paused] {
        for verb in [Verb::WriteMetadata, Verb::WriteSetting] {
            assert_eq!(rollback(phase, verb), phase);
        }
    }
}

/// `rollback` undoes only the transition the verb could have caused. A stop
/// that fails while the phase is already `Paused` was not the thing that put it
/// there, so it is left alone for [`reconcile`] to settle.
#[test]
fn rollback_leaves_a_phase_the_verb_did_not_produce() {
    assert_eq!(rollback(Phase::Paused, Verb::Stop), Phase::Paused);
    assert_eq!(rollback(Phase::Idle, Verb::Start), Phase::Idle);
    assert_eq!(rollback(Phase::Recording, Verb::Pause), Phase::Recording);
}

/// The device is the authority on every disagreement but one.
#[test]
fn reconcile_lets_the_device_win_except_when_it_says_nothing() {
    for phase in [Phase::Idle, Phase::Recording, Phase::Paused] {
        assert_eq!(reconcile(phase, RecordingState::Stopped), Phase::Idle);
        assert_eq!(reconcile(phase, RecordingState::Started), Phase::Recording);
        assert_eq!(reconcile(phase, RecordingState::Paused), Phase::Paused);
        // A garbled reply is not evidence of anything. Reading `Unknown` as
        // `Stopped` would unfreeze metadata mid-recording, which is the one
        // outcome the freeze exists to prevent.
        assert_eq!(reconcile(phase, RecordingState::Unknown), phase);
    }
}

/// Only a take that *begins* seals the previous one's metadata, so a resume
/// must not advance the counter — otherwise pausing and resuming would strand
/// the metadata written for the take still running.
#[test]
fn the_epoch_advances_on_a_start_and_not_on_a_resume() {
    assert!(opens_recording(Phase::Idle, Phase::Recording));
    assert!(!opens_recording(Phase::Paused, Phase::Recording));

    // A metadata write admitted while idle is stamped with the epoch of the
    // recording it is preparing — the same one the start that follows carries.
    assert_eq!(epoch_of(Phase::Idle, Phase::Idle, 4), 5);
    assert_eq!(epoch_of(Phase::Idle, Phase::Recording, 4), 5);
    // Nothing admitted during a take can belong to a different one.
    assert_eq!(epoch_of(Phase::Recording, Phase::Recording, 5), 5);
    assert_eq!(epoch_of(Phase::Paused, Phase::Recording, 5), 5);
}
