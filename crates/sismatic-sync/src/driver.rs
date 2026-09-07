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
//!
//! # A schedule that changes under the loops
//!
//! [`SyncConfig::fields`] is a [`watch::Receiver`], not a `Vec`. The driver
//! therefore has a task the old one did not: a **supervisor**, which owns the
//! loops and is the only thing that starts or stops one. Every published
//! schedule is compared against what is running, field by field, and the
//! difference is applied — new fields get loops, dropped fields lose theirs, and
//! a field whose interval moved is stopped and restarted on the new clock.
//!
//! Re-timing is a restart rather than a message to a running loop, and that is
//! the design rather than a shortcut. A loop's ticker is owned by the loop; the
//! alternative is every loop selecting on a channel it will hear from a handful
//! of times in a year, and paying for that on every tick of every field of every
//! device. What a restart costs is one poll's worth of phase — the new loop's
//! first tick fires immediately — and what it buys is that the hot path stays a
//! ticker and an SSH exchange.
//!
//! Cancellation is per field, through a token that is a child of the driver's
//! own, so stopping one field cannot outlive it and stopping the driver stops
//! them all. Both are cooperative: a loop being re-timed finishes its current
//! exchange under the old interval before it goes.
//!
//! A deployment with nothing to say still fits: [`fixed`] is a schedule with no
//! sender behind it, and a supervisor that finds the sender gone stops listening
//! and keeps polling what it has.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use sismatic_api_types::{Read, Timestamp};
use sismatic_core::devices::device::{Device, DeviceError};
use sismatic_core::devices::registry::Registry;
use sismatic_core::protocol::Value;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_store::DynWriteStore;
use sismatic_store::outbox::DynWriteDrain;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
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
pub struct SyncConfig {
    /// One entry per field to poll on every device, and where a revised list
    /// arrives when the deployment changes its mind.
    ///
    /// A channel rather than a `Vec` because the schedule is the one thing about
    /// this driver an operator changes while it runs — `PATCH /v1/config`, or a
    /// reloaded ConfigMap — and the alternative to a channel is a restart of the
    /// process, which drops every SSH session in the fleet to re-time one field.
    ///
    /// It is a [`watch`] specifically, and the two properties that matters for
    /// are the ones a schedule wants: a receiver reads the *latest* value rather
    /// than a backlog of superseded ones, and a publisher never blocks on a
    /// supervisor that is busy. A deployment with a fixed schedule passes
    /// [`fixed`].
    pub fields: watch::Receiver<Vec<FieldSchedule>>,
    /// Where to report an observed recording state, if anything is listening.
    ///
    /// `None` is the shape every consumer had before the write side existed:
    /// `sismatic-cli`, the driver's own tests, and any deployment running the
    /// read side alone. The port is optional rather than a second `spawn`
    /// because one poll of `RUNNING_STATE` serves both readers, and polling it
    /// twice would double the exchanges on the field polled most often.
    pub reconciler: Option<DynWriteDrain>,
}

/// Hand-written rather than derived because `reconciler` is a trait object, and
/// a port has no `Debug` output worth printing — requiring one would push the
/// bound onto every implementor and test double for a line nobody reads. What a
/// reader of a config dump wants to know is whether a reconciler is wired at
/// all, so that is the one bit reported.
impl std::fmt::Debug for SyncConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncConfig")
            .field("fields", &*self.fields.borrow())
            .field("reconciler", &self.reconciler.is_some())
            .finish()
    }
}

/// A schedule that will never change: the one value, and no sender behind it.
///
/// The shape every caller had before the schedule could move — a test, the CLI,
/// a deployment that has never opened `/v1/config`. The sender is dropped on the
/// way out rather than kept alive somewhere, so the supervisor's first
/// `changed()` reports the channel closed and it stops listening. That is a
/// property worth having rather than a leak tolerated: "this schedule is final"
/// is then something the receiver *learns*, instead of a flag someone has to
/// remember to pass.
#[must_use]
pub fn fixed(fields: Vec<FieldSchedule>) -> watch::Receiver<Vec<FieldSchedule>> {
    watch::channel(fields).1
}

