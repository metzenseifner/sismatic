//! One relay task per device, draining that device's outbox queue in order.
//!
//! # Task shape
//!
//! One task per *device*, where `sismatic-sync` runs one per `(device, field)`.
//! The two grains answer different questions. Sync splits by field so a wedged
//! poll of one field cannot stall another. The relay must not split, because
//! per-device FIFO order is a correctness requirement: a `SetMetadata` and the
//! `StartRecording` that follows it have to reach the device in the order they
//! were submitted, and two tasks on one device would race for the device's
//! command lock in whatever order the scheduler chose.
//!
//! # Error discipline
//!
//! Unlike a poll loop, a relay loop reports every failure at `warn!`. The sync
//! driver suppresses repeats because its log volume scales with the tick rate,
//! a number chosen for data freshness. A relay's volume scales with how many
//! commands an operator submitted, so every failure is a distinct event with a
//! distinct command id and is worth a line.

use std::sync::Arc;
use std::time::Duration;

use std::collections::BTreeMap;

use chrono::{SecondsFormat, Utc};
use sismatic_api_types::{BatchId, CommandId, CommandRecord, Intent, ReadingValue, Timestamp};
use sismatic_core::devices::device::Device;
use sismatic_core::devices::group::DeviceGroup;
use sismatic_core::devices::registry::Registry;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_core::protocol::{RecordingState, Value};
use sismatic_store::outbox::{Claim, CommandDrain, DynCommandDrain, Outcome};
use sismatic_sync::dto;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument, warn};

use crate::translate;

/// How often an idle relay task looks for work.
///
/// A plain interval rather than a notification, because the port is a trait and
/// a database adapter cannot promise one. A `tokio::sync::Notify` fast path can
/// be added later without changing the port.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub poll: Duration,
}

pub struct RelayHandle {
    tasks: JoinSet<()>,
    cancel: CancellationToken,
}

impl RelayHandle {
    /// Signal every task to stop and wait for in-flight exchanges to finish.
    /// Cooperative, as `SyncHandle::shutdown` is: a claimed command
    /// completes its SIS exchange rather than being abandoned half-settled.
    #[instrument(name = "intent_relay_shutdown", skip(self), fields(tasks = self.tasks.len()))]
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        while self.tasks.join_next().await.is_some() {}
        info!("intent relay stopped");
    }
}

pub fn spawn(registry: Arc<Registry>, drain: DynCommandDrain, cfg: RelayConfig) -> RelayHandle {
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();
    for device in registry.devices() {
        tasks.spawn(relay_loop(
            device,
            Arc::clone(&registry),
            drain.clone(),
            cfg.poll,
            cancel.clone(),
        ));
    }
    info!(tasks = tasks.len(), "intent relay started");
    RelayHandle { tasks, cancel }
}

#[instrument(name = "intent_relay", skip_all, fields(device = %device.id()))]
async fn relay_loop(
    device: Arc<Device>,
    registry: Arc<Registry>,
    drain: DynCommandDrain,
    poll: Duration,
    cancel: CancellationToken,
) {
    recover(&device, drain.as_ref()).await;

    let mut ticker = tokio::time::interval(poll);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                // Drain the whole queue rather than one per tick, so a burst of
                // six metadata writes plus a start does not take seven ticks.
                while !cancel.is_cancelled()
                    && dispatch_one(&device, &registry, drain.as_ref()).await {}
            }
        }
    }
    info!("relay loop stopped");
}

/// Claim whatever is next for this device, run it, settle it. Returns whether
/// there was work, so the caller knows whether to look again immediately.
async fn dispatch_one(device: &Device, registry: &Registry, drain: &dyn CommandDrain) -> bool {
    let Ok(Some(claim)) = drain.claim_next(device.id().to_string(), now()).await else {
        return false;
    };

    match claim {
        Claim::One(record) => dispatch_alone(device, drain, record).await,
        // This task completed the barrier, so it owns the whole batch. Every
        // other member's loop found its row already claimed and moved on.
        Claim::Batch { id, records } => dispatch_batch(registry, drain, &id, records).await,
    }
    true
}

