//! The write side: poll each device on a schedule and persist the results.
//!
//! [`spawn`] is the composition root's entry point. It mirrors [`tokio::spawn`]'s
//! contract — it must be called from *within* a Tokio runtime, it returns
//! immediately, and it hands back a [`SyncHandle`] the caller holds so it can run
//! the read-side http-api concurrently and shut the loops down cleanly.
//!
//! # Task shape
//!
//! One task per `(device, field)`, collected in a [`JoinSet`]. That granularity
//! is deliberate: a wedged SSH session polling one field must not stall the
//! others, and a panic in one loop surfaces at the root rather than silently
//! taking the fleet down. It is also what lets each field keep its own clock —
//! a loop owns its ticker, so `RUNNING_STATE` every five seconds and `FIRMWARE`
//! every hour are the same code path with different [`FieldSchedule`]s.
//!
//! # Error discipline
//!
//! Inside a loop, a failed poll (device unreachable, timeout) or a failed write
//! is the *steady state*, not an exception — it is logged and the loop ticks
//! again. The **only** thing that ends a loop is an unknown field name, a
//! configuration error that cannot fix itself by retrying. No `Result` ever
//! escapes a task.
//!
//! # What a failing poll says, and how often
//!
//! A poll loop reports *changes*, not attempts. Left to log every failed tick at
//! `warn!`, one unreachable device would emit a line per field per tick — with a
//! wildcard schedule, dozens of identical lines every few seconds, for as long as
//! the outage lasts. The volume would then be a function of how fast we poll,
//! which is a number chosen for data freshness and has nothing to say about how
//! interesting the failure is.
//!
//! So each loop carries a two-state [`Health`], and [`step`] — a pure function
//! over `Health × Contact` — decides what to emit. Only the transitions are
//! operator-visible: `warn!` when a field stops being readable, `info!` when it
//! becomes readable again, `debug!` in between. Log volume then scales with the
//! number of times the fleet *changes state*, not with the tick rate.
//!
//! [`DeviceError::Cold`] is folded into the same machine as its own [`Contact`]
//! case rather than as an ordinary failure. It means core declined to dial
//! because a recent dial already failed, so it is not independent evidence — the
//! loop that made that dial has already warned. Treating it as a non-announcing
//! transition is what takes an outage from one warning per `(device, field)` down
//! to one per device.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use sismatic_api_types::{Reading, Timestamp};
use sismatic_core::devices::device::{Device, DeviceError};
use sismatic_core::devices::registry::Registry;
use sismatic_core::protocol::Value;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_store::DynWriteStore;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::dto;

/// What to poll and how often.
///
/// There is deliberately no fleet-wide `interval` here: every entry of `fields`
/// already carries its own. A default plus a set of overrides is a *config file*
/// shape, and folding those layers is the config layer's job (see
/// `sismatic_server::configuration::resolve_config`, a pure function that is
/// unit-tested over values). By the time a schedule reaches the driver the
/// precedence question is settled, so this crate never has to answer "which
/// interval applies" — it only reads one off each field.
#[derive(Debug)]
pub struct SyncConfig {
    /// One entry per field to poll on every device.
    pub fields: Vec<FieldSchedule>,
}

/// One field's polling schedule: what to ask for, and how often to ask.
///
/// `name` is a canonical query name as [`Query`] spells it (e.g.
/// `"RUNNING_STATE"`) — the same string that lands in [`Reading::field`], which
/// is why it need not be mirrored as a typed enum here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSchedule {
    /// Canonical query name, e.g. `"RUNNING_STATE"`.
    pub name: String,
    /// Delay between polls of this field on a given device. `None` means never:
    /// the field stays listed, but no poll loop is started for it. This is the
    /// same "unset means never" shape core uses for `sis_keepalive` and
    /// `eager_retry`, and it is why a zero delay is unrepresentable here —
    /// `tokio::time::interval` panics on one, so the type rules it out rather
    /// than a runtime check having to catch it.
    pub interval: Option<Duration>,
}

/// Owns the running poll tasks. Call [`SyncHandle::shutdown`] (or drop it) to
/// stop them.
pub struct SyncHandle {
    tasks: JoinSet<()>,
    cancel: CancellationToken,
}

