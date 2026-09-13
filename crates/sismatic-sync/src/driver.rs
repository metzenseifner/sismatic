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
//! Cancellation is per `(field, device)`, through a token that is a child of
//! the driver's own, so stopping one loop cannot outlive the driver and stopping
//! the driver stops them all. Both are cooperative: a loop being re-timed
//! finishes its current exchange under the old interval before it goes.
//!
//! A deployment with nothing to say still fits: [`fixed`] is a schedule with no
//! sender behind it, and a supervisor that finds the sender gone stops listening
//! and keeps polling what it has.
//!
//! # A fleet that changes under the loops
//!
//! The device set moves too, on a second channel — [`SyncConfig::fleet`], a
//! generation counter whose value is never read, because the supervisor holds
//! the registry and all the channel has to say is *look again*. [`fixed_fleet`]
//! is its counterpart to [`fixed`].
//!
//! This is why the running loops are keyed by `(field, device)` and not by field
//! alone. When every device polled the same fields, a field was the smallest
//! thing a change could be about, and one token per field was one cancel per
//! change instead of one per device. A fleet that can gain a recorder breaks
//! that: under the coarser key the only way to give the new device its loops is
//! to cancel and respawn every field on every device, and since a restarted
//! ticker fires immediately, adding one recorder to a wildcard schedule makes
//! the whole fleet run forty-odd exchanges back to back. At the finer grain the
//! loops that were already right are not touched at all.
//!
//! It also needs one thing a device *id* cannot provide. A device is immutable,
//! so an edited one is replaced: same id, quite possibly the same interval, and
//! an `Arc<Device>` the registry no longer hands out — a private connection and
//! a stale copy of the `disabled_fields` that were just edited. So each running
//! loop records the [`uuid`] of the config it was started against, and [`act`]
//! compares that first. That is what makes "this device's veto changed" reach
//! the poll loops at all.
//!
//! [`uuid`]: sismatic_core::devices::config::DeviceConfig::uuid
//!
//! # Fields a device will not answer
//!
//! The schedule is fleet-wide and support is per device, so the two compose by
//! subtraction: what a device is actually polled for is the schedule minus that
//! device's vetoes. The veto is one-directional — a device can only remove — so
//! nothing here has to arbitrate between the two, and a `PATCH /v1/config` can
//! never turn on a field a recorder cannot answer.
//!
//! The two halves of the veto are enforced in two different places, and the
//! asymmetry is not an accident:
//!
//! * **Declared** (`disabled_fields`) is known before anything starts, so
//!   [`Loops::start`] simply starts no loop for that `(device, field)`. It costs
//!   nothing at all — no task, no ticker, no wake-up.
//! * **Inferred** (`auto_disable_after` consecutive refusals) cannot be known
//!   before polling, and a device with `self_heal_secs` set needs the loop to
//!   *stay* in order to retry. So it is enforced inside [`poll_loop`], which
//!   checks the veto before the exchange and skips it. That costs a timer
//!   wake-up per tick and saves the SSH round trip, which is the trade the whole
//!   feature is about. With `self_heal_secs` unset there is no retry to perform,
//!   so the loop ends itself rather than waking forever to do nothing.
//!
//! This crate is also the only place that *counts* refusals, and that is what
//! makes "a hand-issued write must not disable a field for the fleet" true by
//! construction rather than by convention: counting requires repetition, and
//! this is what repeats. Enforcement is the other way round — `Device::run`
//! checks the veto for every caller — so once a field is off, nothing asks for
//! it.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use sismatic_api_types::{Read, Timestamp};
use sismatic_core::devices::auto_disabled::Refusal;
use sismatic_core::devices::config::Uuid;
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
    /// Where a *device set* change is announced.
    ///
    /// A generation counter rather than the fleet itself, and the value is never
    /// read: the supervisor holds the `Arc<Registry>` already, so all this has
    /// to carry is "look again". That is also what makes a [`watch`]'s lossiness
    /// free here — three device changes arriving faster than the supervisor
    /// wakes collapse into one reconcile against the same final fleet, which is
    /// the right answer rather than a tolerated approximation.
    ///
    /// A deployment whose fleet is fixed for the life of the process passes
    /// [`fixed_fleet`].
    pub fleet: watch::Receiver<u64>,
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

/// A fleet that will never change: no sender, so the supervisor learns on its
/// first `changed()` that nothing can announce a device change and stops asking.
///
/// [`fixed`]'s counterpart, and the shape every caller had before the device set
/// could move — `sismatic-cli`, this crate's own tests, and any deployment that
/// builds its registry once and leaves it alone.
#[must_use]
pub fn fixed_fleet() -> watch::Receiver<u64> {
    watch::channel(0).1
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
        mut fleet,
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

    // `borrow_and_update` rather than `borrow`, so a value published between this
    // line and the first `changed()` below is not applied twice.
    let mut schedule = fields.borrow_and_update().clone();
    fleet.borrow_and_update();

    // Announced once per field rather than once per (device, field): a field
    // nobody polls is a property of the schedule, and repeating it per device
    // would say the same thing as many times as there are devices. Only at
    // startup — after that a field being switched off is a *change*, and
    // `Loops::reconcile` says so as one.
    for field in schedule.iter().filter(|f| f.interval.is_none()) {
        info!(
            field = field.name,
            "polling disabled for this field; no loop started"
        );
    }

    let initial = loops.reconcile(&schedule);
    info!(
        tasks = loops.tasks.len(),
        declined = initial.declined,
        "sync driver started"
    );

    // Whether there is still anyone who could publish. A closed channel is not a
    // failure: it is a deployment whose schedule — or whose fleet — was decided
    // once (see `fixed` and `fixed_fleet`), and the answer to it is to stop
    // asking rather than to spin on a `changed()` that returns immediately
    // forever. The two are tracked apart because a deployment can reasonably
    // have one and not the other.
    let mut watching_fields = true;
    let mut watching_fleet = true;

    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            changed = fields.changed(), if watching_fields => match changed {
                Ok(()) => {
                    schedule = fields.borrow_and_update().clone();
                    report(loops.reconcile(&schedule), &loops, "the poll schedule changed");
                }
                Err(_) => watching_fields = false,
            },
            changed = fleet.changed(), if watching_fleet => match changed {
                Ok(()) => {
                    // The value is a generation counter and is deliberately not
                    // read: what it says is "look again", and the registry is
                    // what gets looked at. That is also why losing intermediate
                    // values costs nothing — three device changes collapsing
                    // into one wake-up reconcile against the same final fleet.
                    fleet.borrow_and_update();
                    // Against the schedule already in force. Re-borrowing
                    // `fields` here would apply a pending schedule change on the
                    // fleet's wake-up and leave its own arm reporting a no-op,
                    // attributing the change to the wrong event in the log.
                    report(loops.reconcile(&schedule), &loops, "the device fleet changed");
                }
                Err(_) => watching_fleet = false,
            },
        }
    }

    info!(tasks = loops.tasks.len(), "draining poll loops");
    // Cancelled above — every loop's token is a child of it — so this waits on
    // loops that are already on their way out.
    while loops.tasks.join_next().await.is_some() {}
}