/// One field's polling schedule: what to ask for, and how often to ask.
///
/// `name` is a canonical query name as [`Query`] spells it (e.g.
/// `"RUNNING_STATE"`) — the same string that lands in [`Read::field`], which
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

/// Owns the supervisor, and through it every poll loop. Call
/// [`SyncHandle::shutdown`] (or drop it) to stop them.
pub struct SyncHandle {
    supervisor: JoinHandle<()>,
    cancel: CancellationToken,
}

impl SyncHandle {
    /// Signal every loop to stop and wait for the in-flight polls to drain.
    ///
    /// Cancellation is cooperative: a loop finishes its current SSH exchange
    /// before exiting, rather than being aborted mid-exchange.
    ///
    /// Instrumented here rather than at the call site because this is where the
    /// duration of a drain is known: the formatter derives it from the span's
    /// close, and since cancellation is cooperative it is bounded below by the
    /// slowest in-flight SSH exchange, which is exactly the thing worth
    /// watching. How *many* loops are being waited on is logged by the
    /// supervisor, which is what holds them now — the count moved there with
    /// them, and it is the honest place for it: the schedule can have changed
    /// several times since this handle was made.
    ///
    /// `skip(self)` is required, not stylistic: `#[instrument]` records every
    /// argument including the receiver, and [`SyncHandle`] is not `Debug`.
    #[instrument(name = "sync_shutdown", skip(self))]
    pub async fn shutdown(self) {
        self.cancel.cancel();
        // The supervisor drains the loops before it returns, so awaiting it is
        // awaiting the fleet. An `Err` here is a panicked supervisor, which
        // leaves nothing to drain and nothing to say that the panic itself has
        // not already said.
        let _ = self.supervisor.await;
        info!("sync driver stopped");
    }
}

/// Start the supervisor, which starts one poll loop per `(device, field)` and
/// keeps them matching whatever schedule is published.
///
/// Must be called from within a Tokio runtime (it uses [`tokio::spawn`]).
pub fn spawn(registry: Arc<Registry>, write: DynWriteStore, cfg: SyncConfig) -> SyncHandle {
    let cancel = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(registry, write, cfg, cancel.clone()));
    SyncHandle { supervisor, cancel }
}

/// Hold the running loops to the published schedule until cancelled, then drain
/// them.
///
/// One task, and it does nothing between schedules — the polling is all in the
/// loops it owns. What it is *for* is that starting and stopping a loop needs
/// somewhere with a `JoinSet` and a runtime, and the alternative to a task is a
/// mutex around one shared between the handler thread and the loops.
async fn supervise(
    registry: Arc<Registry>,
    write: DynWriteStore,
    cfg: SyncConfig,
    cancel: CancellationToken,
) {
    let SyncConfig {
        mut fields,
        reconciler,
    } = cfg;

    // Announced once rather than per loop: whether a field reconciler exists is a
    // property of the deployment, not of a device.
    if reconciler.is_some() {
        info!("observed recording states will be reported to the write outbox");
    }

    let mut loops = Loops {
        registry,
        write,
        reconciler,
        cancel: cancel.clone(),
        tasks: JoinSet::new(),
        running: BTreeMap::new(),
    };

    // `borrow_and_update` rather than `borrow`, so a schedule published between
    // this line and the first `changed()` below is not applied twice.
    let initial = fields.borrow_and_update().clone();

    // Announced once per field rather than once per (device, field): a disabled
    // field is a property of the config, and repeating it per device would say
    // the same thing as many times as there are devices. Only at startup — after
    // that a field being switched off is a *change*, and `Loops::reconcile` says
    // so as one.
    for field in initial.iter().filter(|f| f.interval.is_none()) {
        info!(
            field = field.name,
            "polling disabled for this field; no loop started"
        );
    }

    loops.reconcile(&initial);
    info!(tasks = loops.tasks.len(), "sync driver started");

    // Whether there is still anyone who could publish a schedule. A closed
    // channel is not a failure: it is a deployment whose schedule was decided
    // once — see `fixed` — and the answer to it is to stop asking rather than to
    // spin on a `changed()` that returns immediately forever.
    let mut watching = true;

    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            changed = fields.changed(), if watching => match changed {
                Ok(()) => {
                    let next = fields.borrow_and_update().clone();
                    let change = loops.reconcile(&next);
                    if change.is_nothing() {
                        // The schedule was republished with nothing new in it —
                        // a `PATCH` that named a field at the interval it
                        // already had, or a reload of an unchanged file. Worth a
                        // line, because "the request landed and changed nothing"
                        // is otherwise indistinguishable from a request that
                        // never arrived.
                        debug!("a schedule was published that changes no poll loop");
                    } else {
                        info!(
                            started = change.started,
                            stopped = change.stopped,
                            retimed = change.retimed,
                            tasks = loops.tasks.len(),
                            "the poll schedule changed"
                        );
                    }
                }
                Err(_) => watching = false,
            },
        }
    }

    info!(tasks = loops.tasks.len(), "draining poll loops");
    // Cancelled above — every loop's token is a child of it — so this waits on
    // loops that are already on their way out.
    while loops.tasks.join_next().await.is_some() {}
}

