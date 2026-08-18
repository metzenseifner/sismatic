//! An in-memory outbox: the [`CommandSubmit`] / [`CommandLog`] /
//! [`CommandDrain`] trio the write side runs on today.
//!
//! # Shape
//!
//! ```text
//! logs:    device -> DeviceLog { phase, epoch, queue, history, keys }
//! records: command_id -> CommandRecord
//! ```
//!
//! Two maps rather than one because the two lookups are different questions.
//! `GET /v1/commands/{id}` names a command and nothing else, so it must not
//! have to scan a fleet to find which device's log holds it; the admission
//! decision names a device and nothing else, so it must not have to scan
//! commands. Each map answers its own question in one lookup.
//!
//! # The critical section
//!
//! Locking follows the device, as it already does for readings:
//!
//! > Locking follows the outer key, so writers contend per *device*, not per
//! > `(device, field)`.
//!
//! For readings that is an optimisation. Here it is the correctness mechanism.
//! The port says the admission decision and the append are one unit, and a
//! `DashMap` entry guard held across both is what makes that true: two requests
//! that both read `phase = Idle` and both pass would let a metadata write be
//! dispatched into a recording that a concurrent start had already begun.
//! `submit` therefore holds one guard from the check to the last mutation, and
//! `concurrent_starts_admit_exactly_one` below is the test that fails if that
//! is ever split.
//!
//! # Lock order
//!
//! Every method that touches both maps takes `logs` **before** `records`, and
//! never the reverse. Two `DashMap`s are two independent sets of shard locks,
//! so a method that took them the other way round could deadlock against
//! `submit` under concurrency. [`MemoryOutbox::settle`] is the one place this
//! is not free: it is handed a command id and has to learn the device before it
//! can take the right guard, which it does with a separate short-lived read.
//!
//! # What it is not
//!
//! `records` and each log's `history` grow without bound, for the same reason
//! `MemoryStore::history` does and with the same consequence: fine for a test
//! and a development server, not the deployment story. The outbox adds a second
//! reason to want a durable adapter — a pending command is lost on restart, so
//! the delivery guarantee holds only for a process that stays up. Neither limit
//! is part of the port, so a SQL adapter is a drop-in.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use dashmap::DashMap;
use sismatic_api_types::{
    Accepted, CommandId, CommandRecord, CommandStatus, DeviceId, Phase, RecordingPhase,
    RecordingState, Timestamp,
};
use sismatic_store::outbox::{
    CommandDrain, CommandLog, CommandSubmit, Outcome, Submission, SubmitError, Verb, admit,
    epoch_of, opens_recording, reconcile, rollback,
};
use sismatic_store::{ReadError, WriteError};

/// One device's write-side state and queue, held under one entry so the five
/// fields cannot be read or updated out of step.
///
/// Invariant: every `CommandId` in `queue` or `history` names a record already
/// present in [`MemoryOutbox::records`]. Insert the record first, push the id
/// second — the two maps are separate, so a reader walking a queue mid-append
/// would otherwise meet an id with nothing behind it.
#[derive(Debug)]
struct DeviceLog {
    phase: Phase,
    epoch: u64,
    /// Ids awaiting dispatch, oldest first.
    queue: VecDeque<CommandId>,
    /// Every id ever admitted for this device, in submission order. Kept
    /// separately from `queue` because `queue` is consumed by the relay, and
    /// "what has this device been asked to do" outlives "what is still owed".
    /// An explicit order is also what lets `commands_for` promise newest-first
    /// without sorting on a timestamp that two submissions can share.
    history: Vec<CommandId>,
    /// Idempotency key -> the command it first produced.
    keys: BTreeMap<String, CommandId>,
}

/// An unknown device is idle at epoch 0 — the same reasoning `ReadStore::latest`
/// gives for answering `None`: this port holds what was submitted and no
/// catalog of what exists, so it cannot tell an unknown device from one that
/// has been asked to do nothing yet.
///
/// Hand-written rather than derived because [`Phase`] has no `Default` and
/// should not gain one: it is a wire type, and a default phase is a claim about
/// a device that only this adapter is in a position to make.
impl Default for DeviceLog {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            epoch: 0,
            queue: VecDeque::new(),
            history: Vec::new(),
            keys: BTreeMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct MemoryOutbox {
    logs: Arc<DashMap<DeviceId, DeviceLog>>,
    records: Arc<DashMap<CommandId, CommandRecord>>,
    max_attempts: u32,
    backoff: Duration,
}

impl MemoryOutbox {
    /// The default delay before a failed command is retried, multiplied by the
    /// number of attempts already spent. Chosen against the same clock the
    /// relay polls on: shorter than this and a device that is down for a second
    /// burns the whole retry budget before it can answer.
    pub const DEFAULT_BACKOFF: Duration = Duration::from_millis(500);