/// Log what a reconcile did, at a level that follows whether it did anything.
///
/// A pass that moves nothing is `debug`, because "the request landed and changed
/// nothing" is otherwise indistinguishable from a request that never arrived —
/// but it is not news, and both a republished schedule and a device edit that
/// misses this driver entirely can produce one.
fn report(change: Change, loops: &Loops, what: &'static str) {
    if change.is_nothing() {
        debug!(what, "a change was published that moves no poll loop");
    } else {
        info!(
            started = change.started,
            stopped = change.stopped,
            retimed = change.retimed,
            rebound = change.rebound,
            declined = change.declined,
            tasks = loops.tasks.len(),
            what
        );
    }
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
    /// One entry per running loop, keyed by `(field, device id)`.
    ///
    /// Keyed by the pair rather than by field alone, which it was until the
    /// fleet could change while the process runs. When every device polled the
    /// same fields, a field *was* the smallest thing a change could be about,
    /// and one token per field was one cancel per change rather than one per
    /// device. Adding a single recorder breaks that: under the coarser key the
    /// only way to give it loops is to cancel and respawn every field across
    /// the whole fleet, which — on a wildcard schedule — makes every device run
    /// forty-odd exchanges back to back because each restarted ticker fires
    /// immediately.
    ///
    /// At this grain the loops that were already right are not touched at all,
    /// so adding a device costs exactly that device's loops and nothing else
    /// changes phase. The price is a token per pair rather than per field, which
    /// is a `CancellationToken` — an `Arc` and an atomic — per running loop that
    /// already owns a task and a ticker.
    running: BTreeMap<Key, Running>,
}

/// What identifies one poll loop: the field it polls and the device it polls.
type Key = (String, String);

/// A running loop, and what it was started against.
struct Running {
    interval: Duration,
    /// The [`uuid`] of the device config this loop was started for.
    ///
    /// Carried because a device id is not enough to decide whether a loop is
    /// still correct. A device is immutable, so an edited one is *replaced*: the
    /// id is unchanged, the interval may be unchanged, and the `Arc<Device>` the
    /// loop is holding is one the registry no longer hands out — pointed at a
    /// connection nobody else will ever use, and carrying the old
    /// `disabled_fields`. Comparing UUIDs is what catches that; comparing
    /// intervals cannot.
    ///
    /// [`uuid`]: sismatic_core::devices::config::DeviceConfig::uuid
    device: Uuid,
    cancel: CancellationToken,
}

/// What a `(field, device)` pair should be running, as a value the pure decision
/// below can compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wanted {
    interval: Duration,
    device: Uuid,
}

/// What one pass of [`Loops::reconcile`] did, for the line it logs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Change {
    started: usize,
    stopped: usize,
    /// Same device, different interval.
    retimed: usize,
    /// Same id, different device — a replaced config. Counted apart from
    /// `retimed` because the two have different causes and an operator reading
    /// the line wants to know which happened: a re-timing came from the
    /// schedule, a rebinding came from the fleet.
    rebound: usize,
    /// How many `(field, device)` pairs the schedule named and a device's
    /// `disabled_fields` declined.
    ///
    /// Not a change — it is the same every pass until the fleet or the schedule
    /// moves — but reported with them, because it is the number that explains
    /// why `tasks` is smaller than fields × devices.
    declined: usize,
}