/// The poll loops that are running, and everything needed to start another.
struct Loops {
    registry: Arc<Registry>,
    write: DynWriteStore,
    reconciler: Option<DynWriteDrain>,
    /// The driver's token. Every loop's is a child of it, so one cancel stops
    /// the fleet and no per-field token can outlive the driver.
    cancel: CancellationToken,
    tasks: JoinSet<()>,
    /// One entry per field with loops running: what they are ticking at, and the
    /// token that stops just that field across every device.
    ///
    /// Keyed by field rather than by `(device, field)` because the schedule is:
    /// every device polls the same fields, so a field is the smallest thing a
    /// change can be about, and one token per field is one cancel per change
    /// rather than one per device.
    running: BTreeMap<String, (Duration, CancellationToken)>,
}

/// What one pass of [`Loops::reconcile`] did, for the line it logs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Change {
    started: usize,
    stopped: usize,
    retimed: usize,
}

impl Change {
    /// Whether the published schedule asked for anything the loops were not
    /// already doing.
    fn is_nothing(self) -> bool {
        self == Self::default()
    }
}

impl Loops {
    /// Make what is running match `schedule`, and report what moved.
    fn reconcile(&mut self, schedule: &[FieldSchedule]) -> Change {
        // Reap what the last change stopped, before adding to the set. Those
        // tasks have long since finished — they were cancelled a schedule ago —
        // so this is collecting handles rather than waiting on anything, and it
        // is what keeps the set from growing by a fleet's worth per change.
        while let Some(joined) = self.tasks.try_join_next() {
            if let Err(err) = joined {
                warn!(%err, "a poll loop ended abnormally");
            }
        }

        // Every field either list mentions. A field that has left the schedule
        // has to be visited too — it is the one whose loops must stop — and it
        // appears in `running` alone. Owned rather than borrowed, because the
        // pass below mutates the very map half of these names came from.
        let named: BTreeSet<String> = schedule
            .iter()
            .map(|f| f.name.clone())
            .chain(self.running.keys().cloned())
            .collect();

        let mut change = Change::default();
        for field in &named {
            let field = field.as_str();
            let wanted = schedule
                .iter()
                .find(|f| f.name == field)
                .and_then(|f| f.interval);
            let running = self.running.get(field).map(|(interval, _)| *interval);

            match act(running, wanted) {
                Action::Leave => {}
                Action::Stop => {
                    self.stop(field);
                    change.stopped += 1;
                    info!(field, "polling stopped for this field");
                }
                Action::Start => {
                    // `act` returns `Start` only when `wanted` is `Some`.
                    if let Some(interval) = wanted {
                        self.start(field, interval);
                        change.started += 1;
                        info!(field, interval_secs = interval.as_secs(), "polling started");
                    }
                }
                Action::Retime => {
                    if let Some(interval) = wanted {
                        self.stop(field);
                        self.start(field, interval);
                        change.retimed += 1;
                        info!(
                            field,
                            interval_secs = interval.as_secs(),
                            "polling re-timed"
                        );
                    }
                }
            }
        }
        change
    }

    /// Start one loop per device for `field`, under a token of its own.
    fn start(&mut self, field: &str, interval: Duration) {
        let token = self.cancel.child_token();
        for device in self.registry.devices() {
            self.tasks.spawn(poll_loop(
                device,
                field.to_owned(),
                self.write.clone(),
                // Cloning an `Option<Arc<_>>` is a refcount bump when present
                // and nothing when absent.
                self.reconciler.clone(),
                interval,
                token.clone(),
            ));
        }
        self.running.insert(field.to_owned(), (interval, token));
    }