    /// `max_attempts` counts total tries, not retries: `1` means no retry.
    ///
    /// There is no `Default` impl on purpose. A derived one would give
    /// `max_attempts = 0`, which makes the retry arm of [`settle`](Self::settle)
    /// unreachable and every transient SSH failure terminal on the first try —
    /// a silent, plausible-looking wrong answer. A `0` passed here is read as
    /// `1`, since "try it zero times" is not a thing a caller can mean.
    pub fn with_max_attempts(max_attempts: u32) -> Self {
        Self {
            logs: Arc::default(),
            records: Arc::default(),
            max_attempts: max_attempts.max(1),
            backoff: Self::DEFAULT_BACKOFF,
        }
    }

    /// Override the retry delay. Separate from the constructor because the
    /// backoff is a tuning knob and `max_attempts` is a policy — and because a
    /// test wants `Duration::ZERO` here without that being a spelling any
    /// deployment reaches for.
    #[must_use]
    pub fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// The device a command belongs to, read and released before any log guard
    /// is taken. Exists to keep the `logs`-before-`records` order in `settle`,
    /// which is handed an id and has to learn the device from it.
    fn device_of(&self, id: &CommandId) -> Option<DeviceId> {
        self.records.get(id).map(|r| r.device.clone())
    }
}

/// `at` plus `delay`, or `at` unchanged if it is not a timestamp this can
/// parse.
///
/// Degrading to "retry immediately" rather than propagating a parse failure:
/// the timestamp comes from the relay, an unparseable one is a bug in the
/// caller and not a reason to lose a command, and the fallback is the behaviour
/// the field was added to improve on rather than a new failure mode.
fn delay_from(at: &Timestamp, delay: Duration) -> Timestamp {
    let Ok(parsed) = DateTime::parse_from_rfc3339(at.as_str()) else {
        return at.clone();
    };
    let Ok(delay) = chrono::Duration::from_std(delay) else {
        return at.clone();
    };
    Timestamp((parsed.with_timezone(&Utc) + delay).to_rfc3339_opts(SecondsFormat::Millis, true))
}

#[async_trait::async_trait]
impl CommandSubmit for MemoryOutbox {
    async fn submit(&self, s: Submission) -> Result<Accepted, SubmitError> {
        // The whole critical section. This guard is an exclusive hold on the
        // device's shard for as long as `log` is alive, so no concurrent
        // submission for the same device can observe the phase this one is
        // about to change. Devices contend on nothing.
        let mut log = self.logs.entry(s.device.clone()).or_default();

        // Idempotent Receiver: a client whose POST timed out and was retried
        // gets the original command back, rather than a second start that the
        // admission table would then refuse with a 409 for something the client
        // believes never landed.
        if let Some(key) = &s.idempotency_key
            && let Some(existing) = log.keys.get(key)
            && let Some(record) = self.records.get(existing)
        {
            return Ok(Accepted {
                id: record.id.clone(),
                epoch: record.epoch,
            });
        }

        let before = log.phase;
        let after =
            admit(before, Verb::of(&s.intent)).map_err(|rejection| SubmitError::Rejected {
                rejection,
                phase: before,
            })?;
        let epoch = epoch_of(before, after, log.epoch);

        let record = CommandRecord {
            id: s.id.clone(),
            device: s.device.clone(),
            intent: s.intent,
            epoch,
            status: CommandStatus::Pending,
            attempts: 0,
            enqueued_at: s.at.clone(),
            updated_at: s.at.clone(),
            // Due immediately: the backoff exists to space out *retries*, and a
            // first attempt has nothing to back off from.
            not_before: s.at,
        };

        // Record before queue, so a reader walking the queue never meets an id
        // with no record behind it.
        self.records.insert(s.id.clone(), record);
        log.queue.push_back(s.id.clone());
        log.history.push(s.id.clone());

        log.phase = after;
        if opens_recording(before, after) {
            log.epoch = epoch;
        }
        if let Some(key) = s.idempotency_key {
            log.keys.insert(key, s.id.clone());
        }

        Ok(Accepted { id: s.id, epoch })
    }
}

#[async_trait::async_trait]
impl CommandLog for MemoryOutbox {
    async fn command(&self, id: CommandId) -> Result<Option<CommandRecord>, ReadError> {
        Ok(self.records.get(&id).map(|r| r.clone()))
    }