impl SyncHandle {
    /// Signal every loop to stop and wait for the in-flight polls to drain.
    ///
    /// Cancellation is cooperative: a loop finishes its current SSH exchange
    /// before exiting, rather than being aborted mid-command.
    ///
    /// Instrumented here rather than at the call site because this is where the
    /// two facts about a drain are known: how many loops are being waited on
    /// (the span's `tasks`, recorded when the span opens — a `JoinSet` that has
    /// been drained can no longer say), and how long the wait took, which the
    /// formatter derives from the span's close. Since cancellation is
    /// cooperative, that duration is bounded below by the slowest in-flight SSH
    /// exchange, which is exactly the thing worth watching.
    ///
    /// `skip(self)` is required, not stylistic: `#[instrument]` records every
    /// argument including the receiver, and [`SyncHandle`] is not `Debug`.
    #[instrument(name = "sync_shutdown", skip(self), fields(tasks = self.tasks.len()))]
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        while self.tasks.join_next().await.is_some() {}
        info!("sync driver stopped");
    }
}

/// Start one poll loop per `(device, field)` and return a handle to them.
///
/// Must be called from within a Tokio runtime (it uses [`tokio::spawn`]).
pub fn spawn(registry: Arc<Registry>, write: DynWriteStore, cfg: SyncConfig) -> SyncHandle {
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();

    // Announced once per field rather than once per (device, field): a disabled
    // field is a property of the config, and repeating it per device would say
    // the same thing as many times as there are devices.
    for field in cfg.fields.iter().filter(|f| f.interval.is_none()) {
        info!(
            field = field.name,
            "polling disabled for this field; no loop started"
        );
    }

    for device in registry.devices() {
        for field in &cfg.fields {
            // No task at all, rather than a task that never ticks, so the count
            // logged below stays the number of loops actually running.
            let Some(interval) = field.interval else {
                continue;
            };
            tasks.spawn(poll_loop(
                device.clone(),
                field.name.clone(),
                write.clone(),
                interval,
                cancel.clone(),
            ));
        }
    }

    info!(tasks = tasks.len(), "sync driver started");
    SyncHandle { tasks, cancel }
}

/// Poll one field on one device forever, persisting each reading, until
/// cancelled.
async fn poll_loop(
    device: Arc<Device>,
    field: String,
    write: DynWriteStore,
    interval: Duration,
    cancel: CancellationToken,
) {
    // A bad field name cannot fix itself by retrying — log and never start.
    let query = match Query::from_str(&field) {
        Ok(query) => query,
        Err(_) => {
            warn!(
                device = device.id(),
                field, "unknown query field; poll loop not started"
            );
            return;
        }
    };
    let instruction = query.instruction();

    let mut ticker = tokio::time::interval(interval);
    // Re-pace from completion so a slow device does not trigger a burst of
    // catch-up ticks the moment it recovers.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    // Optimistic on purpose: starting at `Down` would make a fleet that is up
    // announce a recovery per field at startup, and starting at `Up` makes the
    // first failed poll read as the onset it is.
    let mut health = Health::Up;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                let outcome = device.run(&instruction).await;

                // The whole logging decision, taken by a pure function over
                // (previous state, this poll) before anything is emitted.
                let (next, report) = step(health, Contact::of(&outcome));
                health = next;
                announce(report, device.id(), &field, outcome.as_ref().err());

                if let Ok(value) = outcome {
                    let reading = Reading {
                        device: device.id().to_string(),
                        field: field.clone(),
                        value: dto::to_dto(value),
                        at: Timestamp(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
                    };
                    // Not part of `health`: a store that will not take a reading
                    // says nothing about whether the device answered.
                    if let Err(err) = write.upsert_latest(reading).await {
                        warn!(device = device.id(), field, %err, "failed to persist reading");
                    }
                }
            }
        }
    }

    info!(device = device.id(), field, "poll loop stopped");
}

/// What one loop believes about its `(device, field)` pair right now. Two states,
/// because an operator only ever asks one question of a poll loop: is this
/// reading current, or stale?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Up,
    Down,
}