impl Change {
    /// Whether the published value asked for anything the loops were not
    /// already doing.
    ///
    /// `declined` is excluded on purpose: a pass that started nothing, stopped
    /// nothing and declined forty pairs did not *change* anything, and treating
    /// it as a change would put an `info!` line in the log on every republished
    /// schedule for as long as any device disables any field.
    fn is_nothing(self) -> bool {
        self.started == 0 && self.stopped == 0 && self.retimed == 0 && self.rebound == 0
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

        // The fleet as it stands, read once. `Registry::devices` allocates a
        // vector of handles, and reading it per field would do that once per
        // field for an answer that cannot change inside one pass.
        let fleet: BTreeMap<String, Arc<Device>> = self
            .registry
            .devices()
            .into_iter()
            .map(|device| (device.id().to_owned(), device))
            .collect();

        let (wanted, declined) = wanted_loops(schedule, &fleet);

        // Every pair either side mentions. A pair that has left — because its
        // field left the schedule, or its device left the fleet — has to be
        // visited too, and it appears in `running` alone. Owned rather than
        // borrowed, because the pass below mutates the very map half of these
        // came from.
        let named: BTreeSet<Key> = wanted
            .keys()
            .cloned()
            .chain(self.running.keys().cloned())
            .collect();

        let mut change = Change {
            declined,
            ..Change::default()
        };
        for key in &named {
            let (field, device_id) = (key.0.as_str(), key.1.as_str());
            let want = wanted.get(key).copied();
            let current = self.running.get(key).map(|running| Wanted {
                interval: running.interval,
                device: running.device,
            });

            match act(current, want) {
                Action::Leave => {}
                Action::Stop => {
                    self.stop(key);
                    change.stopped += 1;
                    debug!(field, device = device_id, "polling stopped for this pair");
                }
                // `act` returns these three only when `want` is `Some`, and the
                // device is in `fleet` because that is where `want` came from.
                Action::Start => {
                    if let Some(want) = want
                        && let Some(device) = fleet.get(device_id)
                    {
                        self.start(key, device, want);
                        change.started += 1;
                        debug!(
                            field,
                            device = device_id,
                            interval_secs = want.interval.as_secs(),
                            "polling started"
                        );
                    }
                }
                Action::Retime => {
                    if let Some(want) = want
                        && let Some(device) = fleet.get(device_id)
                    {
                        self.stop(key);
                        self.start(key, device, want);
                        change.retimed += 1;
                        debug!(
                            field,
                            device = device_id,
                            interval_secs = want.interval.as_secs(),
                            "polling re-timed"
                        );
                    }
                }
                Action::Rebind => {
                    if let Some(want) = want
                        && let Some(device) = fleet.get(device_id)
                    {
                        self.stop(key);
                        self.start(key, device, want);
                        change.rebound += 1;
                        debug!(
                            field,
                            device = device_id,
                            "polling rebound to a replaced device"
                        );
                    }
                }
            }
        }
        change
    }

    /// Start one loop for one `(field, device)` pair, under a token of its own.
    fn start(&mut self, key: &Key, device: &Arc<Device>, want: Wanted) {
        let cancel = self.cancel.child_token();
        self.tasks.spawn(poll_loop(
            Arc::clone(device),
            key.0.clone(),
            self.write.clone(),
            // Cloning an `Option<Arc<_>>` is a refcount bump when present and
            // nothing when absent.
            self.reconciler.clone(),
            want.interval,
            cancel.clone(),
        ));
        self.running.insert(
            key.clone(),
            Running {
                interval: want.interval,
                device: want.device,
                cancel,
            },
        );
    }

    /// Signal one pair's loop to stop, and forget it.
    ///
    /// It does not *wait*: cancellation is cooperative, so a loop midway through
    /// an SSH exchange finishes it, and blocking the supervisor on that would
    /// hold up every other pair in the same change — including the one this pair
    /// is being re-timed to. The tasks are reaped at the next reconcile, or
    /// drained at shutdown.
    fn stop(&mut self, key: &Key) {
        if let Some(running) = self.running.remove(key) {
            running.cancel.cancel();
        }
    }
}

/// Every `(field, device)` pair that should have a loop, and how many the
/// devices declined.
///
/// Free-standing and taking the fleet as an argument rather than reading
/// `self.registry`, so the expansion — the place the fleet-wide schedule and the
/// per-device veto actually meet — is testable over values, with no registry, no
/// connector and no runtime.
///
/// This is where the *declared* veto is applied, and the inferred one is not.
/// The asymmetry is the point. A field named in `disabled_fields` is known
/// unsupported before anything starts, so the cheapest thing is to start
/// nothing: no task, no ticker, no wake-up. A field that might yet be *inferred*
/// unsupported has to be polled to become so, and one that has been inferred
/// still needs its loop when `self_heal_secs` is set, because the loop is what
/// performs the retry. So the inferred veto is enforced inside the loop instead
/// — see [`poll_loop`].
fn wanted_loops(
    schedule: &[FieldSchedule],
    fleet: &BTreeMap<String, Arc<Device>>,
) -> (BTreeMap<Key, Wanted>, usize) {
    let mut wanted = BTreeMap::new();
    let mut declined = 0usize;

    for entry in schedule {
        // `None` is *never*: the field stays listed and no loop is started.
        let Some(interval) = entry.interval else {
            continue;
        };
        for (id, device) in fleet {
            if device.config().disabled_fields.contains(&entry.name) {
                declined += 1;
                continue;
            }
            wanted.insert(
                (entry.name.clone(), id.clone()),
                Wanted {
                    interval,
                    device: device.config().uuid,
                },
            );
        }
    }
    (wanted, declined)
}

/// What the wanted state means for the loop that may be running a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Nothing to do: running against the right device at the right interval,
    /// or absent from both.
    Leave,
    /// Not running and wanted.
    Start,
    /// Running and no longer wanted — the field left the schedule or was set to
    /// never, the device left the fleet, or the device now declines the field.
    /// All four are the same thing to a loop.
    Stop,
    /// Running against the right device at the wrong interval.
    Retime,
    /// Running against a device the registry no longer hands out.
    Rebind,
}