    async fn commands_for(&self, device: DeviceId) -> Result<Vec<CommandRecord>, ReadError> {
        let Some(log) = self.logs.get(&device) else {
            return Ok(Vec::new());
        };
        Ok(log
            .history
            .iter()
            .rev()
            .filter_map(|id| self.records.get(id).map(|r| r.clone()))
            .collect())
    }

    async fn phase(&self, device: DeviceId) -> Result<RecordingPhase, ReadError> {
        let (phase, epoch) = self
            .logs
            .get(&device)
            .map_or((Phase::Idle, 0), |log| (log.phase, log.epoch));
        Ok(RecordingPhase { phase, epoch })
    }
}

#[async_trait::async_trait]
impl CommandDrain for MemoryOutbox {
    async fn claim_next(
        &self,
        device: DeviceId,
        at: Timestamp,
    ) -> Result<Option<CommandRecord>, WriteError> {
        let mut log = self.logs.entry(device).or_default();
        let Some(id) = log.queue.front().cloned() else {
            return Ok(None);
        };

        let mut record = self
            .records
            .get_mut(&id)
            .ok_or_else(|| WriteError::backend("a queued id has no record behind it"))?;

        // A command still serving its backoff blocks the queue rather than
        // being skipped over. Skipping would let a later command overtake the
        // retry of an earlier one, which is exactly the reordering that puts a
        // metadata write after the start it was meant to precede.
        if at.as_str() < record.not_before.as_str() {
            return Ok(None);
        }

        log.queue.pop_front();
        record.status = CommandStatus::InFlight;
        record.attempts += 1;
        record.updated_at = at;
        Ok(Some(record.clone()))
    }

    async fn settle(
        &self,
        id: CommandId,
        outcome: Outcome,
        at: Timestamp,
    ) -> Result<(), WriteError> {
        // Read the device and release, so the guards below are taken in the
        // `logs`-then-`records` order every other method uses.
        let device = self
            .device_of(&id)
            .ok_or_else(|| WriteError::backend("settling an unknown command"))?;
        let mut log = self.logs.entry(device).or_default();
        let mut record = self
            .records
            .get_mut(&id)
            .ok_or_else(|| WriteError::backend("settling an unknown command"))?;

        if record.status != CommandStatus::InFlight {
            return Err(WriteError::backend(
                "settling a command that was not in flight",
            ));
        }

        match outcome {
            Outcome::Succeeded(value) => {
                record.status = CommandStatus::Succeeded { value };
            }
            Outcome::Failed(_) if record.attempts < self.max_attempts => {
                record.status = CommandStatus::Pending;
                // Back onto the *front* of the queue: a retry must not be
                // overtaken by commands submitted after it.
                log.queue.push_front(id.clone());
                // Spaced out proportionally to what has already been spent, so
                // three attempts against a device that is down do not all land
                // inside one poll interval and exhaust the budget in a moment.
                record.not_before = delay_from(&at, self.backoff * record.attempts);
            }
            Outcome::Failed(reason) => {
                record.status = CommandStatus::Failed { reason };
                // Dead Letter Channel. The phase this command optimistically
                // moved is rolled back, so a start that never reached the
                // device stops freezing this device's metadata.
                log.phase = rollback(log.phase, Verb::of(&record.intent));
            }
        }
        record.updated_at = at;
        Ok(())
    }

    async fn observe(&self, device: DeviceId, observed: RecordingState) -> Result<(), WriteError> {
        let mut log = self.logs.entry(device).or_default();
        let before = log.phase;
        let after = reconcile(before, observed);
        // A recording started from the front panel opens a take this process
        // never admitted, and the metadata of the previous one is sealed by it
        // just the same.
        if opens_recording(before, after) {
            log.epoch += 1;
        }
        log.phase = after;
        Ok(())
    }