/// A poll result reduced to the three cases the health machine distinguishes.
///
/// The distinction that matters is [`Gated`](Contact::Gated) versus
/// [`Failed`](Contact::Failed): both mean "no reading", but only `Failed` is
/// *news*. A gated poll is core reporting a fact some other loop's dial already
/// established (and already reported), so treating the two alike is what would
/// put the onset in the log once per field instead of once per device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Contact {
    /// The device answered.
    Reached,
    /// We tried to reach it and could not.
    Failed,
    /// We did not try: core's cold gate is shut. See [`DeviceError::Cold`].
    Gated,
}

impl Contact {
    /// Classify a poll result. Total and pure — every `DeviceError` lands in
    /// exactly one case, so the machine below never needs a fallback arm.
    fn of(outcome: &Result<Value, DeviceError>) -> Self {
        match outcome {
            Ok(_) => Contact::Reached,
            Err(DeviceError::Cold { .. }) => Contact::Gated,
            Err(DeviceError::Connect(_) | DeviceError::Command(_)) => Contact::Failed,
        }
    }
}

/// What, if anything, this poll is worth saying out loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Report {
    /// Nothing changed and nothing failed — the steady state of a healthy fleet,
    /// which must not cost a log line per field per tick.
    Silent,
    /// This loop just stopped being able to read its field.
    Onset,
    /// It still cannot, which is the same news as last tick.
    Ongoing,
    /// It can again.
    Recovery,
}

/// The state machine, as one total function over `Health × Contact`.
///
/// Six cases, all written out, so the policy is readable as a table rather than
/// inferred from control flow — and testable without a device, a clock, a task,
/// or a log subscriber, since it touches none of them.
///
/// The one non-obvious entry is `(Up, Gated)`: it moves to `Down` (the field
/// *is* unreadable) but reports `Ongoing` rather than `Onset`, because a shut
/// gate is downstream of a failed dial that some other loop has already
/// announced. That is what collapses an outage from one warning per
/// `(device, field)` to one per device.
const fn step(before: Health, contact: Contact) -> (Health, Report) {
    match (before, contact) {
        (Health::Up, Contact::Reached) => (Health::Up, Report::Silent),
        (Health::Up, Contact::Failed) => (Health::Down, Report::Onset),
        (Health::Up, Contact::Gated) => (Health::Down, Report::Ongoing),
        (Health::Down, Contact::Reached) => (Health::Up, Report::Recovery),
        (Health::Down, Contact::Failed) => (Health::Down, Report::Ongoing),
        (Health::Down, Contact::Gated) => (Health::Down, Report::Ongoing),
    }
}