/// The whole of the diff, as one total function over what is running and what is
/// wanted.
///
/// Pure, and separated from the effects for the reason [`step`] is: the decision
/// is the part with cases worth stating, and stated here it is testable without
/// a runtime, a registry or a device.
///
/// The device is compared *before* the interval, and that order is the whole
/// reason [`Running::device`] exists. A replaced device usually keeps its
/// interval — an operator editing `disabled_fields` or a password is not
/// touching the schedule — so a comparison that looked at the interval first
/// would answer [`Leave`](Action::Leave) and leave the loop holding an
/// `Arc<Device>` that nothing else references: a private connection, and a stale
/// copy of the very `disabled_fields` the edit changed.
const fn act(running: Option<Wanted>, wanted: Option<Wanted>) -> Action {
    match (running, wanted) {
        (None, None) => Action::Leave,
        (None, Some(_)) => Action::Start,
        (Some(_), None) => Action::Stop,
        (Some(now), Some(next)) => {
            if now.device.as_u128() != next.device.as_u128() {
                Action::Rebind
            // `Duration` has no `const` equality, so the comparison is spelled
            // out on the one field that decides it.
            } else if now.interval.as_nanos() == next.interval.as_nanos() {
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

    // Read once: a device is immutable, so neither of these can move under the
    // loop. A device whose policy changed is a *different* device, and the
    // supervisor will have replaced this loop along with it.
    let threshold = device.config().auto_disable_after;
    let self_heal = device.config().self_heal;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                // The inferred veto, checked before the exchange rather than
                // enforced by it. `Device::run` would refuse this anyway — that
                // is the guarantee every caller inherits — but going through it
                // would cost an error to build, classify and log on every tick
                // of a field nobody expects to answer. This is a set lookup.
                if let Some(retry_in) = device.auto_disabled().veto(&field) {
                    match retry_in {
                        Some(retry_in) => debug!(
                            device = device.id(),
                            field,
                            retry_in_secs = retry_in.as_secs(),
                            "skipping a poll of an auto-disabled field"
                        ),
                        // Unreachable in practice: a veto with no retry stops
                        // the loop below rather than letting it tick forever.
                        // Handled anyway, because "the loop that was supposed
                        // to stop did not" should cost a log line, not an
                        // exchange per tick.
                        None => debug!(
                            device = device.id(),
                            field, "skipping a poll of a permanently disabled field"
                        ),
                    }
                    continue;
                }

                let outcome = device.run(&instruction).await;

                // Count the answer *before* anything else looks at it. Only a
                // repeating caller can observe "consecutive", so this loop is
                // the only place in the system that counts — see `AutoDisabled`.
                if let Err(err) = &outcome
                    && err.is_refusal()
                {
                    match device.auto_disabled().refused(&field, threshold, self_heal) {
                        Refusal::Counted { refusals } => debug!(
                            device = device.id(),
                            field,
                            refusals,
                            threshold,
                            "the device refused this field"
                        ),
                        Refusal::Disabled { refusals } => match self_heal {
                            Some(wait) => info!(
                                device = device.id(),
                                field,
                                refusals,
                                retry_in_secs = wait.as_secs(),
                                "auto-disabling this field after consecutive refusals; \
                                 it will be tried again"
                            ),
                            None => info!(
                                device = device.id(),
                                field,
                                refusals,
                                "auto-disabling this field after consecutive refusals; \
                                 add it to this device's `disabled_fields` to make it \
                                 permanent, or set `self_heal_secs` to retry it"
                            ),
                        },
                        Refusal::Still => {}
                    }
                } else if outcome.is_ok() && device.auto_disabled().answered(&field) {
                    info!(
                        device = device.id(),
                        field, "an auto-disabled field answered again and is no longer disabled"
                    );
                }

                // The whole logging decision, taken by a pure function over
                // (previous state, this poll) before anything is emitted.
                let (next, report) = step(health, Contact::of(&outcome));
                health = next;
                announce(report, device.id(), &field, outcome.as_ref().err());

                // A field that will never be retried has no future work, so the
                // loop ends rather than waking forever to do nothing — the same
                // reasoning as the unknown-field case above. With `self_heal`
                // set the loop must stay: it *is* the retry mechanism.
                if self_heal.is_none() && device.auto_disabled().veto(&field).is_some() {
                    info!(
                        device = device.id(),
                        field, "poll loop stopped: this field is auto-disabled with no retry"
                    );
                    return;
                }

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
    /// The device answered, and the answer was *no*.
    ///
    /// Distinguished from [`Reached`](Contact::Reached) because there is no
    /// value to store, and from [`Failed`](Contact::Failed) because the device
    /// is demonstrably up: a refusal is a complete exchange on a healthy
    /// channel. Folding it into `Failed` — which is what this did before the
    /// veto existed — is what left every unanswerable field of every device
    /// reading `Down` forever, a state that was true of the *field* and false
    /// of everything an operator uses that word for.
    Refused,
    /// We tried to reach it and could not.
    Failed,
    /// We did not try: core's cold gate is shut. See [`DeviceError::Cold`].
    Gated,
    /// We did not try: the field is vetoed on this device.
    ///
    /// Reachable only by a race — the loop checks the veto before the exchange
    /// — but it is a state the type must carry, because `Device::run` can
    /// return it and a match that did not handle it would have to be a
    /// wildcard, which is the thing this enum exists to avoid.
    Vetoed,
}

impl Contact {
    /// Classify a poll result. Total and pure — every `DeviceError` lands in
    /// exactly one case, so the machine below never needs a fallback arm.
    fn of(outcome: &Result<Value, DeviceError>) -> Self {
        match outcome {
            Ok(_) => Contact::Reached,
            Err(DeviceError::Cold { .. }) => Contact::Gated,
            Err(DeviceError::Disabled { .. }) => Contact::Vetoed,
            Err(err) if err.is_refusal() => Contact::Refused,
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
    /// The device answered and said no. Not a health transition in either
    /// direction — it is reported so a refusal is visible at all, since the
    /// counting that follows one is at `debug`.
    Refusal,
}

/// The state machine, as one total function over `Health × Contact`.
///
/// Ten cases, all written out, so the policy is readable as a table rather than
/// inferred from control flow — and testable without a device, a clock, a task,
/// or a log subscriber, since it touches none of them.
///
/// The one non-obvious entry is `(Up, Gated)`: it moves to `Down` (the field
/// *is* unreadable) but reports `Ongoing` rather than `Onset`, because a shut
/// gate is downstream of a failed dial that some other loop has already
/// announced. That is what collapses an outage from one warning per
/// `(device, field)` to one per device.
///
/// [`Refused`](Contact::Refused) is the entry that changed when the field veto
/// arrived, and it is worth saying why in a table rather than in a commit
/// message. `Health` answers one question — *is this read current or stale* —
/// and a refusal makes it neither: the device is up, the exchange completed, and
/// there is simply no value. Treating it as `Failed` (as this did before) put a
/// device that is answering perfectly into `Down` on every field it declines,
/// and left it there permanently, because nothing about an unlicensed feature
/// ever recovers. So a refusal moves `Health` in neither direction and is
/// reported on its own terms. What *does* eventually act on it is the refusal
/// count, which is not a health question.
///
/// [`Vetoed`](Contact::Vetoed) is silent for the same reason at the other end:
/// we did not ask, so we learned nothing, so there is nothing to report and
/// nothing to move.
const fn step(before: Health, contact: Contact) -> (Health, Report) {
    match (before, contact) {
        (Health::Up, Contact::Reached) => (Health::Up, Report::Silent),
        (Health::Up, Contact::Failed) => (Health::Down, Report::Onset),
        (Health::Up, Contact::Gated) => (Health::Down, Report::Ongoing),
        (Health::Down, Contact::Reached) => (Health::Up, Report::Recovery),
        (Health::Down, Contact::Failed) => (Health::Down, Report::Ongoing),
        (Health::Down, Contact::Gated) => (Health::Down, Report::Ongoing),
        (health, Contact::Refused) => (health, Report::Refusal),
        (health, Contact::Vetoed) => (health, Report::Silent),
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
        // `debug`, not `warn`: a refusal repeats at the tick rate for as long as
        // the device declines the field, which is the same volume argument
        // `Ongoing` is held to. The events bounded by something an operator
        // cares about — the count reaching the threshold, the field healing —
        // are announced by the loop at `info`.
        Report::Refusal => debug!(device, field, error, "the device refused this field"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The wire `RecordingState`, not core's: `observe` takes what
    // `dto::state_to_dto` produces.
    use sismatic_api_types::{DeviceId, Read, RecordingState, WriteId, WriteRecord};
    use std::collections::BTreeSet;

    use sismatic_core::devices::auto_disabled::VetoSource;
    use sismatic_core::devices::config::{DeviceConfig, Resolved, Uuid};
    use sismatic_core::devices::connector::fake::CountingConnector;
    use sismatic_core::devices::connector::{ConnectError, Connector};
    use sismatic_core::devices::controller::ControllerError;
    use sismatic_core::devices::transport::Transport;
    use sismatic_core::devices::transport::fake::{Exhausted, FakeTransport};
    use sismatic_core::protocol::SisError;
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
            uuid: Uuid::nil(),
            disabled_fields: BTreeSet::new(),
            // Inference off: none of these tests is about it, and a fixture
            // that opted in would take a field out of the schedule mid-test on
            // the strength of a scripted refusal.
            auto_disable_after: 0,
            self_heal: None,
        }
        .derive_uuid()
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
                fleet: fixed_fleet(),
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
                fleet: fixed_fleet(),
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
                fleet: fixed_fleet(),
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
                fleet: fixed_fleet(),
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

    /// The diff, as a table, for the reason [`step`]'s table is: this is where a
    /// re-timing that silently did nothing — or a pair that was started twice —
    /// would come from, and none of it needs a device to check.
    #[test]
    fn the_diff_is_a_table() {
        let one = Uuid::from_u128(1);
        let at = |secs, device| {
            Some(Wanted {
                interval: Duration::from_secs(secs),
                device,
            })
        };

        assert_eq!(act(None, None), Action::Leave);
        assert_eq!(act(None, at(5, one)), Action::Start);
        assert_eq!(act(at(5, one), None), Action::Stop);
        assert_eq!(act(at(5, one), at(5, one)), Action::Leave);
        assert_eq!(act(at(5, one), at(10, one)), Action::Retime);
        // A field dropped from the schedule and a field set to never are the
        // same instruction to a loop, and reach `act` as the same argument.
        assert_eq!(act(at(10, one), None), Action::Stop);
    }

    /// The case the coarser key could not express, and the reason a running loop
    /// records which device it was started against. A replaced device usually
    /// keeps its interval — editing `disabled_fields` or a password does not
    /// touch the schedule — so a diff that compared intervals alone would answer
    /// `Leave` and strand the loop on a handle the registry no longer hands out.
    #[test]
    fn a_replaced_device_rebinds_even_at_an_unchanged_interval() {
        let before = Some(Wanted {
            interval: Duration::from_secs(5),
            device: Uuid::from_u128(1),
        });
        let after = Some(Wanted {
            interval: Duration::from_secs(5),
            device: Uuid::from_u128(2),
        });

        assert_eq!(act(before, after), Action::Rebind);
        // And a device change outranks an interval change, because the handle
        // being wrong is the more serious of the two and both are fixed by the
        // same stop-and-start.
        assert_eq!(
            act(
                before,
                Some(Wanted {
                    interval: Duration::from_secs(10),
                    device: Uuid::from_u128(2),
                })
            ),
            Action::Rebind
        );
    }

    // ---- expanding a fleet-wide schedule over a per-device veto ------------

    fn fleet_of(configs: Vec<DeviceConfig>) -> BTreeMap<String, Arc<Device>> {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([FIRMWARE_REPLY; 8])
        }));
        configs
            .into_iter()
            .map(|config| {
                let id = config.id.clone();
                (
                    id,
                    Arc::new(Device::new(
                        config,
                        Arc::clone(&connector) as Arc<dyn Connector>,
                    )),
                )
            })
            .collect()
    }

    fn every(secs: u64, names: &[&str]) -> Vec<FieldSchedule> {
        names
            .iter()
            .map(|name| FieldSchedule {
                name: (*name).to_owned(),
                interval: Some(Duration::from_secs(secs)),
            })
            .collect()
    }

    /// The expansion, over values: the fleet-wide schedule crossed with the
    /// fleet, minus each device's declared veto.
    #[test]
    fn the_wanted_set_is_the_schedule_crossed_with_the_fleet_minus_the_vetoes() {
        let fleet = fleet_of(vec![
            device_config("plain"),
            config_disabling("licensed", &["STREAM_2_NAME"]),
        ]);
        let (wanted, declined) = wanted_loops(&every(5, &["FIRMWARE", "STREAM_2_NAME"]), &fleet);

        let mut keys: Vec<_> = wanted.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                ("FIRMWARE".to_owned(), "licensed".to_owned()),
                ("FIRMWARE".to_owned(), "plain".to_owned()),
                ("STREAM_2_NAME".to_owned(), "plain".to_owned()),
            ],
            "the vetoed pair, and only it, should be absent"
        );
        assert_eq!(declined, 1);
    }

    /// A field at `None` is *never*: it stays listed and expands to nothing, so
    /// it contributes no loops and is not counted as declined either — nobody
    /// refused it, the schedule simply does not ask for it.
    #[test]
    fn a_field_set_to_never_expands_to_no_pairs() {
        let fleet = fleet_of(vec![device_config("plain")]);
        let schedule = vec![FieldSchedule {
            name: "FIRMWARE".into(),
            interval: None,
        }];
        let (wanted, declined) = wanted_loops(&schedule, &fleet);

        assert!(wanted.is_empty());
        assert_eq!(declined, 0, "a field nobody polls was not declined");
    }

    /// The whole point of keying by the pair: adding a device must not disturb
    /// the loops already running. Under the old per-field key the only way to
    /// give the new device its loops was to cancel and respawn every field
    /// across the fleet.
    #[tokio::test]
    async fn adding_a_device_starts_only_that_devices_loops() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([FIRMWARE_REPLY; 64])
        }));
        let registry = Arc::new(Registry::build(
            vec![device_config("first")],
            vec![],
            connector,
        ));
        let (fleet_tx, fleet_rx) = watch::channel(0u64);

        let handle = spawn(
            Arc::clone(&registry),
            Arc::new(RecordingStore::default()),
            SyncConfig {
                fields: fixed(every(60, &["FIRMWARE", "UNIT_NAME"])),
                fleet: fleet_rx,
                reconciler: None,
            },
        );
        // Two fields on one device.
        wait_for(|| registry.len() == 1).await;

        registry.apply(Resolved {
            devices: vec![device_config("first"), device_config("second")],
            groups: vec![],
        });
        fleet_tx.send_replace(1);

        // The new device's two loops appear; the first device's two are never
        // stopped, so nothing it holds is disturbed.
        wait_for(|| registry.device("second").is_some()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.shutdown().await;
    }

    /// Removing a device stops exactly its loops, and a fleet change reconciles
    /// against the schedule already in force rather than re-reading one.
    #[tokio::test]
    async fn removing_a_device_stops_its_loops_and_leaves_the_rest() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([FIRMWARE_REPLY; 64])
        }));
        let registry = Arc::new(Registry::build(
            vec![device_config("keeper"), device_config("goner")],
            vec![],
            connector,
        ));
        let (fleet_tx, fleet_rx) = watch::channel(0u64);

        let handle = spawn(
            Arc::clone(&registry),
            Arc::new(RecordingStore::default()),
            SyncConfig {
                fields: fixed(every(60, &["FIRMWARE"])),
                fleet: fleet_rx,
                reconciler: None,
            },
        );
        wait_for(|| registry.len() == 2).await;

        registry.apply(Resolved {
            devices: vec![device_config("keeper")],
            groups: vec![],
        });
        fleet_tx.send_replace(1);

        wait_for(|| registry.device("goner").is_none()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        // A supervisor that failed to stop the departed device's loop would
        // still be holding it here, and the drain would wait on it.
        tokio::time::timeout(Duration::from_secs(2), handle.shutdown())
            .await
            .expect("the loops should drain");
    }

    /// A device that gains a `disabled_fields` entry at runtime is a *replaced*
    /// device, so its loop for that field must stop — not merely be vetoed
    /// inside a loop that keeps ticking.
    #[tokio::test]
    async fn a_device_that_gains_a_veto_loses_that_fields_loop() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([FIRMWARE_REPLY; 64])
        }));
        let registry = Arc::new(Registry::build(
            vec![device_config("edited")],
            vec![],
            connector,
        ));
        let (fleet_tx, fleet_rx) = watch::channel(0u64);
        let store = Arc::new(RecordingStore::default());

        let handle = spawn(
            Arc::clone(&registry),
            store.clone(),
            SyncConfig {
                fields: fixed(every(60, &["FIRMWARE"])),
                fleet: fleet_rx,
                reconciler: None,
            },
        );
        wait_for(|| !store.fields().is_empty()).await;

        let change = registry.apply(Resolved {
            devices: vec![config_disabling("edited", &["FIRMWARE"])],
            groups: vec![],
        });
        assert_eq!(
            change.replaced,
            vec!["edited"],
            "editing disabled_fields mints a different device"
        );
        fleet_tx.send_replace(1);

        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(2), handle.shutdown())
            .await
            .expect("the loops should drain");
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
                fleet: fixed_fleet(),
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
                fleet: fixed_fleet(),
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
                fleet: fixed_fleet(),
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
                fleet: fixed_fleet(),
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

    // ---- fields a device will not answer ----------------------------------

    /// A refusal is the device *answering*, so it must not move the health
    /// machine. Before the veto existed this classified as `Failed`, which put
    /// a perfectly reachable recorder into `Down` on every field it declines and
    /// left it there forever — nothing about an unlicensed feature recovers.
    #[test]
    fn a_refusal_classifies_apart_from_a_failure() {
        let refused = Err(DeviceError::Command(ControllerError::Rejected {
            instruction: "STREAM_2_NAME".into(),
            error: SisError { code: 13 },
        }));
        assert_eq!(Contact::of(&refused), Contact::Refused);
        assert!(
            refused.as_ref().unwrap_err().is_refusal(),
            "core and this crate must agree on what a refusal is"
        );
    }

    /// ...and the machine leaves health where it found it, in both directions.
    #[test]
    fn a_refusal_moves_the_health_machine_in_neither_direction() {
        use Contact::*;
        use Report::*;

        assert_eq!(step(Health::Up, Refused), (Health::Up, Refusal));
        assert_eq!(step(Health::Down, Refused), (Health::Down, Refusal));

        // A device refusing one field is still up, so a later *failure* on that
        // field is a fresh onset rather than something a refusal already
        // swallowed.
        assert_eq!(
            reports([Reached, Refused, Refused, Failed]),
            vec![Silent, Refusal, Refusal, Onset]
        );
    }

    /// We did not ask, so we learned nothing, so nothing is reported and
    /// nothing moves.
    #[test]
    fn a_vetoed_poll_is_silent_and_leaves_health_alone() {
        let vetoed = Err(DeviceError::Disabled {
            field: "STREAM_2_NAME".into(),
            source: VetoSource::Declared,
            retry_in: None,
        });
        assert_eq!(Contact::of(&vetoed), Contact::Vetoed);
        assert_eq!(
            step(Health::Down, Contact::Vetoed),
            (Health::Down, Report::Silent),
            "a veto must not be read as a recovery"
        );
        assert!(
            !vetoed.as_ref().unwrap_err().is_refusal(),
            "the device said nothing; we did"
        );
    }

    /// A device config with a declared veto over `fields`.
    fn config_disabling(id: &str, fields: &[&str]) -> DeviceConfig {
        DeviceConfig {
            disabled_fields: fields.iter().map(|f| (*f).to_owned()).collect(),
            ..device_config(id)
        }
        .derive_uuid()
    }

    /// The cheap half of the veto: a declared field costs no task at all, so a
    /// device that disables it is never even dialed.
    #[tokio::test]
    async fn a_declared_veto_starts_no_loop_and_opens_no_connection() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([FIRMWARE_REPLY; 8])
        }));
        let opens = connector.opens_handle();
        let registry = Arc::new(Registry::build(
            vec![config_disabling("vetoed", &["FIRMWARE"])],
            vec![],
            connector,
        ));
        let store = Arc::new(RecordingStore::default());

        let handle = spawn(
            registry,
            store.clone(),
            SyncConfig {
                fields: fixed(vec![FieldSchedule {
                    name: "FIRMWARE".into(),
                    interval: Some(Duration::from_millis(10)),
                }]),
                fleet: fixed_fleet(),
                reconciler: None,
            },
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
        handle.shutdown().await;

        assert_eq!(
            opens.load(Ordering::SeqCst),
            0,
            "a declared veto must not cost a dial"
        );
        assert!(
            store.fields().is_empty(),
            "and must store nothing for that field"
        );
    }

    /// The same schedule on a device that declares nothing still runs, so the
    /// test above is showing a veto rather than a broken fixture.
    #[tokio::test]
    async fn a_device_without_the_veto_still_polls_the_same_field() {
        let (registry, opens) = registry_of_one();
        let store = Arc::new(RecordingStore::default());

        let handle = spawn(
            registry,
            store.clone(),
            SyncConfig {
                fields: fixed(vec![FieldSchedule {
                    name: "FIRMWARE".into(),
                    interval: Some(Duration::from_millis(10)),
                }]),
                fleet: fixed_fleet(),
                reconciler: None,
            },
        );
        wait_for(|| !store.fields().is_empty()).await;
        handle.shutdown().await;

        assert!(opens.load(Ordering::SeqCst) > 0);
    }

    /// Enforcement is core's and applies to every caller, not just to the poll
    /// loops — which is what stops a relay dispatch or a CLI call from asking
    /// for a field the fleet has switched off.
    #[tokio::test]
    async fn a_declared_veto_refuses_a_direct_call_too() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([FIRMWARE_REPLY; 8])
        }));
        let opens = connector.opens_handle();
        let registry = Registry::build(
            vec![config_disabling("vetoed", &["FIRMWARE"])],
            vec![],
            connector,
        );
        let device = registry.device("vetoed").expect("configured");

        let err = device
            .run(&Query::Firmware.instruction())
            .await
            .expect_err("the field is vetoed");
        assert!(
            matches!(
                err,
                DeviceError::Disabled {
                    source: VetoSource::Declared,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            opens.load(Ordering::SeqCst),
            0,
            "a veto is decided before anything is dialed"
        );
    }

    /// `device_config`, with the inference turned on and an optional self-heal
    /// window.
    fn config_inferring(id: &str, after: u32, self_heal: Option<Duration>) -> DeviceConfig {
        DeviceConfig {
            auto_disable_after: after,
            self_heal,
            ..device_config(id)
        }
        .derive_uuid()
    }

    /// A registry over `configs` whose every device refuses every exchange.
    ///
    /// `Exhausted::Stall` behind an ample script: a refusal does not discard the
    /// connection (that is the point of `ControllerError::Rejected`), so one
    /// transport serves the whole test, and a script that ran out would end the
    /// polling for its own reasons and let a broken veto pass.
    fn refusing_registry(configs: Vec<DeviceConfig>) -> Arc<Registry> {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads(["E13\r\n"; 512]).on_exhausted(Exhausted::Stall)
        }));
        Arc::new(Registry::build(configs, vec![], connector))
    }

    /// Start polling `FIRMWARE` fast enough that a hundred milliseconds is many
    /// ticks.
    fn poll_firmware(registry: Arc<Registry>, store: DynWriteStore) -> SyncHandle {
        spawn(
            registry,
            store,
            SyncConfig {
                fields: fixed(vec![FieldSchedule {
                    name: "FIRMWARE".into(),
                    interval: Some(Duration::from_millis(5)),
                }]),
                fleet: fixed_fleet(),
                reconciler: None,
            },
        )
    }

    /// How many refusals have been *counted* for `field`, which is one per
    /// exchange: `AutoDisabled::refused` increments even once the veto is armed,
    /// so a count that stops growing is an exchange that stopped happening.
    fn refusals_of(device: &Device, field: &str) -> u32 {
        device
            .auto_disabled()
            .snapshot()
            .into_iter()
            .find(|f| f.name == field)
            .map_or(0, |f| f.refusals)
    }

    /// The feature, end to end: a device that keeps saying no stops being asked.
    ///
    /// The assertion is that the count *stops*, not merely that a flag is set.
    /// A veto that was recorded but still polled every tick would satisfy any
    /// test written against the learned set's `disabled` alone, and would leave
    /// the exchange this exists to remove exactly where it was.
    #[tokio::test]
    async fn repeated_refusals_auto_disable_the_field_and_stop_the_exchanges() {
        let registry = refusing_registry(vec![config_inferring("refuser", 2, None)]);
        let device = registry.device("refuser").expect("configured");
        let store = Arc::new(RecordingStore::default());

        let handle = poll_firmware(Arc::clone(&registry), store.clone());
        wait_for(|| device.auto_disabled().veto("FIRMWARE").is_some()).await;
        let at_veto = refusals_of(&device, "FIRMWARE");
        // Many more ticks at a 5ms interval, had any been taken.
        tokio::time::sleep(Duration::from_millis(150)).await;
        handle.shutdown().await;

        assert_eq!(
            at_veto, 2,
            "the threshold is two consecutive refusals, so the veto arms on the second"
        );
        assert_eq!(
            refusals_of(&device, "FIRMWARE"),
            at_veto,
            "nothing may be asked of a field once it is auto-disabled"
        );
        assert!(
            store.fields().is_empty(),
            "a refusal carries no value to store"
        );
    }

    /// With `self_heal_secs` unset the loop has no future work, so it ends
    /// rather than waking forever to skip an exchange it will never make.
    #[tokio::test]
    async fn a_veto_with_no_retry_ends_the_poll_loop() {
        let registry = refusing_registry(vec![config_inferring("refuser", 1, None)]);
        let device = registry.device("refuser").expect("configured");

        let handle = poll_firmware(Arc::clone(&registry), Arc::new(RecordingStore::default()));
        wait_for(|| device.auto_disabled().veto("FIRMWARE").is_some()).await;

        // A stopped loop is one `shutdown` has nothing to wait for. A loop still
        // parked on its ticker would also drain quickly, so this is a guard
        // against the loop having wedged rather than a proof it ended — which
        // the count above is.
        tokio::time::timeout(Duration::from_secs(2), handle.shutdown())
            .await
            .expect("the loops should drain");
    }

    /// The inference is opt-out: at zero nothing is counted, so a device that
    /// refuses forever keeps being asked forever — the behavior before this
    /// existed, and the one a deployment that wants only declared vetoes gets.
    ///
    /// Two devices under one schedule, because the assertion is about *absence*:
    /// `strict` is the control arm that proves the loops ran at all, so a
    /// version of this that polled nothing cannot pass by doing nothing.
    #[tokio::test]
    async fn a_threshold_of_zero_never_auto_disables() {
        let registry = refusing_registry(vec![
            config_inferring("strict", 2, None),
            config_inferring("never", 0, None),
        ]);
        let strict = registry.device("strict").expect("configured");
        let never = registry.device("never").expect("configured");

        let handle = poll_firmware(Arc::clone(&registry), Arc::new(RecordingStore::default()));
        wait_for(|| strict.auto_disabled().veto("FIRMWARE").is_some()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.shutdown().await;

        assert_eq!(
            never.auto_disabled().veto("FIRMWARE"),
            None,
            "nothing should have been inferred at a threshold of zero"
        );
        assert!(
            never.auto_disabled().snapshot().is_empty(),
            "and nothing should have been recorded either"
        );
    }

    /// `self_heal_secs` end to end: a field that was auto-disabled is tried
    /// again once its window closes, and an answer puts it back in service.
    ///
    /// The device refuses twice and answers thereafter, which is the shape of a
    /// license applied in the field without a restart — the case the setting
    /// exists for. Real time rather than a paused clock, because what is being
    /// tested is that the *poll loop* performs the retry, and pausing the clock
    /// would stop the very ticker doing it.
    #[tokio::test]
    async fn a_self_healing_field_is_retried_and_comes_back() {
        let mut script = vec!["E13\r\n", "E13\r\n"];
        script.extend(["2.11\r\n"; 64]);
        let connector = Arc::new(CountingConnector::new(move || {
            FakeTransport::with_reads(script.clone()).on_exhausted(Exhausted::Stall)
        }));
        let registry = Arc::new(Registry::build(
            vec![config_inferring(
                "healer",
                2,
                Some(Duration::from_millis(60)),
            )],
            vec![],
            connector,
        ));
        let device = registry.device("healer").expect("configured");
        let store = Arc::new(RecordingStore::default());

        let handle = poll_firmware(Arc::clone(&registry), store.clone());

        wait_for(|| device.auto_disabled().veto("FIRMWARE").is_some()).await;
        assert!(
            store.fields().is_empty(),
            "nothing has answered yet, so nothing should be stored"
        );

        // The loop is still ticking — that is what the window costs, and what
        // performs the retry — so the field comes back with no further help.
        wait_for(|| !store.fields().is_empty()).await;
        handle.shutdown().await;

        assert_eq!(
            device.auto_disabled().veto("FIRMWARE"),
            None,
            "an answer clears the veto"
        );
        assert!(
            device.auto_disabled().snapshot().is_empty(),
            "and clears the count with it, so the next refusal starts afresh"
        );
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
                fleet: fixed_fleet(),
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
