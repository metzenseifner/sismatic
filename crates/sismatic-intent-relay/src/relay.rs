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

use chrono::{SecondsFormat, Utc};
use sismatic_api_types::{Intent, ReadingValue, Timestamp};
use sismatic_core::devices::device::Device;
use sismatic_core::devices::registry::Registry;
use sismatic_core::protocol::RecordingState;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_store::outbox::{CommandDrain, DynCommandDrain, Outcome};
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
        tasks.spawn(relay_loop(device, drain.clone(), cfg.poll, cancel.clone()));
    }
    info!(tasks = tasks.len(), "intent relay started");
    RelayHandle { tasks, cancel }
}

#[instrument(name = "intent_relay", skip_all, fields(device = %device.id()))]
async fn relay_loop(
    device: Arc<Device>,
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
                while !cancel.is_cancelled() && dispatch_one(&device, drain.as_ref()).await {}
            }
        }
    }
    info!("relay loop stopped");
}

/// Claim one command, run it, settle it. Returns whether there was work, so the
/// caller knows whether to look again immediately.
async fn dispatch_one(device: &Device, drain: &dyn CommandDrain) -> bool {
    let Ok(Some(record)) = drain.claim_next(device.id().to_string(), now()).await else {
        return false;
    };

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
        return true;
    }

    let outcome = match translate::to_instruction(&record.intent) {
        Err(err) => Outcome::Failed(err.to_string()),
        Ok(instruction) => match device.run(&instruction).await {
            Ok(value) => Outcome::Succeeded(dto::to_dto(value)),
            Err(err) => Outcome::Failed(err.to_string()),
        },
    };

    if let Outcome::Failed(ref reason) = outcome {
        warn!(command = %record.id, reason, "command failed");
    }
    if let Err(err) = drain.settle(record.id, outcome, now()).await {
        warn!(%err, "could not record the outcome of a command");
    }
    true
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