/// Emit `report`. The only effectful half of the pair: [`step`] decides, this
/// speaks, and keeping them apart is what lets the decision be unit-tested.
///
/// Levels follow how often each report can possibly fire. `Onset` and `Recovery`
/// are bounded by the number of times a device changes state, so they are
/// operator-visible; `Ongoing` is bounded only by the tick rate, so it is not.
fn announce(report: Report, device: &str, field: &str, error: Option<&DeviceError>) {
    let error = error.map(tracing::field::display);
    match report {
        Report::Silent => {}
        Report::Onset => warn!(device, field, error, "polling this field started failing"),
        Report::Ongoing => debug!(device, field, error, "polling this field is still failing"),
        Report::Recovery => info!(device, field, "polling this field recovered"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sismatic_api_types::Reading;
    use sismatic_core::devices::config::DeviceConfig;
    use sismatic_core::devices::connector::fake::CountingConnector;
    use sismatic_core::devices::connector::{ConnectError, Connector};
    use sismatic_core::devices::transport::Transport;
    use sismatic_core::devices::transport::fake::FakeTransport;
    use sismatic_store::{WriteError, WriteStore};

    use super::*;

    /// What a `FIRMWARE` query gets back; the cheapest reply to script.
    const FIRMWARE_REPLY: &str = "2.11\r\n";

    /// A `WriteStore` that just records what reached it, which is the only
    /// evidence a poll loop ran at all.
    #[derive(Default)]
    struct RecordingStore {
        fields: Mutex<Vec<String>>,
    }

    impl RecordingStore {
        fn fields(&self) -> Vec<String> {
            self.fields.lock().expect("lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl WriteStore for RecordingStore {
        async fn upsert_latest(&self, reading: Reading) -> Result<(), WriteError> {
            self.fields.lock().expect("lock").push(reading.field);
            Ok(())
        }
    }

    fn device_config(id: &str) -> DeviceConfig {
        DeviceConfig {
            id: id.into(),
            host: "10.0.0.1".into(),
            port: 22023,
            username: "admin".into(),
            password: "extron".into(),
            connect_timeout: Duration::from_millis(500),
            command_timeout: Duration::from_millis(500),
            eager: false,
            sis_keepalive: None,
            eager_retry: None,
            cold_backoff: None,
        }
    }

    /// A registry of one device whose every connection replays firmware replies.
    fn registry_of_one() -> (Arc<Registry>, Arc<AtomicUsize>) {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([FIRMWARE_REPLY; 8])
        }));
        let opens = connector.opens_handle();
        let registry = Registry::build(vec![device_config("fixture")], vec![], connector);
        (Arc::new(registry), opens)
    }

    /// Poll `cond` until it holds, or panic after ~2s, so a spawned loop can make
    /// progress without the test racing on a fixed sleep.
    async fn wait_for(cond: impl Fn() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not met in time");
    }

    #[tokio::test]
    async fn a_field_with_no_interval_starts_no_poll_loop() {
        let (registry, _opens) = registry_of_one();
        let store = Arc::new(RecordingStore::default());

        // The disabled field is listed first, so a bug that ignored `None` would
        // show up as its name reaching the store before the enabled one's.
        let sync = spawn(
            registry,
            store.clone(),
            SyncConfig {
                fields: vec![
                    FieldSchedule {
                        name: "UNIT_NAME".to_owned(),
                        interval: None,
                    },
                    FieldSchedule {
                        name: "FIRMWARE".to_owned(),
                        interval: Some(Duration::from_millis(10)),
                    },
                ],
            },
        );

        // Wait for the enabled field to prove the driver is running at all, then
        // assert the disabled one never appears alongside it.
        wait_for(|| !store.fields().is_empty()).await;
        sync.shutdown().await;

        let seen = store.fields();
        assert!(
            seen.iter().all(|f| f == "FIRMWARE"),
            "a disabled field must never be polled, got: {seen:?}"
        );
    }

    #[tokio::test]
    async fn a_fleet_with_every_field_disabled_starts_nothing_and_still_shuts_down() {
        let (registry, opens) = registry_of_one();
        let store = Arc::new(RecordingStore::default());

        let sync = spawn(
            registry,
            store.clone(),
            SyncConfig {
                fields: vec![FieldSchedule {
                    name: "FIRMWARE".to_owned(),
                    interval: None,
                }],
            },
        );

        // No loop means no connection is ever opened — the device is left as
        // untouched as if it had not been listed.
        tokio::time::sleep(Duration::from_millis(50)).await;
        sync.shutdown().await;

        assert_eq!(opens.load(Ordering::SeqCst), 0);
        assert!(store.fields().is_empty());
    }

    // ---- the health machine ----------------------------------------------
    //
    // `step` is a total function over a six-element domain, so these tests state
    // the whole table rather than sampling it. No device, no clock, no task, no
    // log subscriber — the point of splitting the decision out of `announce`.

    /// Replay a run of polls through the machine, returning what each one
    /// reported. Starts at [`Health::Up`], exactly as [`poll_loop`] does.
    fn reports(run: impl IntoIterator<Item = Contact>) -> Vec<Report> {
        let mut health = Health::Up;
        run.into_iter()
            .map(|contact| {
                let (next, report) = step(health, contact);
                health = next;
                report
            })
            .collect()
    }

    #[test]
    fn the_machine_is_a_table() {
        use Contact::*;
        use Health::*;
        use Report::*;

        assert_eq!(step(Up, Reached), (Up, Silent));
        assert_eq!(step(Up, Failed), (Down, Onset));
        assert_eq!(step(Up, Gated), (Down, Ongoing));
        assert_eq!(step(Down, Reached), (Up, Recovery));
        assert_eq!(step(Down, Failed), (Down, Ongoing));
        assert_eq!(step(Down, Gated), (Down, Ongoing));
    }

    #[test]
    fn a_healthy_fleet_is_silent() {
        assert!(
            reports([Contact::Reached; 100])
                .iter()
                .all(|r| *r == Report::Silent)
        );
    }

    #[test]
    fn an_outage_costs_one_onset_however_long_it_lasts() {
        let mut run = vec![Contact::Reached, Contact::Failed];
        run.extend([Contact::Failed; 500]);

        let onsets = reports(run).iter().filter(|r| **r == Report::Onset).count();
        assert_eq!(onsets, 1, "log volume must not scale with outage duration");
    }

    #[test]
    fn a_gated_poll_never_announces_an_onset() {
        // What the other 36 field loops on an unreachable device see: they never
        // dialed, so they have nothing to report that the one that did has not.
        let reported = reports([Contact::Gated; 50]);
        assert!(
            reported.iter().all(|r| *r == Report::Ongoing),
            "a shut gate is not independent evidence: {reported:?}"
        );
    }

    #[test]
    fn recovery_is_announced_once_and_the_machine_rearms() {
        use Contact::*;
        use Report::*;

        // Down, back up, down again: the second outage must warn afresh rather
        // than be swallowed by the first.
        assert_eq!(
            reports([Reached, Failed, Failed, Reached, Reached, Failed]),
            vec![Silent, Onset, Ongoing, Recovery, Silent, Onset]
        );
    }

    #[test]
    fn the_first_poll_of_a_down_device_reads_as_an_onset() {
        // The startup case: no prior tick to transition from, and the optimistic
        // initial state is what makes it report rather than pass silently.
        assert_eq!(reports([Contact::Failed]), vec![Report::Onset]);
    }

    #[test]
    fn a_cold_error_classifies_as_gated_and_the_others_as_failed() {
        // Ties the machine to the real error type: a new `DeviceError` variant
        // fails to compile in `Contact::of` rather than defaulting to a case.
        assert_eq!(
            Contact::of(&Err(DeviceError::Cold {
                retry_in: Duration::from_secs(1)
            })),
            Contact::Gated
        );
        assert_eq!(
            Contact::of(&Err(DeviceError::Connect(ConnectError::Failed(
                "refused".into()
            )))),
            Contact::Failed
        );
        assert_eq!(Contact::of(&Ok(Value::Port(22023))), Contact::Reached);
    }

    /// A connector that refuses every dial, counting the attempts.
    struct RefusingConnector {
        attempts: Arc<AtomicUsize>,
    }

    impl RefusingConnector {
        fn new() -> Self {
            Self {
                attempts: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn attempts_handle(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.attempts)
        }
    }

    #[async_trait::async_trait]
    impl Connector for RefusingConnector {
        async fn connect(
            &self,
            _config: &DeviceConfig,
        ) -> Result<Box<dyn Transport>, ConnectError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(ConnectError::Failed("down".into()))
        }
    }

    #[tokio::test]
    async fn an_unreachable_device_is_dialed_once_per_window_not_once_per_field_per_tick() {
        // The startup case this exists for: several fields on one device that is
        // not answering. Each loop is free to tick as fast as it likes, but the
        // device's cold gate means only one of those ticks becomes a dial.
        let connector = Arc::new(RefusingConnector::new());
        let attempts = connector.attempts_handle();
        let mut config = device_config("unreachable");
        config.cold_backoff = Some(Duration::from_secs(3600));
        let registry = Arc::new(Registry::build(vec![config], vec![], connector));
        let store = Arc::new(RecordingStore::default());

        let sync = spawn(
            registry,
            store.clone(),
            SyncConfig {
                fields: ["FIRMWARE", "UNIT_NAME", "MODEL_NAME", "TIMEZONE"]
                    .into_iter()
                    .map(|name| FieldSchedule {
                        name: name.to_owned(),
                        interval: Some(Duration::from_millis(5)),
                    })
                    .collect(),
            },
        );

        // Four loops ticking every 5ms for ~150ms: without the gate this is on the
        // order of a hundred dials, each paying a connect timeout in production.
        tokio::time::sleep(Duration::from_millis(150)).await;
        sync.shutdown().await;

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "the whole fleet-poll should cost one dial per backoff window"
        );
        assert!(store.fields().is_empty(), "nothing was readable to persist");
    }
}