/// One command against one device.
async fn dispatch_alone(device: &Device, drain: &dyn CommandDrain, record: CommandRecord) {
    // The one place a stale phase can still do damage: a recording started from
    // the front panel between admission and now. Re-read the device before a
    // metadata write, and only before a metadata write — every other intent is
    // admissible in every phase or is itself a phase change.
    if matches!(record.intent, Intent::SetMetadata { .. })
        && let Some(state) = observe_state(device, drain).await
        && state.is_recording()
    {
        let reason = "a recording was in progress when this write reached the device";
        let _ = drain
            .settle(record.id, Outcome::Failed(reason.into()), now())
            .await;
        return;
    }

    let outcome = match translate::to_instruction(&record.intent) {
        Err(err) => Outcome::Failed(err.to_string()),
        Ok(instruction) => match device.run(&instruction).await {
            Ok(value) => Outcome::Succeeded(dto::to_dto(value)),
            Err(err) => Outcome::Failed(err.to_string()),
        },
    };
    report(drain, record.id, outcome).await;
}

/// Every member of a batch, as one group run.
///
/// This is the whole point of the rendezvous. [`DeviceGroup::run`] spawns every
/// member's exchange before awaiting any, so the members act *in unison* — which
/// is a stronger property than "each was dispatched at about the same time", and
/// the one a device group needs. Running the members from one task,
/// through one group, is what makes it true; N tasks each running their own row
/// would go out in whatever order the scheduler chose.
///
/// The group is built ad hoc from the records rather than looked up by id,
/// because the batch is not the group: a group can be addressed many times, and
/// a batch may be a *subset* of one when `DispatchReady` fired on a partially
/// arrived device group. What must be dispatched is exactly the rows that were
/// claimed.
async fn dispatch_batch(
    registry: &Registry,
    drain: &dyn CommandDrain,
    batch: &BatchId,
    records: Vec<CommandRecord>,
) {
    let Some(first) = records.first() else {
        return;
    };

    // Translated once: every row of a batch carries the same intent, because
    // they were expanded from one request. A per-record translation would be
    // the same work N times and would leave open the question of what to do if
    // two of them disagreed.
    let instruction = match translate::to_instruction(&first.intent) {
        Ok(instruction) => instruction,
        Err(err) => {
            warn!(%batch, %err, "a batch could not be translated");
            settle_all(drain, &records, |_| Outcome::Failed(err.to_string())).await;
            return;
        }
    };

    let mut members = Vec::with_capacity(records.len());
    for record in &records {
        match registry.device(&record.device) {
            Some(device) => members.push(device),
            None => {
                // The catalog admitted a member the registry does not hold, so
                // the two disagree about the device set. Failing the batch is
                // the only honest answer: dispatching the rest would start
                // part of a device group under a policy that may well have been
                // `FailBatch`.
                let reason = format!("member '{}' is not in the registry", record.device);
                warn!(%batch, reason, "a batch names a device the registry does not hold");
                settle_all(drain, &records, |_| Outcome::Failed(reason.clone())).await;
                return;
            }
        }
    }

    let group = DeviceGroup::new(batch.clone(), members);
    match group.run(&instruction).await {
        Ok(values) => {
            let by_device: BTreeMap<&str, &Value> =
                values.iter().map(|(id, v)| (id.as_str(), v)).collect();
            settle_all(drain, &records, |record| {
                match by_device.get(record.device.as_str()) {
                    Some(value) => Outcome::Succeeded(dto::to_dto((*value).clone())),
                    // `run` promises one value per member on success, so this
                    // is unreachable rather than a case with a right answer.
                    None => Outcome::Failed("the group returned no value for this member".into()),
                }
            })
            .await;
        }
        Err(error) => {
            // A partial failure is surfaced per member, not collapsed: the
            // members that succeeded *did* run, and reporting them as failed
            // would tell an operator to restart a recorder that is already
            // recording.
            warn!(%batch, %error, "a group command failed for at least one member");
            let failed: BTreeMap<&str, String> = error
                .failures
                .iter()
                .map(|(id, e)| (id.as_str(), e.to_string()))
                .collect();
            settle_all(drain, &records, |record| {
                match failed.get(record.device.as_str()) {
                    Some(reason) => Outcome::Failed(reason.clone()),
                    None => Outcome::Succeeded(ReadingValue::Ack(format!(
                        "{} accepted as part of batch {batch}",
                        record.device
                    ))),
                }
            })
            .await;
        }
    }
}

