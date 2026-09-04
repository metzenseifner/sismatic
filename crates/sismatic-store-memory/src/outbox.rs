//! An in-memory outbox: the [`WriteSubmit`] / [`WriteLog`] /
//! [`WriteDrain`] trio the write side runs on today, plus the [`GroupState`]
//! read the group routes answer from.
//!
//! # Shape
//!
//! ```text
//! logs:    device -> DeviceLog { desired_recording_state, epoch, queue, history, keys }
//! batches: batch_id -> Batch { members, ready, armed_at, barrier }
//! groups:  group -> field -> GroupExpectation
//! records: write_id -> WriteRecord
//! ```
//!
//! `groups` is here rather than beside the reads because it holds what a
//! device group was *told*, which is the same kind of belief a `DeviceLog`'s
//! desired recording state is, and is written by the same call under the same
//! lock — see [`sismatic_store::group`].
//!
//! `records` is separate from `logs` because the two lookups are different
//! questions. `GET /v1/writes/{id}` names a write and nothing else, so it
//! must not have to scan a fleet to find which device's log holds it; the
//! admission decision names devices and nothing else, so it must not have to
//! scan writes.
//!
//! # One lock, not a sharded map
//!
//! `logs` and `batches` live under one `Mutex`, and this is the load-bearing
//! choice in the file.
//!
//! Atomic admission across a group means holding every member's state at once,
//! and `DashMap` cannot promise that. Two member ids may hash into the same
//! shard, and a shard's `RwLock` is not reentrant, so a group submission that
//! took one entry guard per member would deadlock against itself on exactly the
//! id pairs that collide — a bug no small test fixture reproduces, because
//! whether it fires depends on the hash of the operator's device names.
//! Acquiring in sorted order does not help: the collision is one lock taken
//! twice, not two locks taken out of order.
//!
//! The cost is that submissions to unrelated devices now serialise. That is
//! affordable because submissions arrive at *operator* rate — a handful per
//! minute, one per button press — while the work each one does under the lock
//! is a few map inserts and no I/O. Sharding buys throughput this workload has
//! no use for and charges an unreproducible deadlock for it.
//!
//! `records` stays a `DashMap`: it is read by the API on every status poll and
//! written outside the admission critical section, so it is the one map where
//! contention is real and where nothing needs to be atomic with anything else.
//!
//! # Lock order
//!
//! `state` (the mutex) is always taken before `records`, never the reverse.
//! [`MemoryOutbox::settle`] is the one place that is not free: it is handed a
//! write id and has to learn the device before it can take the right guard,
//! which it does with a separate short-lived `records` read that is released
//! first.
//!
//! # What it is not
//!
//! `records`, each log's `history`, and settled `batches` grow without bound,
//! for the same reason `MemoryStore::history` does and with the same
//! consequence: fine for a test and a development server, not the deployment
//! story. The outbox adds a second reason to want a durable adapter — a pending
//! write is lost on restart, so the delivery guarantee holds only for a
//! process that stays up. Neither limit is part of the port.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use dashmap::DashMap;
use sismatic_api_types::{
    Acceptance, Accepted, Barrier, BatchId, DesiredRecordingState, DeviceDesiredRecordingState,
    DeviceId, FieldName, GroupExpectation, GroupId, RecordingState, Timestamp, WriteId,
    WriteRecord, WriteStatus,
};
use sismatic_store::group::{GroupState, expects};
use sismatic_store::outbox::{
    BarrierPolicy, Claim, Outcome, Submission, SubmitError, Verb, WriteDrain, WriteLog,
    WriteSubmit, admit, epoch_of, opens_recording, reconcile, rollback,
};
use sismatic_store::{ReadError, WriteError};

/// One device's write-side state and queue.
///
/// Invariant: every `WriteId` in `queue` or `history` names a record already
/// present in [`MemoryOutbox::records`]. Insert the record first, push the id
/// second — the two live under different locks, so a reader walking a queue
/// mid-append would otherwise meet an id with nothing behind it.
#[derive(Debug)]
struct DeviceLog {
    desired_recording_state: DesiredRecordingState,
    epoch: u64,
    /// Ids awaiting dispatch, oldest first.
    queue: VecDeque<WriteId>,
    /// Every id ever admitted for this device, in submission order. Kept
    /// separately from `queue` because `queue` is consumed by the relay, and
    /// "what has this device been asked to do" outlives "what is still owed".
    /// An explicit order is also what lets `writes_for` promise newest-first
    /// without sorting on a timestamp that two submissions can share.
    history: Vec<WriteId>,
    /// Idempotency key -> the write it first produced.
    keys: BTreeMap<String, WriteId>,
}

/// An unknown device is idle at epoch 0 — the same reasoning `ReadStore::latest`
/// gives for answering `None`: this port holds what was submitted and no
/// catalog of what exists, so it cannot tell an unknown device from one that
/// has been asked to do nothing yet.
///
/// Hand-written rather than derived because [`DesiredRecordingState`] has no
/// `Default` and should not gain one: it is a wire type, and a default state is
/// a claim about a device that only this adapter is in a position to make.
impl Default for DeviceLog {
    fn default() -> Self {
        Self {
            desired_recording_state: DesiredRecordingState::Idle,
            epoch: 0,
            queue: VecDeque::new(),
            history: Vec::new(),
            keys: BTreeMap::new(),
        }
    }
}

/// A batch's readiness, held alongside the queues so the barrier can be
/// evaluated inside the same critical section that claims a row.
#[derive(Debug, Clone)]
struct Batch {
    /// Every member the batch was expanded across.
    members: BTreeSet<DeviceId>,
    /// Members whose row has reached the head of their own queue and whose
    /// device is otherwise claimable.
    ///
    /// Recomputed from the live queues on every tick rather than accumulated as
    /// members announce themselves. A member that reached the head and was then
    /// overtaken — by the `push_front` a retry of the write *ahead* of it
    /// performs — is no longer ready, and an accumulated set would not notice:
    /// the batch would release and claim whatever now sat at that member's
    /// head, which is a different write entirely. Kept as a field rather than
    /// a local because it is the one piece of barrier state worth reading back.
    ready: BTreeSet<DeviceId>,
    /// When the barrier was armed, so a stuck member cannot hold it forever.
    armed_at: Timestamp,
    barrier: BarrierPolicy,
    gate: Gate,
}

/// Where a batch's rendezvous has got to.
///
/// Three states rather than a `released: bool`, because a member arriving after
/// the barrier resolved has to know *how* it resolved. Under
/// [`Barrier::DispatchReady`] a straggler should go out on its own — the device
/// group has already started without it and one more recorder is still better
/// than one fewer. Under [`Barrier::FailBatch`] it must not: the take was
/// abandoned,
/// and dispatching now would produce exactly the lone recording that policy
/// exists to rule out. A boolean cannot tell those apart, and the member that
/// asks is by definition not the one that was there when it was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// Still filling.
    Waiting,
    /// Resolved by dispatch. Later arrivals dispatch alone.
    Open,
    /// Resolved by timeout under `FailBatch`. Later arrivals are failed.
    Abandoned,
}