    /// Signal every loop polling `field` to stop, and forget them.
    ///
    /// It does not *wait*: cancellation is cooperative, so a loop midway through
    /// an SSH exchange finishes it, and blocking the supervisor on that would
    /// hold up every other field in the same change — including the one this
    /// field is being re-timed to. The tasks are reaped at the next reconcile,
    /// or drained at shutdown.
    fn stop(&mut self, field: &str) {
        if let Some((_, token)) = self.running.remove(field) {
            token.cancel();
        }
    }
}

/// What a schedule entry means for the loops that may be running the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Nothing to do: running at the right interval, or absent from both.
    Leave,
    /// Not running and wanted.
    Start,
    /// Running and no longer wanted — dropped from the schedule, or set to
    /// never, which are the same thing to a loop.
    Stop,
    /// Running at the wrong interval.
    Retime,
}

/// The whole of the diff, as one total function over what is running and what is
/// wanted.
///
/// Pure, and separated from the effects for the reason [`step`] is: the decision
/// is the part with cases worth stating, and stated here it is testable without
/// a runtime, a registry or a device.
const fn act(running: Option<Duration>, wanted: Option<Duration>) -> Action {
    match (running, wanted) {
        (None, None) => Action::Leave,
        (None, Some(_)) => Action::Start,
        (Some(_), None) => Action::Stop,
        // `Duration` has no `const` equality, so the comparison is spelled out
        // on the one field that decides it.
        (Some(now), Some(next)) => {
            if now.as_nanos() == next.as_nanos() {
                Action::Leave
            } else {
                Action::Retime
            }
        }
    }
}

/// Poll one field on one device forever, persisting each read, until
/// cancelled.
async fn poll_loop(
    device: Arc<Device>,
    field: String,
    write: DynWriteStore,
    reconciler: Option<DynWriteDrain>,
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

                // Every condition as one combinator chain producing "the thing to report", or
                // `None`. The effect stays outside the pipeline, so the `.await` is visible.
                let to_report = reconciler
                    .as_ref()
                    .filter(|_| is_running_state(&field))
                    .zip(outcome.as_ref().ok().and_then(Value::as_state));

                // Reconcile before persisting, and by borrow, because `outcome` is moved by the `if
                // let Ok(value)` below. Deliberately outside `health`: a store that will not take
                // an observation says nothing about whether the device answered — the same reason a
                // failed `upsert_latest` is not folded in.
                if let Some((drain, state)) = to_report
                   && let Err(err) = drain
                       .observe(device.id().to_string(), dto::state_to_dto(state))
                       .await
               {
                   warn!(device = device.id(), %err, "failed to report the observed recording state");
               }

                if let Ok(value) = outcome {
                    let read = Read {
                        device: device.id().to_string(),
                        field: field.clone(),
                        value: dto::to_dto(value),
                        at: Timestamp(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
                    };
                    // Not part of `health`: a store that will not take a read
                    // says nothing about whether the device answered.
                    if let Err(err) = write.upsert_latest(read).await {
                        warn!(device = device.id(), field, %err, "failed to persist read");
                    }
                }
            }
        }
    }

    info!(device = device.id(), field, "poll loop stopped");
}
/// The one field whose value the write side reconciles against. Read off the
/// catalog rather than written as a literal, so a rename in core moves this
/// with it instead of silently disabling the hook.
fn is_running_state(field: &str) -> bool {
    field == Query::RunningState.name()
}

/// The drift sentinel for the *other* place that names this field.
///
/// `sismatic-store` files a group's recording expectation under
/// [`RECORDING_STATE_FIELD`], and has to spell it as a literal because a port
/// the front end depends on may not see core's instruction catalog. This crate
/// is one of the few that sees both, and already reads the canonical name off
/// the catalog a line above — so the two are held together here rather than
/// hoped to agree.
///
/// What a rename would otherwise cost: expectations filed under the old name,
/// reads written under the new one, and every group reporting `unknown`
/// forever with nothing failing to say why.
///
/// [`RECORDING_STATE_FIELD`]: sismatic_store::group::RECORDING_STATE_FIELD
#[test]
fn the_stores_recording_field_is_the_name_this_driver_polls_it_under() {
    assert_eq!(
        sismatic_store::group::RECORDING_STATE_FIELD,
        Query::RunningState.name()
    );
    assert!(is_running_state(
        sismatic_store::group::RECORDING_STATE_FIELD
    ));
}