/// Settle every row of a batch, deciding each one's outcome from the record.
async fn settle_all(
    drain: &dyn CommandDrain,
    records: &[CommandRecord],
    outcome_for: impl Fn(&CommandRecord) -> Outcome,
) {
    for record in records {
        report(drain, record.id.clone(), outcome_for(record)).await;
    }
}

/// Record one outcome, warning about the failure and about a failure to record
/// it — two different problems that would otherwise share a line.
async fn report(drain: &dyn CommandDrain, id: CommandId, outcome: Outcome) {
    if let Outcome::Failed(ref reason) = outcome {
        warn!(command = %id, reason, "command failed");
    }
    if let Err(err) = drain.settle(id, outcome, now()).await {
        warn!(%err, "could not record the outcome of a command");
    }
}

/// Decide what to do with commands a previous process left `InFlight`.
///
/// Delivery to a device is at-least-once: a process that dies after
/// [`Device::run`] returns and before `settle` lands leaves a command claimed
/// with the device already changed. This narrows that to at-most-once *effect*
/// for the three intents where a repeat is not harmless.
///
/// Metadata and setting writes are replayed, because writing the same value
/// twice is the same as writing it once. A lifecycle command is *not* replayed
/// blindly: the device is asked what it is doing, and the answer settles the
/// command without a second attempt.
///
/// A replay is spelled as [`Outcome::Failed`] rather than a requeue method of
/// its own. That is not a lie about what happened — the attempt genuinely did
/// not complete — and the port already says a failed outcome is "retryable if
/// `attempts < max_attempts`; the adapter decides". So the adapter puts the
/// record back at the *front* of the device's queue, which preserves FIFO, and
/// spends one unit of the retry budget rather than looping across restarts
/// forever. A command whose budget is exhausted lands in the dead-letter state
/// instead, where an operator can see it.
async fn recover(device: &Device, drain: &dyn CommandDrain) {
    let Ok(stranded) = drain.in_flight(device.id().to_string()).await else {
        warn!("could not read the commands left in flight by a previous process");
        return;
    };
    if stranded.is_empty() {
        return;
    }
    warn!(
        stranded = stranded.len(),
        "commands were left in flight by a previous process"
    );

    // One exchange for the whole batch: every decision below reads the same
    // state, and asking once per record would be the same question repeated.
    let observed = observe_state(device, drain).await;

    for record in stranded {
        let outcome = match (&record.intent, observed) {
            (Intent::SetMetadata { .. } | Intent::SetSetting { .. }, _) => Outcome::Failed(
                "left in flight by a previous process; replayed, because writing the same \
                 value twice is the same as writing it once"
                    .into(),
            ),
            // Undecidable, so it is reported rather than guessed at. An
            // operator can resubmit; a second start against a device whose
            // behaviour on a duplicate is unattested cannot be taken back.
            (_, None) => Outcome::Failed(
                "left in flight by a previous process and the device did not answer".into(),
            ),
            // The effect the command asked for exists, so the command is done.
            // `is_recording` is true of `Paused` as well as `Started`: a start
            // that landed and was then paused still landed.
            (Intent::StartRecording, Some(state)) if state.is_recording() => {
                Outcome::Succeeded(ReadingValue::State(dto::state_to_dto(state)))
            }
            (Intent::StopRecording, Some(state @ RecordingState::Stopped)) => {
                Outcome::Succeeded(ReadingValue::State(dto::state_to_dto(state)))
            }
            (Intent::PauseRecording, Some(state @ RecordingState::Paused)) => {
                Outcome::Succeeded(ReadingValue::State(dto::state_to_dto(state)))
            }
            (_, Some(_)) => Outcome::Failed(
                "left in flight by a previous process; the device state does not show the \
                 command took effect"
                    .into(),
            ),
        };
        if let Err(err) = drain.settle(record.id, outcome, now()).await {
            warn!(%err, "could not settle a command left in flight");
        }
    }
}

/// Ask the device what it is doing and fold the answer into the phase.
async fn observe_state(device: &Device, drain: &dyn CommandDrain) -> Option<RecordingState> {
    let value = device.run(&Query::RunningState.instruction()).await.ok()?;
    let state = value.as_state()?;
    let _ = drain
        .observe(device.id().to_string(), dto::state_to_dto(state))
        .await;
    Some(state)
}

fn now() -> Timestamp {
    Timestamp(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true))
}