/// Everything the mutex guards. One struct rather than three `Mutex`es, because
/// arming a batch, queueing its members' rows and recording what the device
/// group was told are one atomic step, and separate locks would be chances to
/// interleave.
#[derive(Debug, Default)]
struct State {
    logs: BTreeMap<DeviceId, DeviceLog>,
    batches: BTreeMap<BatchId, Batch>,
    /// `group -> field -> what the group was last told that field should be`.
    ///
    /// Under the same lock as `logs` because it is written in the same critical
    /// section that admits the submission — that is the whole contract of
    /// [`GroupState`]: an expectation exists exactly when the request that
    /// produced it was accepted (see [`sismatic_store::group`]).
    ///
    /// A `BTreeMap` inside, so `expected_all` iterates in the field order the
    /// port promises rather than re-establishing it with a sort per read — the
    /// same reasoning `MemoryStore`'s inner map is one.
    groups: BTreeMap<GroupId, BTreeMap<FieldName, GroupExpectation>>,
}

#[derive(Clone)]
pub struct MemoryOutbox {
    state: Arc<Mutex<State>>,
    records: Arc<DashMap<WriteId, WriteRecord>>,
    max_attempts: u32,
    backoff: Duration,
}

impl MemoryOutbox {
    /// The default delay before a failed write is retried, multiplied by the
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
            state: Arc::default(),
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

    /// The one lock. A poisoned mutex means a previous holder panicked while
    /// the maps were half-updated; the guard is taken anyway rather than
    /// propagating, because every mutation under it is a small infallible map
    /// operation and there is no partial state a panic could plausibly leave.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The device a write belongs to, read and released before the state lock
    /// is taken. Exists to keep the `state`-before-`records` order in `settle`,
    /// which is handed an id and has to learn the device from it.
    fn device_of(&self, id: &WriteId) -> Option<DeviceId> {
        self.records.get(id).map(|r| r.device.clone())
    }
}

/// `at` plus `delay`, or `at` unchanged if it is not a timestamp this can
/// parse.
///
/// Degrading to "retry immediately" rather than propagating a parse failure:
/// the timestamp comes from the relay, an unparseable one is a bug in the
/// caller and not a reason to lose a write, and the fallback is the behaviour
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

/// Whether `deadline` has passed by `now`.
///
/// `false` when either timestamp is unparseable, which is the conservative
/// answer: an unreadable clock should leave the barrier waiting for a real
/// member rather than fire the timeout policy on a parsing accident.
fn elapsed_past(armed_at: &Timestamp, timeout: Duration, now: &Timestamp) -> bool {
    let (Ok(armed), Ok(now)) = (
        DateTime::parse_from_rfc3339(armed_at.as_str()),
        DateTime::parse_from_rfc3339(now.as_str()),
    ) else {
        return false;
    };
    let Ok(timeout) = chrono::Duration::from_std(timeout) else {
        return false;
    };
    now > armed + timeout
}

#[async_trait::async_trait]
impl WriteSubmit for MemoryOutbox {
    async fn submit(&self, s: Submission) -> Result<Acceptance, SubmitError> {
        if s.targets.is_empty() {
            return Err(SubmitError::Malformed("no targets".into()));
        }
        if s.ids.len() != s.targets.len() {
            return Err(SubmitError::Malformed(format!(
                "{} ids for {} targets",
                s.ids.len(),
                s.targets.len()
            )));
        }

        // The whole critical section, and for a group it spans every member.
        // Nothing between the first admission check and the last queue push can
        // observe a half-applied submission — which is what makes "the device
        // group starts or nothing does" true of the *decision* as well as the
        // dispatch.
        let mut state = self.state();

        // Idempotent Receiver: a client whose POST timed out and was retried
        // gets the original writes back, rather than a second start that the
        // admission table would then refuse with a 409 for something the client
        // believes never landed.
        //
        // Keyed off the first target: a repeat of the same request names the
        // same targets in the same order, so one member's key answers for all
        // of them, and a batch is returned whole.
        if let Some(key) = &s.idempotency_key
            && let Some(existing) = state
                .logs
                .get(&s.targets[0])
                .and_then(|log| log.keys.get(key))
                .cloned()
            && let Some(first) = self.records.get(&existing).map(|r| r.clone())
        {
            let writes = match &first.batch {
                // Every row of the batch, so a retry of a group request sees
                // the same set it was first told about.
                Some(batch) => self.batch_acceptances(&state, batch),
                None => vec![Accepted {
                    id: first.id.clone(),
                    device: first.device.clone(),
                    epoch: first.epoch,
                }],
            };
            return Ok(Acceptance {
                batch: first.batch.clone(),
                writes,
            });
        }

        // Admit every target *before* recording anything. A group refused by
        // its second member must leave the first member's queue untouched, so
        // the decision is taken in full and only then applied.
        let mut admitted = Vec::with_capacity(s.targets.len());
        for device in &s.targets {
            let log = state.logs.entry(device.clone()).or_default();
            let before = log.desired_recording_state;
            let after =
                admit(before, Verb::of(&s.intent)).map_err(|rejection| SubmitError::Rejected {
                    device: device.clone(),
                    rejection,
                    desired_recording_state: before,
                })?;
            admitted.push((before, after, epoch_of(before, after, log.epoch)));
        }

        // Past this point nothing can fail, so nothing needs unwinding.
        let mut writes = Vec::with_capacity(s.targets.len());
        for (i, device) in s.targets.iter().enumerate() {
            let (before, after, epoch) = admitted[i];
            let id = s.ids[i].clone();

            let record = WriteRecord {
                id: id.clone(),
                device: device.clone(),
                intent: s.intent.clone(),
                batch: s.batch.clone(),
                epoch,
                status: WriteStatus::Pending,
                attempts: 0,
                enqueued_at: s.at.clone(),
                updated_at: s.at.clone(),
                // Due immediately: the backoff exists to space out *retries*,
                // and a first attempt has nothing to back off from.
                not_before: s.at.clone(),
            };

            // Record before queue, so a reader walking the queue never meets an
            // id with no record behind it.
            self.records.insert(id.clone(), record);

            let log = state.logs.entry(device.clone()).or_default();
            log.queue.push_back(id.clone());
            log.history.push(id.clone());
            log.desired_recording_state = after;
            if opens_recording(before, after) {
                log.epoch = epoch;
            }
            if let Some(key) = &s.idempotency_key {
                log.keys.insert(key.clone(), id.clone());
            }

            writes.push(Accepted {
                id,
                device: device.clone(),
                epoch,
            });
        }

        // Recorded here — inside the critical section, after the last thing
        // that could have failed — so it holds exactly when the request was
        // admitted. A submission the table refused returned above with the
        // group's previous expectation untouched, which is what stops a
        // rejected start from claiming the device group was asked to record.
        // An idempotent replay returned above too, and deliberately: it
        // produced no new writes, so moving `since` forward would report a
        // device group as freshly told when nothing was sent.
        //
        // Every accepted group write gets one, batched or not: a metadata write
        // fans out without a rendezvous (it gains nothing from acting in
        // unison) and is exactly as capable of reaching four recorders out of
        // five, which is the drift this is here to make visible.
        if let Some(group) = &s.group {
            let (field, value) = expects(&s.intent);
            state.groups.entry(group.clone()).or_default().insert(
                field.clone(),
                GroupExpectation {
                    field,
                    value,
                    // The submission's instant, so every member of one request
                    // shares it and `since` reads as "when the device group
                    // was told" rather than when some row happened to be
                    // written.
                    since: s.at.clone(),
                },
            );
        }

        if let Some(batch_id) = &s.batch {
            // Armed here rather than on first arrival, so `armed_at` measures
            // the wait from when the request was accepted — which is the
            // interval an operator experiences — rather than from whenever the
            // first relay task happened to tick.
            state.batches.insert(
                batch_id.clone(),
                Batch {
                    members: s.targets.iter().cloned().collect(),
                    ready: BTreeSet::new(),
                    armed_at: s.at.clone(),
                    barrier: s.barrier.unwrap_or(BarrierPolicy {
                        timeout: Duration::from_secs(15),
                        on_timeout: Barrier::FailBatch,
                    }),
                    gate: Gate::Waiting,
                },
            );
        }

        Ok(Acceptance {
            batch: s.batch,
            writes,
        })
    }
}