    async fn in_flight(&self, device: DeviceId) -> Result<Vec<CommandRecord>, WriteError> {
        let Some(log) = self.logs.get(&device) else {
            return Ok(Vec::new());
        };
        Ok(log
            .history
            .iter()
            .filter_map(|id| self.records.get(id).map(|r| r.clone()))
            .filter(|r| r.status == CommandStatus::InFlight)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{Intent, ReadingValue, Rejection};

    const T0: &str = "2026-08-17T00:00:00.000Z";

    fn at(s: &str) -> Timestamp {
        Timestamp(s.to_owned())
    }

    fn outbox() -> MemoryOutbox {
        // Zero backoff by default: the tests that are not about retry timing
        // should not have to wait for it, and the one that is sets its own.
        MemoryOutbox::with_max_attempts(3).with_backoff(Duration::ZERO)
    }

    fn title(value: &str) -> Intent {
        Intent::SetMetadata {
            field: "TITLE".to_owned(),
            value: value.to_owned(),
        }
    }

    async fn submit(
        outbox: &MemoryOutbox,
        id: &str,
        intent: Intent,
    ) -> Result<Accepted, SubmitError> {
        outbox
            .submit(Submission {
                id: id.to_owned(),
                device: "sim".to_owned(),
                intent,
                at: at(T0),
                idempotency_key: None,
            })
            .await
    }

    /// Claim, then settle with `outcome`. The pair the relay always performs
    /// together.
    async fn dispatch(outbox: &MemoryOutbox, outcome: Outcome) -> Option<CommandId> {
        let record = outbox
            .claim_next("sim".to_owned(), at(T0))
            .await
            .expect("claim")?;
        outbox
            .settle(record.id.clone(), outcome, at(T0))
            .await
            .expect("settle");
        Some(record.id)
    }

    async fn phase_of(outbox: &MemoryOutbox) -> RecordingPhase {
        outbox.phase("sim".to_owned()).await.expect("phase")
    }

    // ---- admission and the freeze ----------------------------------------

    /// The requirement, as one run through the port.
    #[tokio::test]
    async fn metadata_is_frozen_from_the_moment_a_start_is_accepted() {
        let outbox = outbox();

        assert!(submit(&outbox, "a", title("before")).await.is_ok());
        assert!(submit(&outbox, "b", Intent::StartRecording).await.is_ok());
        assert_eq!(
            submit(&outbox, "c", title("during")).await.unwrap_err(),
            SubmitError::Rejected {
                rejection: Rejection::MetadataFrozen,
                phase: Phase::Recording,
            }
        );
        assert!(submit(&outbox, "d", Intent::StopRecording).await.is_ok());
        assert!(submit(&outbox, "e", title("after")).await.is_ok());
    }

    /// The freeze applies to metadata and not to settings, which is the whole
    /// reason the two are separate `Intent` variants.
    #[tokio::test]
    async fn a_setting_is_writable_during_a_recording() {
        let outbox = outbox();
        submit(&outbox, "a", Intent::StartRecording).await.unwrap();

        assert!(
            submit(
                &outbox,
                "b",
                Intent::SetSetting {
                    field: "TIMEZONE".to_owned(),
                    value: "UTC".to_owned(),
                }
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn an_unknown_device_is_idle_at_epoch_zero() {
        let outbox = outbox();
        assert_eq!(
            outbox.phase("never-heard-of".to_owned()).await.unwrap(),
            RecordingPhase {
                phase: Phase::Idle,
                epoch: 0
            }
        );
    }

    // ---- the epoch --------------------------------------------------------

    /// The worked example from the design note, as one sequence: the writes
    /// that prepare a take carry the epoch of the take that then starts, and a
    /// write after it stops cannot be confused with them.
    #[tokio::test]
    async fn metadata_prepared_for_a_take_shares_that_takes_epoch() {
        let outbox = outbox();

        let epoch_of = |a: Accepted| a.epoch;
        let first_title = submit(&outbox, "a", title("Week 4")).await.map(epoch_of);
        let presenter = submit(
            &outbox,
            "b",
            Intent::SetMetadata {
                field: "PRESENTER".to_owned(),
                value: "Komar".to_owned(),
            },
        )
        .await
        .map(epoch_of);
        let start = submit(&outbox, "c", Intent::StartRecording)
            .await
            .map(epoch_of);

        assert_eq!((first_title, presenter, start), (Ok(1), Ok(1), Ok(1)));

        submit(&outbox, "d", Intent::StopRecording).await.unwrap();
        let next_title = submit(&outbox, "e", title("Week 5")).await.map(epoch_of);
        assert_eq!(next_title, Ok(2), "a new take must get a new epoch");
    }

    /// A resume is not a new take, so the metadata already sealed by the first
    /// start stays sealed to it rather than being re-stamped.
    #[tokio::test]
    async fn pausing_and_resuming_stays_in_one_epoch() {
        let outbox = outbox();
        submit(&outbox, "a", Intent::StartRecording).await.unwrap();
        submit(&outbox, "b", Intent::PauseRecording).await.unwrap();
        let resumed = submit(&outbox, "c", Intent::StartRecording).await.unwrap();

        assert_eq!(resumed.epoch, 1);
        assert_eq!(phase_of(&outbox).await.epoch, 1);
    }

    // ---- atomicity --------------------------------------------------------

    /// The race the atomicity contract exists for, and the one test that fails
    /// if the critical section in `submit` is ever split. Fifty concurrent
    /// starts against one idle device: exactly one may be admitted, because the
    /// first to take the device's entry moves the phase before any other can
    /// read it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_starts_admit_exactly_one() {
        let outbox = outbox();
        let mut tasks = tokio::task::JoinSet::new();
        for n in 0..50 {
            let outbox = outbox.clone();
            tasks.spawn(async move {
                submit(&outbox, &format!("cmd-{n}"), Intent::StartRecording).await
            });
        }

        let mut admitted = 0;
        while let Some(result) = tasks.join_next().await {
            if result.expect("no task panics").is_ok() {
                admitted += 1;
            }
        }
        assert_eq!(
            admitted, 1,
            "the phase guard must serialise competing starts"
        );
    }

    /// The same guard from the other side: a metadata write and a start racing
    /// must not both be admitted, because that is the interleaving that puts a
    /// title on a recording that had already begun.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn a_metadata_write_racing_a_start_cannot_both_win_twice_over() {
        for round in 0..200 {
            let outbox = outbox();
            let a = outbox.clone();
            let b = outbox.clone();
            let write = tokio::spawn(async move { submit(&a, "w", title("racy")).await });
            let start = tokio::spawn(async move { submit(&b, "s", Intent::StartRecording).await });
            let (write, start) = (write.await.unwrap(), start.await.unwrap());

            // Both orders are legitimate; what is not is a write admitted
            // against a phase the start had already moved.
            match (write, start) {
                (Ok(write), Ok(start)) => assert_eq!(
                    write.epoch, start.epoch,
                    "round {round}: a write admitted alongside a start must belong to it"
                ),
                // The start is unopposed — nothing in the table refuses one
                // against an idle device — so the write is the only submission
                // that can lose, and it must lose for the stated reason.
                (Err(refused), Ok(_)) => assert_eq!(
                    refused,
                    SubmitError::Rejected {
                        rejection: Rejection::MetadataFrozen,
                        phase: Phase::Recording,
                    },
                    "round {round}: unexpected grounds for refusing the write"
                ),
                (write, start) => {
                    panic!("round {round}: the start cannot be refused, got {write:?} / {start:?}")
                }
            }
        }
    }

    // ---- claim, settle, order --------------------------------------------

    #[tokio::test]
    async fn claiming_follows_submission_order() {
        let outbox = outbox();
        submit(&outbox, "first", title("a")).await.unwrap();
        submit(&outbox, "second", title("b")).await.unwrap();
        submit(&outbox, "third", Intent::StartRecording)
            .await
            .unwrap();

        let mut claimed = Vec::new();
        while let Some(id) = dispatch(&outbox, Outcome::Succeeded(ReadingValue::Flag(true))).await {
            claimed.push(id);
        }
        assert_eq!(claimed, ["first", "second", "third"]);
    }

    #[tokio::test]
    async fn an_empty_queue_claims_nothing() {
        let outbox = outbox();
        assert_eq!(
            outbox.claim_next("sim".to_owned(), at(T0)).await.unwrap(),
            None
        );
    }

    /// The ordering property a retry could break. A failed command goes back to
    /// the front, so it is re-dispatched before the command submitted after it
    /// — otherwise a re-tried metadata write could land after the start it was
    /// meant to precede.
    #[tokio::test]
    async fn a_retry_is_re_dispatched_before_later_commands() {
        let outbox = outbox();
        submit(&outbox, "write", title("a")).await.unwrap();
        submit(&outbox, "start", Intent::StartRecording)
            .await
            .unwrap();

        let failed = dispatch(&outbox, Outcome::Failed("ssh died".into())).await;
        assert_eq!(failed.as_deref(), Some("write"));

        let retried = dispatch(&outbox, Outcome::Succeeded(ReadingValue::Flag(true))).await;
        assert_eq!(
            retried.as_deref(),
            Some("write"),
            "the retry must not be overtaken by the start behind it"
        );
    }

    #[tokio::test]
    async fn attempts_are_counted_and_exhausted() {
        let outbox = outbox();
        submit(&outbox, "cmd", title("a")).await.unwrap();

        for _ in 0..3 {
            dispatch(&outbox, Outcome::Failed("down".into())).await;
        }
        // The fourth claim finds nothing: three attempts is the budget.
        assert_eq!(
            outbox.claim_next("sim".to_owned(), at(T0)).await.unwrap(),
            None
        );

        let record = outbox.command("cmd".to_owned()).await.unwrap().unwrap();
        assert_eq!(record.attempts, 3);
        assert_eq!(
            record.status,
            CommandStatus::Failed {
                reason: "down".to_owned()
            }
        );
    }

    /// A command still serving its backoff is not claimable yet, and — the part
    /// that matters — it blocks the queue behind it rather than being skipped.
    #[tokio::test]
    async fn a_backing_off_command_holds_the_queue() {
        let outbox = MemoryOutbox::with_max_attempts(3).with_backoff(Duration::from_secs(60));
        submit(&outbox, "write", title("a")).await.unwrap();
        submit(&outbox, "start", Intent::StartRecording)
            .await
            .unwrap();

        dispatch(&outbox, Outcome::Failed("down".into())).await;

        // One second later the retry is not yet due, and the start must not
        // overtake it.
        assert_eq!(
            outbox
                .claim_next("sim".to_owned(), at("2026-08-17T00:00:01.000Z"))
                .await
                .unwrap(),
            None
        );
        // A minute later it is.
        let due = outbox
            .claim_next("sim".to_owned(), at("2026-08-17T00:01:00.000Z"))
            .await
            .unwrap();
        assert_eq!(due.map(|r| r.id).as_deref(), Some("write"));
    }

    #[tokio::test]
    async fn settling_a_command_twice_is_refused() {
        let outbox = outbox();
        submit(&outbox, "cmd", title("a")).await.unwrap();
        dispatch(&outbox, Outcome::Succeeded(ReadingValue::Flag(true))).await;

        assert_eq!(
            outbox
                .settle("cmd".to_owned(), Outcome::Failed("late".into()), at(T0))
                .await,
            Err(WriteError::backend(
                "settling a command that was not in flight"
            ))
        );
    }

    #[tokio::test]
    async fn settling_an_unknown_command_is_refused() {
        let outbox = outbox();
        assert_eq!(
            outbox
                .settle("nope".to_owned(), Outcome::Failed("x".into()), at(T0))
                .await,
            Err(WriteError::backend("settling an unknown command"))
        );
    }

    /// The failure the rollback exists for: one unreachable recorder must not
    /// leave its own metadata permanently unwritable.
    #[tokio::test]
    async fn a_start_that_never_reached_the_device_unfreezes_metadata() {
        let outbox = MemoryOutbox::with_max_attempts(1).with_backoff(Duration::ZERO);
        submit(&outbox, "start", Intent::StartRecording)
            .await
            .unwrap();
        assert_eq!(phase_of(&outbox).await.phase, Phase::Recording);

        dispatch(&outbox, Outcome::Failed("device unreachable".into())).await;

        assert_eq!(phase_of(&outbox).await.phase, Phase::Idle);
        assert!(submit(&outbox, "title", title("a")).await.is_ok());
    }

    // ---- reconciliation ---------------------------------------------------

    /// A recording started from the front panel becomes visible to the
    /// admission table, which is the only thing that closes the gap between
    /// what this process asked for and what the device is doing.
    #[tokio::test]
    async fn an_observed_recording_freezes_metadata_this_process_never_started() {
        let outbox = outbox();
        assert!(submit(&outbox, "before", title("a")).await.is_ok());

        outbox
            .observe("sim".to_owned(), RecordingState::Started)
            .await
            .unwrap();

        assert_eq!(
            submit(&outbox, "after", title("b")).await.unwrap_err(),
            SubmitError::Rejected {
                rejection: Rejection::MetadataFrozen,
                phase: Phase::Recording,
            }
        );
        // The take the device opened is a take, so it gets an epoch.
        assert_eq!(phase_of(&outbox).await.epoch, 1);
    }

    #[tokio::test]
    async fn an_unknown_observed_state_changes_nothing() {
        let outbox = outbox();
        submit(&outbox, "start", Intent::StartRecording)
            .await
            .unwrap();

        outbox
            .observe("sim".to_owned(), RecordingState::Unknown)
            .await
            .unwrap();

        assert_eq!(phase_of(&outbox).await.phase, Phase::Recording);
    }

    // ---- idempotency ------------------------------------------------------

    #[tokio::test]
    async fn a_repeated_key_returns_the_original_command() {
        let outbox = outbox();
        let submission = |id: &str| Submission {
            id: id.to_owned(),
            device: "sim".to_owned(),
            intent: Intent::StartRecording,
            at: at(T0),
            idempotency_key: Some("take-4".to_owned()),
        };

        let first = outbox.submit(submission("one")).await.unwrap();
        // Without deduplication this second start is refused with
        // `AlreadyRecording` — a 409 for something the client believes never
        // landed.
        let second = outbox.submit(submission("two")).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(
            outbox.commands_for("sim".to_owned()).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn keys_are_scoped_to_one_device() {
        let outbox = outbox();
        for device in ["sim-a", "sim-b"] {
            outbox
                .submit(Submission {
                    id: format!("cmd-{device}"),
                    device: device.to_owned(),
                    intent: Intent::StartRecording,
                    at: at(T0),
                    idempotency_key: Some("take-4".to_owned()),
                })
                .await
                .expect("one key means one command per device, not per fleet");
        }

        for device in ["sim-a", "sim-b"] {
            assert_eq!(
                outbox.commands_for(device.to_owned()).await.unwrap().len(),
                1
            );
        }
    }

    // ---- the log ----------------------------------------------------------

    #[tokio::test]
    async fn commands_are_listed_newest_first_and_scoped_to_one_device() {
        let outbox = outbox();
        submit(&outbox, "first", title("a")).await.unwrap();
        submit(&outbox, "second", title("b")).await.unwrap();
        outbox
            .submit(Submission {
                id: "elsewhere".to_owned(),
                device: "other".to_owned(),
                intent: title("c"),
                at: at(T0),
                idempotency_key: None,
            })
            .await
            .unwrap();

        let ids: Vec<_> = outbox
            .commands_for("sim".to_owned())
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        // Newest first, and stated by insertion order rather than by a
        // timestamp — all three share one, which is exactly the case a sort on
        // `enqueued_at` would get wrong.
        assert_eq!(ids, ["second", "first"]);
    }

    #[tokio::test]
    async fn an_unknown_command_is_absence_not_an_error() {
        let outbox = outbox();
        assert_eq!(outbox.command("nope".to_owned()).await.unwrap(), None);
        assert_eq!(
            outbox.commands_for("nobody".to_owned()).await.unwrap(),
            Vec::new()
        );
    }

    #[tokio::test]
    async fn only_claimed_commands_are_in_flight() {
        let outbox = outbox();
        submit(&outbox, "claimed", title("a")).await.unwrap();
        submit(&outbox, "queued", title("b")).await.unwrap();

        outbox.claim_next("sim".to_owned(), at(T0)).await.unwrap();

        let stranded = outbox.in_flight("sim".to_owned()).await.unwrap();
        assert_eq!(
            stranded.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["claimed"]
        );
    }

    #[tokio::test]
    async fn clone_shares_the_same_backing_outbox() {
        // The composition root hands the same value out under three ports, so a
        // submission through one must be claimable through another.
        let outbox = outbox();
        let handle = outbox.clone();
        submit(&outbox, "cmd", title("a")).await.unwrap();

        let claimed = handle.claim_next("sim".to_owned(), at(T0)).await.unwrap();
        assert_eq!(claimed.map(|r| r.id).as_deref(), Some("cmd"));
    }
}