/// What one loop believes about its `(device, field)` pair right now. Two states,
/// because an operator only ever asks one question of a poll loop: is this
/// read current, or stale?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Up,
    Down,
}

/// A poll result reduced to the three cases the health machine distinguishes.
///
/// The distinction that matters is [`Gated`](Contact::Gated) versus
/// [`Failed`](Contact::Failed): both mean "no read", but only `Failed` is
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

    // The wire `RecordingState`, not core's: `observe` takes what
    // `dto::state_to_dto` produces.
    use sismatic_api_types::{DeviceId, Read, RecordingState, WriteId, WriteRecord};
    use sismatic_core::devices::config::DeviceConfig;
    use sismatic_core::devices::connector::fake::CountingConnector;
    use sismatic_core::devices::connector::{ConnectError, Connector};
    use sismatic_core::devices::transport::Transport;
    use sismatic_core::devices::transport::fake::FakeTransport;
    use sismatic_store::outbox::{Claim, Outcome, WriteDrain};
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
        async fn upsert_latest(&self, read: Read) -> Result<(), WriteError> {
            self.fields.lock().expect("lock").push(read.field);
            Ok(())
        }
    }

    /// A [`WriteDrain`] that records what was observed and does nothing else.
    /// A poll loop only ever calls `observe`; the other three methods belong to
    /// the relay, and stubbing them `Ok`-and-empty rather than `unimplemented!`
    /// means a loop that wrongly reached for one fails an assertion here rather
    /// than panicking inside a spawned task, where the panic is easy to miss.
    #[derive(Default)]
    struct RecordingReconciler {
        observed: Mutex<Vec<(String, RecordingState)>>,
    }

    impl RecordingReconciler {
        fn observed(&self) -> Vec<(String, RecordingState)> {
            self.observed.lock().expect("lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl WriteDrain for RecordingReconciler {
        async fn claim_next(
            &self,
            _device: DeviceId,
            _at: Timestamp,
        ) -> Result<Option<Claim>, WriteError> {
            Ok(None)
        }

        async fn settle(
            &self,
            _id: WriteId,
            _outcome: Outcome,
            _at: Timestamp,
        ) -> Result<(), WriteError> {
            Ok(())
        }

        async fn observe(
            &self,
            device: DeviceId,
            observed: RecordingState,
        ) -> Result<(), WriteError> {
            self.observed.lock().expect("lock").push((device, observed));
            Ok(())
        }

        async fn in_flight(&self, _device: DeviceId) -> Result<Vec<WriteRecord>, WriteError> {
            Ok(Vec::new())
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
            exchange_timeout: Duration::from_millis(500),
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

    /// A registry of one device whose every connection replays `reply`.
    fn registry_replying(reply: &'static str) -> Arc<Registry> {
        let connector = Arc::new(CountingConnector::new(move || {
            FakeTransport::with_reads([reply; 8])
        }));
        Arc::new(Registry::build(
            vec![device_config("fixture")],
            vec![],
            connector,
        ))
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

    /// The read side feeding the write side: one poll of `RUNNING_STATE` serves
    /// both, which is the whole reason `reconciler` is a field on the config
    /// rather than a second driver polling the same register.
    #[tokio::test]
    async fn a_polled_recording_state_reaches_the_reconciler() {
        // `1\r\n` is what `parse_state` decodes as `Started`.
        let registry = registry_replying("1\r\n");
        let store = Arc::new(RecordingStore::default());
        let reconciler = Arc::new(RecordingReconciler::default());

        let sync = spawn(
            registry,
            store,
            SyncConfig {
                fields: fixed(vec![FieldSchedule {
                    name: "RUNNING_STATE".to_owned(),
                    interval: Some(Duration::from_millis(10)),
                }]),
                reconciler: Some(reconciler.clone()),
            },
        );

        wait_for(|| !reconciler.observed().is_empty()).await;
        sync.shutdown().await;

        assert_eq!(
            reconciler.observed()[0],
            ("fixture".to_owned(), RecordingState::Started)
        );
    }

    /// The hook is keyed off the field name, not off whatever the value happens
    /// to be. A firmware string is not a recording state, and folding one into
    /// the write side's desired recording state would be how a poll loop
    /// unfreezes metadata.
    #[tokio::test]
    async fn a_field_that_is_not_the_recording_state_is_never_reconciled() {
        let registry = registry_replying(FIRMWARE_REPLY);
        let store = Arc::new(RecordingStore::default());
        let reconciler = Arc::new(RecordingReconciler::default());

        let sync = spawn(
            registry,
            store.clone(),
            SyncConfig {
                fields: fixed(vec![FieldSchedule {
                    name: "FIRMWARE".to_owned(),
                    interval: Some(Duration::from_millis(10)),
                }]),
                reconciler: Some(reconciler.clone()),
            },
        );

        // The store proves the loop ran, so an empty observation log below is
        // evidence of the filter rather than of a driver that never started.
        wait_for(|| !store.fields().is_empty()).await;
        sync.shutdown().await;

        assert!(
            reconciler.observed().is_empty(),
            "only RUNNING_STATE may be reconciled, got: {:?}",
            reconciler.observed()
        );
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
                fields: fixed(vec![
                    FieldSchedule {
                        name: "UNIT_NAME".to_owned(),
                        interval: None,
                    },
                    FieldSchedule {
                        name: "FIRMWARE".to_owned(),
                        interval: Some(Duration::from_millis(10)),
                    },
                ]),
                reconciler: None,
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
                fields: fixed(vec![FieldSchedule {
                    name: "FIRMWARE".to_owned(),
                    interval: None,
                }]),
                reconciler: None,
            },
        );

        // No loop means no connection is ever opened — the device is left as
        // untouched as if it had not been listed.
        tokio::time::sleep(Duration::from_millis(50)).await;
        sync.shutdown().await;

        assert_eq!(opens.load(Ordering::SeqCst), 0);
        assert!(store.fields().is_empty());
    }

    // ---- a schedule that changes ------------------------------------------

    /// The diff, as a table. Six cases over two `Option`s, all written out, for
    /// the reason [`step`]'s table is: this is where a re-timing that silently
    /// did nothing — or a field that was started twice — would come from, and
    /// none of it needs a device to check.
    #[test]
    fn the_diff_is_a_table() {
        let five = Some(Duration::from_secs(5));
        let ten = Some(Duration::from_secs(10));

        assert_eq!(act(None, None), Action::Leave);
        assert_eq!(act(None, five), Action::Start);
        assert_eq!(act(five, None), Action::Stop);
        assert_eq!(act(five, five), Action::Leave);
        assert_eq!(act(five, ten), Action::Retime);
        // A field dropped from the schedule and a field set to never are the
        // same instruction to a loop, and reach `act` as the same argument.
        assert_eq!(act(ten, None), Action::Stop);
    }

    /// The property the whole supervisor exists for: a field can be re-timed
    /// without restarting the process.
    ///
    /// Asserted on the *rate* rather than on a log line, because the rate is
    /// what an operator changed the number for. The first interval is slow
    /// enough that a driver ignoring the update would produce almost nothing in
    /// the window the second half measures.
    #[tokio::test]
    async fn a_published_schedule_re_times_a_running_field() {
        let registry = registry_replying(FIRMWARE_REPLY);
        let store = Arc::new(RecordingStore::default());
        let (schedule, fields) = watch::channel(vec![FieldSchedule {
            name: "FIRMWARE".to_owned(),
            interval: Some(Duration::from_secs(3_600)),
        }]);

        let sync = spawn(
            registry,
            store.clone(),
            SyncConfig {
                fields,
                reconciler: None,
            },
        );

        // The first tick of an hourly loop fires immediately, so one read is
        // what "started and then went quiet for an hour" looks like.
        wait_for(|| !store.fields().is_empty()).await;
        assert_eq!(store.fields().len(), 1);

        schedule
            .send(vec![FieldSchedule {
                name: "FIRMWARE".to_owned(),
                interval: Some(Duration::from_millis(10)),
            }])
            .expect("the supervisor is listening");

        // Four more reads at ten milliseconds is well inside the timeout and
        // impossible at an hour.
        wait_for(|| store.fields().len() >= 5).await;
        sync.shutdown().await;
    }

    /// The other direction, and the one that has to actually stop a task: a
    /// field switched off is not merely dropped from a list.
    #[tokio::test]
    async fn a_field_switched_off_stops_being_polled() {
        let registry = registry_replying(FIRMWARE_REPLY);
        let store = Arc::new(RecordingStore::default());
        let (schedule, fields) = watch::channel(vec![FieldSchedule {
            name: "FIRMWARE".to_owned(),
            interval: Some(Duration::from_millis(5)),
        }]);

        let sync = spawn(
            registry,
            store.clone(),
            SyncConfig {
                fields,
                reconciler: None,
            },
        );

        wait_for(|| store.fields().len() >= 3).await;
        schedule
            .send(vec![FieldSchedule {
                name: "FIRMWARE".to_owned(),
                interval: None,
            }])
            .expect("the supervisor is listening");

        // Cancellation is cooperative, so a poll already in flight may still
        // land. Settle first, then measure: what must be true is that the count
        // stops moving, not that it froze at the instant of the send.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let settled = store.fields().len();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            store.fields().len(),
            settled,
            "a field switched off should have stopped polling, not slowed down"
        );
        sync.shutdown().await;
    }

    /// A field the first schedule never mentioned gets loops of its own — the
    /// case a diff keyed off the running set alone would miss.
    #[tokio::test]
    async fn a_field_added_to_the_schedule_starts_polling() {
        let registry = registry_replying(FIRMWARE_REPLY);
        let store = Arc::new(RecordingStore::default());
        let (schedule, fields) = watch::channel(vec![FieldSchedule {
            name: "FIRMWARE".to_owned(),
            interval: Some(Duration::from_millis(5)),
        }]);

        let sync = spawn(
            registry,
            store.clone(),
            SyncConfig {
                fields,
                reconciler: None,
            },
        );
        wait_for(|| !store.fields().is_empty()).await;

        schedule
            .send(vec![
                FieldSchedule {
                    name: "FIRMWARE".to_owned(),
                    interval: Some(Duration::from_millis(5)),
                },
                FieldSchedule {
                    name: "UNIT_NAME".to_owned(),
                    interval: Some(Duration::from_millis(5)),
                },
            ])
            .expect("the supervisor is listening");

        wait_for(|| store.fields().iter().any(|f| f == "UNIT_NAME")).await;
        sync.shutdown().await;

        // ...and the field that was already running was left alone rather than
        // restarted out from under itself.
        assert!(store.fields().iter().any(|f| f == "FIRMWARE"));
    }

    /// A publisher that goes away is not a failure. The supervisor stops
    /// listening and keeps the schedule it has — which is the deployment
    /// `fixed` produces, and the shape every caller had before the schedule
    /// could move at all.
    #[tokio::test]
    async fn a_dropped_publisher_leaves_the_loops_running() {
        let registry = registry_replying(FIRMWARE_REPLY);
        let store = Arc::new(RecordingStore::default());
        let (schedule, fields) = watch::channel(vec![FieldSchedule {
            name: "FIRMWARE".to_owned(),
            interval: Some(Duration::from_millis(5)),
        }]);

        let sync = spawn(
            registry,
            store.clone(),
            SyncConfig {
                fields,
                reconciler: None,
            },
        );
        wait_for(|| !store.fields().is_empty()).await;

        drop(schedule);

        // Still polling a hundred milliseconds later: a supervisor that treated
        // a closed channel as an error would have stopped, and one that spun on
        // it would starve the loops it owns.
        let seen = store.fields().len();
        wait_for(|| store.fields().len() > seen + 5).await;
        sync.shutdown().await;
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
                fields: fixed(
                    ["FIRMWARE", "UNIT_NAME", "MODEL_NAME", "TIMEZONE"]
                        .into_iter()
                        .map(|name| FieldSchedule {
                            name: name.to_owned(),
                            interval: Some(Duration::from_millis(5)),
                        })
                        .collect(),
                ),
                reconciler: None,
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