impl MemoryOutbox {
    /// Every row of `batch`, as acceptances. Used by the idempotency path so a
    /// retried group request is answered with the whole batch rather than the
    /// one member whose key matched.
    fn batch_acceptances(&self, state: &State, batch: &BatchId) -> Vec<Accepted> {
        let Some(entry) = state.batches.get(batch) else {
            return Vec::new();
        };
        entry
            .members
            .iter()
            .filter_map(|device| {
                state.logs.get(device).and_then(|log| {
                    log.history
                        .iter()
                        .rev()
                        .filter_map(|id| self.records.get(id).map(|r| r.clone()))
                        .find(|r| r.batch.as_ref() == Some(batch))
                })
            })
            .map(|r| Accepted {
                id: r.id,
                device: r.device,
                epoch: r.epoch,
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl WriteLog for MemoryOutbox {
    async fn write(&self, id: WriteId) -> Result<Option<WriteRecord>, ReadError> {
        Ok(self.records.get(&id).map(|r| r.clone()))
    }

    async fn writes_for(&self, device: DeviceId) -> Result<Vec<WriteRecord>, ReadError> {
        let state = self.state();
        let Some(log) = state.logs.get(&device) else {
            return Ok(Vec::new());
        };
        Ok(log
            .history
            .iter()
            .rev()
            .filter_map(|id| self.records.get(id).map(|r| r.clone()))
            .collect())
    }

    async fn desired_recording_state(
        &self,
        device: DeviceId,
    ) -> Result<DeviceDesiredRecordingState, ReadError> {
        let state = self.state();
        let (desired_recording_state, epoch) = state
            .logs
            .get(&device)
            .map_or((DesiredRecordingState::Idle, 0), |log| {
                (log.desired_recording_state, log.epoch)
            });
        Ok(DeviceDesiredRecordingState {
            desired_recording_state,
            epoch,
        })
    }
}

/// The read half of what [`WriteSubmit::submit`] recorded about device
/// groups.
///
/// On the outbox rather than on `MemoryStore`, because an expectation is
/// write-side belief — the same kind of thing a [`DesiredRecordingState`] is,
/// written by the same call, under the same lock. Putting it beside the
/// reads would have made "what the device group was told" and "what a
/// device reported" two rows in one table, which is exactly the conflation the
/// group routes exist to undo.
#[async_trait::async_trait]
impl GroupState for MemoryOutbox {
    async fn expected(
        &self,
        group: GroupId,
        field: FieldName,
    ) -> Result<Option<GroupExpectation>, ReadError> {
        Ok(self
            .state()
            .groups
            .get(&group)
            .and_then(|fields| fields.get(&field).cloned()))
    }

    async fn expected_all(&self, group: GroupId) -> Result<Vec<GroupExpectation>, ReadError> {
        // `BTreeMap`'s iteration order *is* the field ordering the port
        // promises, so there is nothing to sort here.
        Ok(self
            .state()
            .groups
            .get(&group)
            .map(|fields| fields.values().cloned().collect())
            .unwrap_or_default())
    }
}

/// What evaluating a batched head decided.
enum BatchStep {
    /// Not everyone is here and the barrier has not expired. Leave the row
    /// where it is and try again next tick.
    Wait,
    /// Dispatch these members as one group run.
    Release(Vec<DeviceId>),
    /// Dispatch this member on its own: the barrier already resolved by
    /// dispatch, and this is a straggler under `DispatchReady`.
    Alone,
    /// Fail these members' rows without contacting anything.
    Fail(Vec<DeviceId>),
}

#[async_trait::async_trait]
impl WriteDrain for MemoryOutbox {
    async fn claim_next(
        &self,
        device: DeviceId,
        at: Timestamp,
    ) -> Result<Option<Claim>, WriteError> {
        let mut state = self.state();

        // Peek rather than pop: a batched row at the head must stay there until
        // every sibling has arrived, because popping it would let the write
        // behind it overtake — the exact reordering per-device FIFO exists to
        // prevent.
        let Some(head) = state
            .logs
            .get(&device)
            .and_then(|log| log.queue.front().cloned())
        else {
            return Ok(None);
        };
        let Some(record) = self.records.get(&head).map(|r| r.clone()) else {
            return Err(WriteError::backend("a queued id has no record behind it"));
        };

        // A write still serving its backoff blocks the queue rather than
        // being skipped over. Skipping would let a later write overtake the
        // retry of an earlier one.
        if at.as_str() < record.not_before.as_str() {
            return Ok(None);
        }

        let Some(batch_id) = record.batch.clone() else {
            // Unbatched: the existing path, unchanged.
            let claimed = self.claim_head(&mut state, &device, &at)?;
            return Ok(claimed.map(Claim::One));
        };

        match self.step_batch(&mut state, &batch_id, &device, &at) {
            BatchStep::Wait => Ok(None),
            BatchStep::Alone => Ok(self.claim_head(&mut state, &device, &at)?.map(Claim::One)),
            BatchStep::Release(members) => {
                let mut records = Vec::with_capacity(members.len());
                for member in &members {
                    if let Some(record) = self.claim_head(&mut state, member, &at)? {
                        records.push(record);
                    }
                }
                Ok(Some(Claim::Batch {
                    id: batch_id,
                    records,
                }))
            }
            BatchStep::Fail(members) => {
                // Failed here rather than handed to the relay to fail: there is
                // no device to contact and no exchange to report the failure
                // of, so routing it through a dispatch would only invent one.
                for member in &members {
                    self.fail_head(&mut state, member, &at);
                }
                Ok(None)
            }
        }
    }

    async fn settle(&self, id: WriteId, outcome: Outcome, at: Timestamp) -> Result<(), WriteError> {
        // Read the device and release, so the guards below are taken in the
        // `state`-then-`records` order every other method uses.
        let device = self
            .device_of(&id)
            .ok_or_else(|| WriteError::backend("settling an unknown write"))?;
        let mut state = self.state();
        let mut record = self
            .records
            .get_mut(&id)
            .ok_or_else(|| WriteError::backend("settling an unknown write"))?;

        if record.status != WriteStatus::InFlight {
            return Err(WriteError::backend(
                "settling a write that was not in flight",
            ));
        }

        let log = state.logs.entry(device.clone()).or_default();
        match outcome {
            Outcome::Succeeded(value) => {
                record.status = WriteStatus::Succeeded { value };
            }
            Outcome::Failed(_) if record.attempts < self.max_attempts => {
                record.status = WriteStatus::Pending;
                // Back onto the *front* of the queue: a retry must not be
                // overtaken by writes submitted after it.
                log.queue.push_front(id.clone());
                // Spaced out proportionally to what has already been spent, so
                // three attempts against a device that is down do not all land
                // inside one poll interval and exhaust the budget in a moment.
                record.not_before = delay_from(&at, self.backoff * record.attempts);

                // A retried batch member re-enters the rendezvous: it is no
                // longer at the head *and ready*, so its siblings must wait for
                // it again rather than dispatching without it.
                if let Some(batch) = record.batch.as_ref().and_then(|b| state.batches.get_mut(b)) {
                    batch.ready.remove(&device);
                }
            }
            Outcome::Failed(reason) => {
                record.status = WriteStatus::Failed { reason };
                // Dead Letter Channel. The desired state this write
                // optimistically moved is rolled back, so a start that never
                // reached the device stops freezing this device's metadata.
                log.desired_recording_state =
                    rollback(log.desired_recording_state, Verb::of(&record.intent));
                if let Some(batch) = record.batch.as_ref().and_then(|b| state.batches.get_mut(b)) {
                    // Terminally gone: drop it from the set the barrier waits
                    // on, or a `DispatchReady` batch would keep waiting for a
                    // member that will never arrive.
                    batch.members.remove(&device);
                    batch.ready.remove(&device);
                }
            }
        }
        record.updated_at = at;
        Ok(())
    }

    async fn observe(&self, device: DeviceId, observed: RecordingState) -> Result<(), WriteError> {
        let mut state = self.state();
        let log = state.logs.entry(device).or_default();
        let before = log.desired_recording_state;
        let after = reconcile(before, observed);
        // A recording started from the front panel opens a take this process
        // never admitted, and the metadata of the previous one is sealed by it
        // just the same.
        if opens_recording(before, after) {
            log.epoch += 1;
        }
        log.desired_recording_state = after;
        Ok(())
    }

    async fn in_flight(&self, device: DeviceId) -> Result<Vec<WriteRecord>, WriteError> {
        let state = self.state();
        let Some(log) = state.logs.get(&device) else {
            return Ok(Vec::new());
        };
        Ok(log
            .history
            .iter()
            .filter_map(|id| self.records.get(id).map(|r| r.clone()))
            .filter(|r| r.status == WriteStatus::InFlight)
            .collect())
    }
}

impl MemoryOutbox {
    /// Pop `device`'s head and mark it in flight. The caller has already
    /// established that the head is due and claimable.
    fn claim_head(
        &self,
        state: &mut State,
        device: &DeviceId,
        at: &Timestamp,
    ) -> Result<Option<WriteRecord>, WriteError> {
        let Some(log) = state.logs.get_mut(device) else {
            return Ok(None);
        };
        let Some(id) = log.queue.pop_front() else {
            return Ok(None);
        };
        let mut record = self
            .records
            .get_mut(&id)
            .ok_or_else(|| WriteError::backend("a queued id has no record behind it"))?;
        record.status = WriteStatus::InFlight;
        record.attempts += 1;
        record.updated_at = at.clone();
        Ok(Some(record.clone()))
    }

    /// Fail `device`'s head row outright, without a dispatch and without
    /// spending a retry.
    ///
    /// Deliberately not routed through `settle`'s retry arm: a barrier expiry
    /// is not a transient device failure, and re-queueing the row would only
    /// have it arrive at a rendezvous that has already resolved. The desired
    /// recording state is rolled back for the same reason `settle` rolls it
    /// back — a start that never reached a device must stop freezing that
    /// device's metadata.
    fn fail_head(&self, state: &mut State, device: &DeviceId, at: &Timestamp) {
        let Some(log) = state.logs.get_mut(device) else {
            return;
        };
        let Some(id) = log.queue.pop_front() else {
            return;
        };
        let Some(mut record) = self.records.get_mut(&id) else {
            return;
        };
        record.status = WriteStatus::Failed {
            reason: "the group barrier expired before every member was ready".to_owned(),
        };
        record.updated_at = at.clone();
        log.desired_recording_state =
            rollback(log.desired_recording_state, Verb::of(&record.intent));
    }

    /// Re-evaluate the barrier and decide what this tick does.
    ///
    /// Readiness is recomputed from the live queues rather than accumulated,
    /// for the reason on [`Batch::ready`]: a member can stop being ready
    /// between ticks, and a set that only ever grows would release the batch
    /// against a queue head that had moved on.
    fn step_batch(
        &self,
        state: &mut State,
        batch_id: &BatchId,
        device: &DeviceId,
        at: &Timestamp,
    ) -> BatchStep {
        let Some(batch) = state.batches.get(batch_id).cloned() else {
            // No batch behind the id. "Wait" rather than "dispatch alone": a
            // batched row whose rendezvous has vanished must not go out on its
            // own, because acting alone is what the batch exists to prevent.
            return BatchStep::Wait;
        };

        match batch.gate {
            // Resolved by dispatch; this is a straggler. Under `DispatchReady`
            // that is the whole point — the device group started without it
            // and one more recorder still beats one fewer.
            Gate::Open => return BatchStep::Alone,
            // Resolved by timeout under `FailBatch`. The take was abandoned,
            // so this row goes with it — this member and no other, since the
            // ones that were present were already failed when it resolved.
            Gate::Abandoned => return BatchStep::Fail(vec![device.clone()]),
            Gate::Waiting => {}
        }

        let ready: BTreeSet<DeviceId> = batch
            .members
            .iter()
            .filter(|member| self.is_ready_for(state, member, batch_id, at))
            .cloned()
            .collect();

        let complete = ready == batch.members;
        let expired = !complete && elapsed_past(&batch.armed_at, batch.barrier.timeout, at);

        let step = if complete {
            BatchStep::Release(ready.iter().cloned().collect())
        } else if !expired {
            // Not everyone is here. Try again next tick.
            BatchStep::Wait
        } else {
            match batch.barrier.on_timeout {
                Barrier::DispatchReady => BatchStep::Release(ready.iter().cloned().collect()),
                Barrier::FailBatch => BatchStep::Fail(ready.iter().cloned().collect()),
            }
        };

        let entry = state.batches.get_mut(batch_id).expect("just read");
        entry.ready = ready;
        entry.gate = match step {
            BatchStep::Wait => Gate::Waiting,
            BatchStep::Release(_) | BatchStep::Alone => Gate::Open,
            BatchStep::Fail(_) => Gate::Abandoned,
        };
        step
    }

    /// Whether `member`'s batched row is at the head of its queue and due.
    ///
    /// This *is* the definition of ready, and it is asked of the live queue so
    /// it cannot be stale. A member whose head is some other write — because
    /// a retry of the write ahead was pushed back to the front — is not
    /// ready, however recently it was.
    fn is_ready_for(
        &self,
        state: &State,
        member: &DeviceId,
        batch_id: &BatchId,
        at: &Timestamp,
    ) -> bool {
        let Some(head) = state
            .logs
            .get(member)
            .and_then(|log| log.queue.front().cloned())
        else {
            return false;
        };
        let Some(record) = self.records.get(&head) else {
            return false;
        };
        record.batch.as_deref() == Some(batch_id.as_str())
            && at.as_str() >= record.not_before.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{Intent, ReadValue, Rejection};

    pub(super) const T0: &str = "2026-08-17T00:00:00.000Z";
    const DEV: &str = "sim";

    pub(super) fn at(s: &str) -> Timestamp {
        Timestamp(s.to_owned())
    }

    /// `T0` plus `secs`, for the barrier tests. Written out rather than
    /// computed so the expected instants read at a glance.
    pub(super) fn at_plus(secs: u32) -> Timestamp {
        at(&format!("2026-08-17T00:00:{secs:02}.000Z"))
    }

    pub(super) fn outbox() -> MemoryOutbox {
        // Zero backoff by default: the tests that are not about retry timing
        // should not have to wait for it, and the ones that are set their own.
        MemoryOutbox::with_max_attempts(3).with_backoff(Duration::ZERO)
    }

    fn title(value: &str) -> Intent {
        Intent::SetMetadata {
            field: "TITLE".to_owned(),
            value: value.to_owned(),
        }
    }

    /// A one-device submission — the shape every non-group test uses.
    fn one(id: &str, intent: Intent) -> Submission {
        Submission {
            ids: vec![id.to_owned()],
            targets: vec![DEV.to_owned()],
            group: None,
            batch: None,
            barrier: None,
            intent,
            at: at(T0),
            idempotency_key: None,
        }
    }

    async fn submit(
        outbox: &MemoryOutbox,
        id: &str,
        intent: Intent,
    ) -> Result<Acceptance, SubmitError> {
        outbox.submit(one(id, intent)).await
    }

    /// Claim, then settle with `outcome`. The pair the relay always performs
    /// together.
    async fn dispatch(outbox: &MemoryOutbox, outcome: Outcome) -> Option<WriteId> {
        let claimed = outbox
            .claim_next(DEV.to_owned(), at(T0))
            .await
            .expect("claim")?;
        let Claim::One(record) = claimed else {
            panic!("expected a lone write, got a batch");
        };
        outbox
            .settle(record.id.clone(), outcome, at(T0))
            .await
            .expect("settle");
        Some(record.id)
    }

    pub(super) async fn desired_recording_state_of(
        outbox: &MemoryOutbox,
        device: &str,
    ) -> DeviceDesiredRecordingState {
        outbox
            .desired_recording_state(device.to_owned())
            .await
            .expect("desired_recording_state")
    }

    pub(super) async fn status_of(outbox: &MemoryOutbox, id: &str) -> WriteStatus {
        outbox
            .write(id.to_owned())
            .await
            .expect("read")
            .expect("the write exists")
            .status
    }

    // ---- malformed submissions -------------------------------------------

    #[tokio::test]
    async fn a_submission_with_no_targets_is_refused_as_malformed() {
        let outbox = outbox();
        let mut s = one("cmd", title("a"));
        s.targets.clear();
        s.ids.clear();
        assert_eq!(
            outbox.submit(s).await.unwrap_err(),
            SubmitError::Malformed("no targets".into())
        );
    }

    /// Zipping the shorter of the two would silently drop a member — a group
    /// that started one recorder short and reported success.
    #[tokio::test]
    async fn a_submission_whose_ids_and_targets_disagree_is_refused() {
        let outbox = outbox();
        let mut s = one("cmd", title("a"));
        s.targets.push("other".to_owned());
        assert!(matches!(
            outbox.submit(s).await.unwrap_err(),
            SubmitError::Malformed(_)
        ));
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
                device: DEV.to_owned(),
                rejection: Rejection::MetadataFrozen,
                desired_recording_state: DesiredRecordingState::Recording,
            }
        );
        assert!(submit(&outbox, "d", Intent::StopRecording).await.is_ok());
        assert!(submit(&outbox, "e", title("after")).await.is_ok());
    }

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
        assert_eq!(
            desired_recording_state_of(&outbox(), "never-heard-of").await,
            DeviceDesiredRecordingState {
                desired_recording_state: DesiredRecordingState::Idle,
                epoch: 0
            }
        );
    }

    // ---- the epoch --------------------------------------------------------

    #[tokio::test]
    async fn metadata_prepared_for_a_take_shares_that_takes_epoch() {
        let outbox = outbox();
        let epoch = |a: Acceptance| a.writes[0].epoch;

        let first = submit(&outbox, "a", title("Week 4")).await.map(epoch);
        let presenter = submit(
            &outbox,
            "b",
            Intent::SetMetadata {
                field: "PRESENTER".to_owned(),
                value: "Komar".to_owned(),
            },
        )
        .await
        .map(epoch);
        let start = submit(&outbox, "c", Intent::StartRecording)
            .await
            .map(epoch);
        assert_eq!((first, presenter, start), (Ok(1), Ok(1), Ok(1)));

        submit(&outbox, "d", Intent::StopRecording).await.unwrap();
        assert_eq!(
            submit(&outbox, "e", title("Week 5")).await.map(epoch),
            Ok(2),
            "a new take must get a new epoch"
        );
    }

    #[tokio::test]
    async fn pausing_and_resuming_stays_in_one_epoch() {
        let outbox = outbox();
        submit(&outbox, "a", Intent::StartRecording).await.unwrap();
        submit(&outbox, "b", Intent::PauseRecording).await.unwrap();
        let resumed = submit(&outbox, "c", Intent::StartRecording).await.unwrap();
        assert_eq!(resumed.writes[0].epoch, 1);
        assert_eq!(desired_recording_state_of(&outbox, DEV).await.epoch, 1);
    }

    // ---- atomicity --------------------------------------------------------

    /// The race the atomicity contract exists for, and the test that fails if
    /// the critical section in `submit` is ever split.
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
            "the desired_recording_state guard must serialise competing starts"
        );
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
        while let Some(id) = dispatch(&outbox, Outcome::Succeeded(ReadValue::Flag(true))).await {
            claimed.push(id);
        }
        assert_eq!(claimed, ["first", "second", "third"]);
    }

    #[tokio::test]
    async fn an_empty_queue_claims_nothing() {
        assert_eq!(
            outbox().claim_next(DEV.to_owned(), at(T0)).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_retry_is_re_dispatched_before_later_writes() {
        let outbox = outbox();
        submit(&outbox, "write", title("a")).await.unwrap();
        submit(&outbox, "start", Intent::StartRecording)
            .await
            .unwrap();

        assert_eq!(
            dispatch(&outbox, Outcome::Failed("ssh died".into()))
                .await
                .as_deref(),
            Some("write")
        );
        assert_eq!(
            dispatch(&outbox, Outcome::Succeeded(ReadValue::Flag(true)))
                .await
                .as_deref(),
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
        assert_eq!(
            outbox.claim_next(DEV.to_owned(), at(T0)).await.unwrap(),
            None
        );
        let record = outbox.write("cmd".to_owned()).await.unwrap().unwrap();
        assert_eq!(record.attempts, 3);
        assert_eq!(
            record.status,
            WriteStatus::Failed {
                reason: "down".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn a_backing_off_write_holds_the_queue() {
        let outbox = MemoryOutbox::with_max_attempts(3).with_backoff(Duration::from_secs(60));
        submit(&outbox, "write", title("a")).await.unwrap();
        submit(&outbox, "start", Intent::StartRecording)
            .await
            .unwrap();
        dispatch(&outbox, Outcome::Failed("down".into())).await;

        assert_eq!(
            outbox.claim_next(DEV.to_owned(), at_plus(1)).await.unwrap(),
            None
        );
        let due = outbox
            .claim_next(DEV.to_owned(), at("2026-08-17T00:01:00.000Z"))
            .await
            .unwrap();
        assert!(matches!(due, Some(Claim::One(r)) if r.id == "write"));
    }

    #[tokio::test]
    async fn settling_a_write_twice_is_refused() {
        let outbox = outbox();
        submit(&outbox, "cmd", title("a")).await.unwrap();
        dispatch(&outbox, Outcome::Succeeded(ReadValue::Flag(true))).await;
        assert_eq!(
            outbox
                .settle("cmd".to_owned(), Outcome::Failed("late".into()), at(T0))
                .await,
            Err(WriteError::backend(
                "settling a write that was not in flight"
            ))
        );
    }

    #[tokio::test]
    async fn a_start_that_never_reached_the_device_unfreezes_metadata() {
        let outbox = MemoryOutbox::with_max_attempts(1).with_backoff(Duration::ZERO);
        submit(&outbox, "start", Intent::StartRecording)
            .await
            .unwrap();
        assert_eq!(
            desired_recording_state_of(&outbox, DEV)
                .await
                .desired_recording_state,
            DesiredRecordingState::Recording
        );

        dispatch(&outbox, Outcome::Failed("device unreachable".into())).await;

        assert_eq!(
            desired_recording_state_of(&outbox, DEV)
                .await
                .desired_recording_state,
            DesiredRecordingState::Idle
        );
        assert!(submit(&outbox, "title", title("a")).await.is_ok());
    }

    // ---- reconciliation ---------------------------------------------------

    #[tokio::test]
    async fn an_observed_recording_freezes_metadata_this_process_never_started() {
        let outbox = outbox();
        assert!(submit(&outbox, "before", title("a")).await.is_ok());
        outbox
            .observe(DEV.to_owned(), RecordingState::Started)
            .await
            .unwrap();
        assert!(submit(&outbox, "after", title("b")).await.is_err());
        assert_eq!(desired_recording_state_of(&outbox, DEV).await.epoch, 1);
    }

    #[tokio::test]
    async fn an_unknown_observed_state_changes_nothing() {
        let outbox = outbox();
        submit(&outbox, "start", Intent::StartRecording)
            .await
            .unwrap();
        outbox
            .observe(DEV.to_owned(), RecordingState::Unknown)
            .await
            .unwrap();
        assert_eq!(
            desired_recording_state_of(&outbox, DEV)
                .await
                .desired_recording_state,
            DesiredRecordingState::Recording
        );
    }

    // ---- idempotency ------------------------------------------------------

    #[tokio::test]
    async fn a_repeated_key_returns_the_original_write() {
        let outbox = outbox();
        let submission = |id: &str| Submission {
            idempotency_key: Some("take-4".to_owned()),
            ..one(id, Intent::StartRecording)
        };

        let first = outbox.submit(submission("one")).await.unwrap();
        let second = outbox.submit(submission("two")).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(outbox.writes_for(DEV.to_owned()).await.unwrap().len(), 1);
    }

    // ---- the log ----------------------------------------------------------

    #[tokio::test]
    async fn writes_are_listed_newest_first_and_scoped_to_one_device() {
        let outbox = outbox();
        submit(&outbox, "first", title("a")).await.unwrap();
        submit(&outbox, "second", title("b")).await.unwrap();
        outbox
            .submit(Submission {
                targets: vec!["other".to_owned()],
                ..one("elsewhere", title("c"))
            })
            .await
            .unwrap();

        let ids: Vec<_> = outbox
            .writes_for(DEV.to_owned())
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        // Newest first, and by insertion order rather than by a timestamp — all
        // three share one, which is what a sort on `enqueued_at` gets wrong.
        assert_eq!(ids, ["second", "first"]);
    }

    #[tokio::test]
    async fn an_unknown_write_is_absence_not_an_error() {
        let outbox = outbox();
        assert_eq!(outbox.write("nope".to_owned()).await.unwrap(), None);
        assert_eq!(
            outbox.writes_for("nobody".to_owned()).await.unwrap(),
            Vec::new()
        );
    }

    #[tokio::test]
    async fn only_claimed_writes_are_in_flight() {
        let outbox = outbox();
        submit(&outbox, "claimed", title("a")).await.unwrap();
        submit(&outbox, "queued", title("b")).await.unwrap();
        outbox.claim_next(DEV.to_owned(), at(T0)).await.unwrap();

        let stranded = outbox.in_flight(DEV.to_owned()).await.unwrap();
        assert_eq!(
            stranded.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["claimed"]
        );
    }
}

/// The rendezvous: what a group-addressed write does across its members.
#[cfg(test)]
mod batch_tests {
    use super::tests::*;
    use super::*;
    use sismatic_api_types::{Intent, ReadValue};

    const A: &str = "atrium";
    const B: &str = "annex";
    const GROUP: &str = "atrium-room";
    const BATCH: &str = "batch-1";

    /// A group submission across [`A`] and [`B`], with `barrier` as the policy
    /// and a 10-second bound.
    fn group_start(on_timeout: Barrier) -> Submission {
        Submission {
            ids: vec!["cmd-a".to_owned(), "cmd-b".to_owned()],
            targets: vec![A.to_owned(), B.to_owned()],
            group: Some(GROUP.to_owned()),
            batch: Some(BATCH.to_owned()),
            barrier: Some(BarrierPolicy {
                timeout: Duration::from_secs(10),
                on_timeout,
            }),
            intent: Intent::StartRecording,
            at: at(T0),
            idempotency_key: None,
        }
    }

    /// A lone write queued on `device` *ahead* of the batch, so that member
    /// is not at the head when the batch is submitted.
    fn blocker(id: &str, device: &str) -> Submission {
        Submission {
            ids: vec![id.to_owned()],
            targets: vec![device.to_owned()],
            group: None,
            batch: None,
            barrier: None,
            intent: Intent::SetMetadata {
                field: "TITLE".to_owned(),
                value: "ahead".to_owned(),
            },
            at: at(T0),
            idempotency_key: None,
        }
    }

    async fn claim(outbox: &MemoryOutbox, device: &str, at_: Timestamp) -> Option<Claim> {
        outbox
            .claim_next(device.to_owned(), at_)
            .await
            .expect("claim")
    }

    // ---- expansion --------------------------------------------------------

    #[tokio::test]
    async fn a_group_submission_becomes_one_row_per_member_sharing_a_batch() {
        let outbox = outbox();
        let accepted = outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();

        assert_eq!(accepted.batch.as_deref(), Some(BATCH));
        assert_eq!(
            accepted
                .writes
                .iter()
                .map(|c| (c.id.as_str(), c.device.as_str()))
                .collect::<Vec<_>>(),
            [("cmd-a", A), ("cmd-b", B)]
        );
        // Each row lands in its own device's queue and carries the batch.
        for (id, device) in [("cmd-a", A), ("cmd-b", B)] {
            let record = outbox.write(id.to_owned()).await.unwrap().unwrap();
            assert_eq!(record.device, device);
            assert_eq!(record.batch.as_deref(), Some(BATCH));
            assert_eq!(record.epoch, 1, "every member starts its own first take");
        }
    }

    /// Admission is across every member at once, so one member's refusal
    /// refuses the whole request — and, the part that matters, leaves the
    /// other member's queue untouched.
    #[tokio::test]
    async fn a_group_refused_by_one_member_records_nothing_at_all() {
        let outbox = outbox();
        // B is already recording, so a group start cannot be admitted for it.
        outbox
            .submit(Submission {
                ids: vec!["solo".to_owned()],
                targets: vec![B.to_owned()],
                ..group_start(Barrier::FailBatch)
            })
            .await
            .unwrap();

        let refused = outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, SubmitError::Rejected { device, .. } if device == B),
            "the refusing member must be named, got {refused:?}"
        );

        // A never learned about it.
        assert!(outbox.writes_for(A.to_owned()).await.unwrap().is_empty());
        assert_eq!(
            desired_recording_state_of(&outbox, A)
                .await
                .desired_recording_state,
            DesiredRecordingState::Idle
        );
    }

    // ---- the barrier ------------------------------------------------------

    /// The rendezvous itself. One member at the head is not enough; the batch
    /// goes out only when both are, and it goes out as one claim.
    #[tokio::test]
    async fn no_member_dispatches_until_every_member_is_ready() {
        let outbox = outbox();
        // B has a write ahead of its batched row, so it cannot be ready yet.
        outbox.submit(blocker("ahead-b", B)).await.unwrap();
        outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();

        // A is at the head and due, and still gets nothing.
        assert!(
            claim(&outbox, A, at(T0)).await.is_none(),
            "a member must not go out alone"
        );

        // Clear B's blocker; now both are at the head.
        let Some(Claim::One(ahead)) = claim(&outbox, B, at(T0)).await else {
            panic!("B's blocker should be claimable");
        };
        assert_eq!(ahead.id, "ahead-b");
        outbox
            .settle(ahead.id, Outcome::Succeeded(ReadValue::Flag(true)), at(T0))
            .await
            .unwrap();

        let Some(Claim::Batch { id, records }) = claim(&outbox, A, at(T0)).await else {
            panic!("the barrier should have filled");
        };
        assert_eq!(id, BATCH);
        // Ordered by device id ("annex" before "atrium"), which is the set's
        // order rather than the group's — see `Claim::Batch`.
        assert_eq!(
            records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["cmd-b", "cmd-a"],
            "one claim carries every member's row"
        );
    }

    /// Exactly one task runs the group. A sibling ticking a moment later must
    /// not dispatch the same rows again.
    #[tokio::test]
    async fn only_one_member_receives_the_batch() {
        let outbox = outbox();
        outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();

        assert!(matches!(
            claim(&outbox, A, at(T0)).await,
            Some(Claim::Batch { .. })
        ));
        assert!(
            claim(&outbox, B, at(T0)).await.is_none(),
            "the sibling's row was already claimed by the batch"
        );
    }

    /// The reordering the peek-not-pop rule exists for: a batched row that is
    /// waiting must stay at the head, or the write behind it overtakes.
    #[tokio::test]
    async fn a_waiting_batched_row_is_not_overtaken() {
        let outbox = outbox();
        outbox.submit(blocker("ahead-b", B)).await.unwrap();
        outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();
        // Queued behind A's batched row. A *setting* rather than a metadata
        // write, because the group start already moved A's desired state to
        // Recording and metadata is frozen there — settings are admissible in
        // every state, which is what makes this the write that could
        // overtake.
        outbox
            .submit(Submission {
                ids: vec!["after-a".to_owned()],
                targets: vec![A.to_owned()],
                group: None,
                batch: None,
                barrier: None,
                intent: Intent::SetSetting {
                    field: "TIMEZONE".to_owned(),
                    value: "UTC".to_owned(),
                },
                at: at(T0),
                idempotency_key: None,
            })
            .await
            .unwrap();

        // A is blocked on the barrier, and `after-a` must not slip past it.
        assert!(claim(&outbox, A, at(T0)).await.is_none());
        assert_eq!(
            status_of(&outbox, "after-a").await,
            WriteStatus::Pending,
            "the write behind a waiting batch must stay behind it"
        );
    }

    // ---- the timeout policies --------------------------------------------

    /// `DispatchReady`: one recorder is better than none.
    #[tokio::test]
    async fn dispatch_ready_runs_the_members_that_arrived() {
        let outbox = outbox();
        outbox.submit(blocker("ahead-b", B)).await.unwrap();
        outbox
            .submit(group_start(Barrier::DispatchReady))
            .await
            .unwrap();

        // Inside the bound: still waiting for B.
        assert!(claim(&outbox, A, at_plus(9)).await.is_none());

        // Past it: A goes alone.
        let Some(Claim::Batch { records, .. }) = claim(&outbox, A, at_plus(11)).await else {
            panic!("the ready member should have been dispatched");
        };
        assert_eq!(
            records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["cmd-a"]
        );
    }

    /// ...and a straggler under that policy still goes, because the device
    /// group has already started and one more recorder beats one fewer.
    #[tokio::test]
    async fn a_straggler_under_dispatch_ready_goes_out_alone() {
        let outbox = outbox();
        outbox.submit(blocker("ahead-b", B)).await.unwrap();
        outbox
            .submit(group_start(Barrier::DispatchReady))
            .await
            .unwrap();
        claim(&outbox, A, at_plus(11)).await;

        // B clears its blocker and arrives late.
        let Some(Claim::One(ahead)) = claim(&outbox, B, at_plus(12)).await else {
            panic!("B's blocker should be claimable");
        };
        outbox
            .settle(
                ahead.id,
                Outcome::Succeeded(ReadValue::Flag(true)),
                at_plus(12),
            )
            .await
            .unwrap();

        let late = claim(&outbox, B, at_plus(13)).await;
        assert!(
            matches!(&late, Some(Claim::One(r)) if r.id == "cmd-b"),
            "a straggler must still run under DispatchReady, got {late:?}"
        );
    }

    /// `FailBatch`: two recordings or neither. Nothing is dispatched, and the
    /// rows that were waiting are failed without contacting a device.
    #[tokio::test]
    async fn fail_batch_abandons_the_take_rather_than_starting_half_a_room() {
        let outbox = outbox();
        outbox.submit(blocker("ahead-b", B)).await.unwrap();
        outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();

        assert!(claim(&outbox, A, at_plus(9)).await.is_none());
        assert!(
            claim(&outbox, A, at_plus(11)).await.is_none(),
            "nothing is dispatched when the batch is abandoned"
        );

        assert!(
            matches!(
                status_of(&outbox, "cmd-a").await,
                WriteStatus::Failed { .. }
            ),
            "the ready member's row is failed, not left pending"
        );
        // ...and the desired state it optimistically moved is rolled back, so A's
        // metadata is writable again rather than frozen by a take that never
        // started.
        assert_eq!(
            desired_recording_state_of(&outbox, A)
                .await
                .desired_recording_state,
            DesiredRecordingState::Idle
        );
    }

    #[tokio::test]
    async fn a_straggler_under_fail_batch_is_failed_too() {
        let outbox = outbox();
        outbox.submit(blocker("ahead-b", B)).await.unwrap();
        outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();
        claim(&outbox, A, at_plus(11)).await;

        let Some(Claim::One(ahead)) = claim(&outbox, B, at_plus(12)).await else {
            panic!("B's blocker should be claimable");
        };
        outbox
            .settle(
                ahead.id,
                Outcome::Succeeded(ReadValue::Flag(true)),
                at_plus(12),
            )
            .await
            .unwrap();

        assert!(
            claim(&outbox, B, at_plus(13)).await.is_none(),
            "the take was abandoned; a late member must not start alone"
        );
        assert!(matches!(
            status_of(&outbox, "cmd-b").await,
            WriteStatus::Failed { .. }
        ));
        assert_eq!(
            desired_recording_state_of(&outbox, B)
                .await
                .desired_recording_state,
            DesiredRecordingState::Idle
        );
    }

    /// A batch that fills inside the bound is unaffected by the timeout —
    /// stated because an off-by-one in the comparison would make every batch
    /// take the timeout path.
    #[tokio::test]
    async fn a_batch_that_fills_in_time_is_dispatched_not_timed_out() {
        let outbox = outbox();
        outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();

        let claimed = claim(&outbox, A, at_plus(10)).await;
        assert!(
            matches!(claimed, Some(Claim::Batch { ref records, .. }) if records.len() == 2),
            "got {claimed:?}"
        );
    }

    /// The staleness bug the recomputed `ready` set exists to prevent: a member
    /// that reached the head and was then overtaken by a retry is no longer
    /// ready, and the batch must not release against its new head.
    #[tokio::test]
    async fn a_member_overtaken_by_a_retry_stops_being_ready() {
        let outbox = MemoryOutbox::with_max_attempts(3).with_backoff(Duration::ZERO);
        outbox.submit(blocker("ahead-a", A)).await.unwrap();
        outbox.submit(blocker("ahead-b", B)).await.unwrap();
        outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();

        // A clears its blocker, so A is now at the head and ready.
        let Some(Claim::One(a_head)) = claim(&outbox, A, at(T0)).await else {
            panic!("A's blocker should be claimable");
        };
        assert!(claim(&outbox, A, at(T0)).await.is_none(), "B is not ready");

        // A's blocker fails and is pushed back to the *front*, so A's batched
        // row is no longer at the head — A has stopped being ready.
        outbox
            .settle(a_head.id, Outcome::Failed("ssh died".into()), at(T0))
            .await
            .unwrap();

        // Now B clears its blocker and arrives. With an accumulated `ready`
        // set the batch would release here and claim A's *retry* as if it were
        // the batched row.
        let Some(Claim::One(b_head)) = claim(&outbox, B, at(T0)).await else {
            panic!("B's blocker should be claimable");
        };
        outbox
            .settle(b_head.id, Outcome::Succeeded(ReadValue::Flag(true)), at(T0))
            .await
            .unwrap();

        let claimed = claim(&outbox, B, at(T0)).await;
        assert!(
            claimed.is_none(),
            "A is no longer at the head, so the barrier is not full: got {claimed:?}"
        );
        assert_eq!(
            status_of(&outbox, "ahead-a").await,
            WriteStatus::Pending,
            "A's retry is what sits at its head"
        );
    }

    /// A group write needs no rendezvous — writing the same title to two
    /// recorders gains nothing from unison — so it is expanded without a batch
    /// and each member dispatches as soon as it can.
    #[tokio::test]
    async fn an_unbatched_group_expansion_dispatches_per_member() {
        let outbox = outbox();
        outbox
            .submit(Submission {
                batch: None,
                barrier: None,
                intent: Intent::SetMetadata {
                    field: "TITLE".to_owned(),
                    value: "Week 4".to_owned(),
                },
                ..group_start(Barrier::FailBatch)
            })
            .await
            .unwrap();

        for (device, id) in [(A, "cmd-a"), (B, "cmd-b")] {
            let claimed = claim(&outbox, device, at(T0)).await;
            assert!(
                matches!(&claimed, Some(Claim::One(r)) if r.id == id),
                "{device} should dispatch on its own, got {claimed:?}"
            );
        }
    }

    // ---- what the device group was told ------------------------------------------

    /// `GroupState::expected` for one field of [`GROUP`], unwrapped.
    async fn expected(outbox: &MemoryOutbox, field: &str) -> Option<GroupExpectation> {
        GroupState::expected(outbox, GROUP.to_owned(), field.to_owned())
            .await
            .expect("expected")
    }

    const RUNNING_STATE: &str = "RUNNING_STATE";

    #[tokio::test]
    async fn an_admitted_group_request_records_what_the_room_was_told() {
        let outbox = outbox();
        outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();

        assert_eq!(
            expected(&outbox, RUNNING_STATE).await,
            Some(GroupExpectation {
                field: RUNNING_STATE.to_owned(),
                value: ReadValue::State(RecordingState::Started),
                // The submission's instant, shared by every member's row.
                since: at(T0),
            })
        );
    }

    /// The property the whole port exists for: the take was abandoned, every
    /// row failed, every desired state rolled back — and the device group still
    /// records that it was asked to be recording. Without this the fleet reads
    /// as perfectly consistent and nothing anywhere says a lecture was missed.
    #[tokio::test]
    async fn an_abandoned_take_leaves_the_expectation_standing() {
        let outbox = outbox();
        outbox.submit(blocker("ahead-b", B)).await.unwrap();
        outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();
        // Past the ten-second bound: the barrier never fills, so `FailBatch`
        // fails the rows and `rollback` returns A to idle.
        claim(&outbox, A, at_plus(11)).await;
        assert_eq!(
            desired_recording_state_of(&outbox, A)
                .await
                .desired_recording_state,
            DesiredRecordingState::Idle
        );

        assert_eq!(
            expected(&outbox, RUNNING_STATE).await.map(|e| e.value),
            Some(ReadValue::State(RecordingState::Started)),
            "a failed write must not erase what the device group was asked for"
        );
    }

    /// The counterpart: a submission the admission table *refused* records
    /// nothing, so the previous expectation is what a reader still sees. A
    /// rejected start that overwrote it would claim the device group was asked
    /// for something at an instant when nothing was sent.
    #[tokio::test]
    async fn a_refused_group_request_leaves_the_previous_expectation_untouched() {
        let outbox = outbox();
        outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();

        let refused = outbox
            .submit(Submission {
                ids: vec!["cmd-c".to_owned(), "cmd-d".to_owned()],
                batch: Some("batch-2".to_owned()),
                at: at_plus(30),
                ..group_start(Barrier::FailBatch)
            })
            .await;
        assert!(refused.is_err(), "a second start should be refused");

        assert_eq!(
            expected(&outbox, RUNNING_STATE).await.map(|e| e.since),
            Some(at(T0)),
            "the refused submission must not have moved `since`"
        );
    }

    /// A device-addressed request carries no group, so it files no expectation:
    /// one device's own desired state already says what it was told, and a device
    /// group it happens to belong to was not the thing that was asked.
    #[tokio::test]
    async fn a_device_addressed_request_records_no_group_expectation() {
        let outbox = outbox();
        outbox.submit(blocker("lone", A)).await.unwrap();

        assert_eq!(
            outbox
                .expected_all(GROUP.to_owned())
                .await
                .expect("expected_all"),
            Vec::new()
        );
    }

    /// One entry per field, ordered by field name — the order the port promises
    /// and the group index route renders in.
    #[tokio::test]
    async fn expectations_accumulate_per_field_in_field_order() {
        let outbox = outbox();
        // A title first (admissible while idle), then the start.
        outbox
            .submit(Submission {
                ids: vec!["cmd-t1".to_owned(), "cmd-t2".to_owned()],
                batch: None,
                barrier: None,
                intent: Intent::SetMetadata {
                    field: "TITLE".to_owned(),
                    value: "Week 4".to_owned(),
                },
                ..group_start(Barrier::FailBatch)
            })
            .await
            .unwrap();
        outbox
            .submit(group_start(Barrier::FailBatch))
            .await
            .unwrap();

        let all = outbox
            .expected_all(GROUP.to_owned())
            .await
            .expect("expected_all");
        assert_eq!(
            all.iter().map(|e| e.field.as_str()).collect::<Vec<_>>(),
            [RUNNING_STATE, "TITLE"]
        );
        // A write carries the caller's text unchanged; reconciling it with the
        // device's decode is `sismatic_store::group::satisfies`' job, not this
        // adapter's.
        assert_eq!(all[1].value, ReadValue::Text("Week 4".to_owned()));
    }

    /// An idempotent replay produced no new writes, so it must not report
    /// the device group as freshly told.
    #[tokio::test]
    async fn an_idempotent_replay_does_not_move_the_expectation() {
        let outbox = outbox();
        let keyed = |at_: Timestamp| Submission {
            idempotency_key: Some("retry-me".to_owned()),
            at: at_,
            ..group_start(Barrier::FailBatch)
        };
        outbox.submit(keyed(at(T0))).await.unwrap();
        outbox.submit(keyed(at_plus(30))).await.unwrap();

        assert_eq!(
            expected(&outbox, RUNNING_STATE).await.map(|e| e.since),
            Some(at(T0))
        );
    }
}
