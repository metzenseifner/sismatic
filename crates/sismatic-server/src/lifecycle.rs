//! The store's retention policy, and the task that enforces it.
//!
//! [`sismatic_store::lifecycle::Lifecycle`] takes an *instant* and deletes what
//! is older. This module is the other half: what instant, how often, and what
//! to say about the result. The split is deliberate — the adapter's half is a
//! pure function of its argument and testable without a clock, and this half is
//! a pure function of the configuration and testable without a store.
//!
//! # Why the sweeper lives in the composition root
//!
//! `sismatic-sync` and `sismatic-intent-relay` are crates because each is a
//! *policy* over devices that a different deployment might want differently, and
//! each needs to name a `Device`. A sweeper names nothing: it is one timer over
//! one port, and the only thing it knows that no library does is which object
//! is behind that port. That is the composition root's own knowledge, and the
//! same reason [`status::RegistryStatus`](crate::status::RegistryStatus) lives
//! here rather than in `sismatic-store-memory`.
//!
//! # The two axes, and which one is load-bearing
//!
//! `retain` bounds history *in time* and is the setting an operator reasons
//! about: it says how far back a dashboard can look. `max_memory` bounds it *in
//! bytes* and is the setting that keeps the process alive, because the bytes a
//! retention window costs depend on the fleet size and the poll schedule —
//! numbers the operator setting `retain: 30d` is not holding in their head.
//!
//! So the budget is the backstop, not the plan. When it starts doing work the
//! store is silently keeping less history than the config asks for, and that is
//! worth saying out loud rather than discovering from a graph that stops early
//! — which is what [`Report::Evicting`] exists for.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::de;
use serde::{Deserialize, Deserializer};
use sismatic_api_types::Timestamp;
use sismatic_store::lifecycle::{DynLifecycle, Lifecycle, Usage};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::configuration::StoreConfig;
use crate::units;

/// How far back the store keeps recorded reads.
///
/// A sum type rather than one nullable duration, because the two bounded forms
/// answer different questions and behave differently over time. [`Age`] is a
/// *rolling* window — "the last thirty days", whose meaning moves with the
/// clock — and is what a deployment should almost always write, because it is
/// the only form whose memory cost is stationary. [`Since`] is a *fixed* floor —
/// "nothing from before term started" — which is the right answer for an
/// archival window an operator will move by hand, and the wrong answer for
/// everything else: its cost grows without bound as wall-clock time advances
/// past it, so it needs `max_memory` under it to be safe.
///
/// [`Age`]: Retention::Age
/// [`Since`]: Retention::Since
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retention {
    /// Never expire anything. History is then bounded only by `max_memory`.
    Forever,
    /// Keep the last `Duration` of history, measured from now at each sweep.
    Age(Duration),
    /// Keep everything stamped at or after a fixed instant.
    ///
    /// Held as the wire [`Timestamp`] rather than a `DateTime`, rendered once at
    /// parse time in exactly the format the sync driver stamps reads with. That
    /// is what lets the store compare the two as plain strings — the property
    /// [`TimeSpan::within`](sismatic_api_types::TimeSpan::within) already rests
    /// on — instead of parsing a date on every entry of every sweep.
    Since(Timestamp),
}

impl Retention {
    /// The oldest instant worth keeping, as of `now`; `None` to keep everything.
    ///
    /// Pure, and the whole of the policy: everything about *when* a sweep runs
    /// is the caller's, and everything about *what* it deletes is the store's.
    #[must_use]
    pub fn cutoff(&self, now: DateTime<Utc>) -> Option<Timestamp> {
        match self {
            Retention::Forever => None,
            // An underflow means the window reaches further back than the
            // calendar does, which is a request to keep everything — and is the
            // only sane reading, since the alternative renders a negative year
            // that no longer sorts against a real timestamp as a string.
            Retention::Age(age) => now
                .checked_sub_signed(chrono::TimeDelta::from_std(*age).ok()?)
                .map(stamp),
            Retention::Since(floor) => Some(floor.clone()),
        }
    }
}

/// Render an instant the way the sync driver stamps a read, which is the format
/// the store's string comparison assumes on both sides.
fn stamp(at: DateTime<Utc>) -> Timestamp {
    Timestamp(at.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// Reads back as the operator would have written it, so a startup log answers
/// "what did it decide `retain` was" without them re-deriving it from seconds.
impl fmt::Display for Retention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Retention::Forever => f.write_str("forever"),
            Retention::Age(age) => write!(f, "{}", humantime::format_duration(*age)),
            Retention::Since(floor) => write!(f, "since {floor}"),
        }
    }
}

/// The one sentinel, spelled as the word rather than as a number — see the
/// visitor below for why a bare `0` is refused here and accepted everywhere
/// else in the document.
///
/// Shared with [`configuration`](crate::configuration), which renders a
/// retention back out as text a caller can send in again: the word that parses
/// and the word that is written have to be one constant, or the API's own output
/// eventually stops being valid input.
pub(crate) const FOREVER: &str = "forever";

/// Hand-written rather than `#[serde(untagged)]` for the reason
/// [`RawField`](crate::configuration::RawField) is: untagged reports every
/// failure as "data did not match any variant", and the message an operator
/// needs here is which of the three forms their text failed to be.
impl<'de> Deserialize<'de> for Retention {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RetentionVisitor;

        impl de::Visitor<'_> for RetentionVisitor {
            type Value = Retention;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a duration (30d, 2 weeks, 1h 30min), an instant \
                     (2026-01-01T00:00:00Z, 2026-01-01), or 'forever'",
                )
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Retention, E> {
                let text = text.trim();
                if text.eq_ignore_ascii_case(FOREVER) {
                    return Ok(Retention::Forever);
                }

                // Duration first, then instant. The two vocabularies are
                // disjoint — `30d` is not a date and `2026-01-01` is not a
                // duration — so the order decides nothing but which error is
                // reported when the text is neither.
                match units::duration(text) {
                    Ok(age) => finite(age),
                    Err(duration_err) => match units::instant(text) {
                        Ok(floor) => Ok(Retention::Since(stamp(floor))),
                        // Both, because a typo's nearest correct spelling might
                        // be in either family and this is the one line an
                        // operator gets.
                        Err(instant_err) => Err(E::custom(format!(
                            "invalid retention: {duration_err}; and {instant_err}"
                        ))),
                    },
                }
            }

            /// Seconds, as every other delay in this document is — the form
            /// the environment produces on its own, since `config`'s
            /// type-guessing turns `RETAIN=2592000` into an integer.
            fn visit_u64<E: de::Error>(self, secs: u64) -> Result<Retention, E> {
                finite(Duration::from_secs(secs))
            }

            fn visit_i64<E: de::Error>(self, secs: i64) -> Result<Retention, E> {
                let secs = u64::try_from(secs)
                    .map_err(|_| E::custom(format!("retention cannot be negative: {secs}")))?;
                self.visit_u64(secs)
            }
        }

        deserializer.deserialize_any(RetentionVisitor)
    }
}

/// Refuse a zero window, and say what the two things it might have meant are
/// called.
///
/// The one place `[store]` breaks the document's `0`-means-off convention, and
/// it breaks it on purpose. Read as "off", `retain: 0` would switch off
/// *expiry* and retain everything; read literally it would retain nothing. The
/// two are opposite, one of them is the unbounded growth this whole section
/// exists to prevent, and nothing in the text says which was meant — so it is
/// an error naming both spellings rather than a coin flip an operator discovers
/// from a memory graph a week later.
fn finite<E: de::Error>(age: Duration) -> Result<Retention, E> {
    if age.is_zero() {
        return Err(E::custom(
            "a zero retention window is ambiguous: write 'forever' to keep \
             everything, or a duration such as '1h' to keep almost nothing",
        ));
    }
    Ok(Retention::Age(age))
}

/// Owns the running sweep task. Call [`SweeperHandle::shutdown`] to stop it.
///
/// One task, always — where a disabled sweep used to start none at all. The old
/// shape was the better one while the policy was fixed at startup: nothing is
/// clearer about what is running than nothing running. It stops being available
/// once `store.cleanup_interval` can be set to a real delay by a `PATCH`, since
/// the task that would start ticking has to already exist to hear about it. What
/// is disabled now is the *ticker* rather than the task, and the sweeper says
/// which of the two it is in the line it logs.
pub struct SweeperHandle {
    task: JoinHandle<()>,
    cancel: CancellationToken,
}

impl SweeperHandle {
    /// Signal the sweep to stop and wait for an in-flight pass to finish.
    ///
    /// Cooperative, as the other two handles' shutdowns are. A pass that is
    /// midway through applying a sweep completes it: abandoning one would leave
    /// the ledger settled for entries still in the maps, which is the one state
    /// the adapter's accounting cannot repair.
    ///
    /// `skip(self)` because [`SweeperHandle`] is not `Debug`, and
    /// `#[instrument]` records every argument unless told otherwise.
    #[instrument(name = "store_sweeper_shutdown", skip(self))]
    pub async fn shutdown(self) {
        self.cancel.cancel();
        let _ = self.task.await;
        info!("store sweeper stopped");
    }
}

/// Start the sweep and return a handle to it.
///
/// Must be called from within a Tokio runtime (it uses [`tokio::spawn`]).
///
/// `policy` carries both axes the sweeper reads — how far back to keep, and how
/// often to enforce it — and one it does not: `max_memory` is enforced by the
/// adapter on the write path, so it travels in the same struct and is applied by
/// the composition root rather than here. See [`StoreConfig`].
///
/// A `cleanup` of `None` starts the task with no ticker. That is a supported
/// deployment — a store bounded by `max_memory` alone still cannot exhaust the
/// machine — but it is not a quiet one: without a sweep, history is kept until
/// the budget pushes it out, so `retain` describes nothing and the oldest
/// reading a dashboard can reach becomes a function of the poll schedule.
pub fn spawn(store: DynLifecycle, policy: watch::Receiver<StoreConfig>) -> SweeperHandle {
    let cancel = CancellationToken::new();
    let task = tokio::spawn(sweep_loop(store, policy, cancel.clone()));
    SweeperHandle { task, cancel }
}

/// A policy that will never change: the one value, and no sender behind it.
///
/// The sweeper's counterpart of [`sismatic_sync::fixed`], and the same reading
/// of a closed channel — see [`sweep_loop`].
#[must_use]
pub fn fixed(policy: StoreConfig) -> watch::Receiver<StoreConfig> {
    watch::channel(policy).1
}

/// Sweep on the published interval until cancelled, re-pacing whenever the
/// policy moves.
///
/// The first tick of a `tokio::time::interval` fires immediately, and that is
/// wanted rather than tolerated: a process restarting into a store that has
/// just been repopulated from a long-running predecessor should not wait a full
/// interval before enforcing the window. Since the adapter's prune is
/// idempotent, an immediate first pass on an empty store costs one lock. The
/// same property is what makes re-pacing cheap — a shortened window is enforced
/// at once rather than at the end of the interval that was already running.
///
/// The outer loop is one policy, as in the relay: a `tokio` interval's period is
/// fixed at construction, so a new one is a new pass. Between passes there may
/// be no ticker at all, which is what `cleanup: never` looks like from in here —
/// the task waits on a change and on nothing else.
#[instrument(name = "store_sweeper", skip_all)]
async fn sweep_loop(
    store: DynLifecycle,
    mut policy: watch::Receiver<StoreConfig>,
    cancel: CancellationToken,
) {
    // What the budget's cumulative counter read at the last tick, so this loop
    // can report the *delta* — what eviction did during this interval — rather
    // than a total that would go on being alarming long after the burst that
    // caused it. Outside the outer loop, so a re-paced sweeper does not report
    // the whole process's eviction history as this interval's.
    let mut seen_evicted = 0;

    // Whether anyone can still publish a policy. A closed channel is a
    // deployment whose policy was decided once — see [`fixed`] — and the answer
    // is to stop asking rather than to spin on a `changed()` that returns
    // immediately forever.
    let mut watching = true;

    'repace: loop {
        let current = policy.borrow_and_update().clone();
        announce_policy(&current);

        let mut ticker = current.cleanup.map(|every| {
            let mut ticker = tokio::time::interval(every);
            // Re-pace from completion rather than firing a burst of catch-up
            // ticks, for the same reason the poll loops do: a sweep that ran
            // long was competing for the store's locks, and the answer to that
            // is not to sweep more.
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            ticker
        });

        loop {
            tokio::select! {
                () = cancel.cancelled() => break 'repace,
                changed = policy.changed(), if watching => match changed {
                    Ok(()) => continue 'repace,
                    Err(_) => watching = false,
                },
                () = tick(ticker.as_mut()) =>
                    sweep(store.as_ref(), &current.retain, &mut seen_evicted).await,
            }
        }
    }

    info!("sweep loop stopped");
}

/// The next tick of `ticker`, or never.
///
/// A function rather than a `select!` precondition on `ticker.is_some()`,
/// because "no ticker" and "a ticker that has not fired" have to be the same
/// thing to the loop above: a disabled sweep still has to wait on the other two
/// branches, and a `select!` whose every branch is disabled panics.
async fn tick(ticker: Option<&mut tokio::time::Interval>) {
    match ticker {
        Some(ticker) => {
            ticker.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Say what a policy about to take effect will do — at startup, and again each
/// time one is published.
///
/// A disabled sweep warns rather than informs, and it does so on every pass: it
/// is a state a deployment can sit in for months, and the one line saying so
/// would otherwise be a startup message nobody has scrolled back to.
fn announce_policy(policy: &StoreConfig) {
    match policy.cleanup {
        None => warn!(
            retain = %policy.retain,
            "store cleanup is disabled; recorded history will be bounded by the memory budget alone"
        ),
        Some(interval) => info!(
            retain = %policy.retain,
            interval_secs = interval.as_secs(),
            "store sweeper running"
        ),
    }
}

/// One pass: expire what the window puts out of scope, then say how full the
/// store is.
///
/// The usage reading is taken whether or not anything expired, because the tick
/// that pruned nothing is the one an operator watching a budget most needs to
/// see.
async fn sweep(store: &dyn Lifecycle, retain: &Retention, seen_evicted: &mut u64) {
    if let Some(cutoff) = retain.cutoff(Utc::now()) {
        match store.prune(cutoff.clone()).await {
            Ok(swept) if swept.entries > 0 => info!(
                entries = swept.entries,
                bytes = swept.bytes,
                %cutoff,
                "expired reads removed from the store"
            ),
            Ok(_) => debug!(%cutoff, "nothing had expired"),
            // A failed sweep is the steady state, not an exception: the next
            // tick tries again, and the budget is still holding the line in the
            // meantime.
            Err(err) => warn!(%err, "the store refused to prune"),
        }
    }

    let usage = match store.usage().await {
        Ok(usage) => usage,
        Err(err) => {
            warn!(%err, "could not read the store's usage");
            return;
        }
    };

    announce(report(&usage, *seen_evicted), &usage);
    *seen_evicted = usage.evicted;
}

/// What a tick's usage reading is worth saying.
///
/// A pure function over `(reading, what the last tick saw)`, decided before
/// anything is emitted — the same shape `sismatic_sync::driver::step` uses, and
/// for the same reason: the interesting question is which of these three states
/// a deployment is in, and that is worth being able to assert directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Report {
    /// Within budget, and the budget dropped nothing this interval. The
    /// configured retention is what is bounding history, which is the intended
    /// arrangement.
    Quiet,
    /// The budget dropped `entries` unexpired reads this interval, so history
    /// is shorter than `retain` asks for. Either the budget wants raising or
    /// the window wants shortening; nothing here can tell which.
    Evicting { entries: u64 },
    /// Eviction has run out of things to evict and the store is still over
    /// budget — the budget is below the floor `latest` occupies, which no
    /// amount of deleting history will fix.
    OverBudget,
}

fn report(usage: &Usage, seen_evicted: u64) -> Report {
    if usage.over_budget() {
        Report::OverBudget
    } else if usage.evicted > seen_evicted {
        Report::Evicting {
            entries: usage.evicted - seen_evicted,
        }
    } else {
        Report::Quiet
    }
}

/// Emit what [`report`] decided, with the reading that produced it.
fn announce(report: Report, usage: &Usage) {
    match report {
        Report::Quiet => debug!(bytes = usage.bytes, entries = usage.entries, "store swept"),
        Report::Evicting { entries } => warn!(
            evicted = entries,
            bytes = usage.bytes,
            budget = usage.budget,
            "the memory budget is discarding reads the retention window would have kept; \
             raise store.max_memory or shorten store.retain"
        ),
        Report::OverBudget => warn!(
            bytes = usage.bytes,
            budget = usage.budget,
            "the store cannot fit within store.max_memory even with no history at all; \
             the budget is below what one reading per polled field costs"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sismatic_store::lifecycle::Swept;

    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        units::instant(text).expect("a test instant")
    }

    #[test]
    fn forever_has_no_cutoff() {
        assert_eq!(Retention::Forever.cutoff(at("2026-09-07T12:00:00Z")), None);
    }

    #[test]
    fn an_age_is_measured_back_from_now() {
        let cutoff = Retention::Age(Duration::from_secs(86_400))
            .cutoff(at("2026-09-07T12:00:00Z"))
            .expect("a bounded window has a cutoff");
        assert_eq!(cutoff, Timestamp("2026-09-06T12:00:00.000Z".into()));
    }

    #[test]
    fn an_age_cutoff_moves_with_the_clock() {
        // The property that makes `Age` the form with a stationary memory cost:
        // an hour later the window has slid an hour, so the amount inside it is
        // whatever one window's worth of polling produces, forever.
        let retention = Retention::Age(Duration::from_secs(3_600));
        let first = retention.cutoff(at("2026-09-07T12:00:00Z")).unwrap();
        let later = retention.cutoff(at("2026-09-07T13:00:00Z")).unwrap();
        assert_eq!(first, Timestamp("2026-09-07T11:00:00.000Z".into()));
        assert_eq!(later, Timestamp("2026-09-07T12:00:00.000Z".into()));
    }

    #[test]
    fn a_fixed_floor_does_not_move() {
        // ...and this is why it needs a budget under it: the window widens by a
        // day every day.
        let retention = Retention::Since(Timestamp("2026-01-01T00:00:00.000Z".into()));
        assert_eq!(
            retention.cutoff(at("2026-09-07T12:00:00Z")),
            retention.cutoff(at("2027-09-07T12:00:00Z")),
        );
    }

    #[test]
    fn a_window_older_than_the_calendar_keeps_everything() {
        // `Duration` reaches far past what a `DateTime` can subtract from. The
        // answer has to be `None` rather than a rendered negative year, which
        // would no longer sort against a real timestamp as a string — and would
        // therefore expire the entire store.
        let forever_ish = Retention::Age(Duration::from_secs(u64::MAX));
        assert_eq!(forever_ish.cutoff(at("2026-09-07T12:00:00Z")), None);
    }

    #[test]
    fn a_cutoff_is_rendered_the_way_a_stored_read_is() {
        // The load-bearing format agreement: the store compares these as plain
        // strings, so a cutoff written with a different precision would compare
        // wrong at the boundary. Millisecond precision, `Z`, as
        // `sismatic_server::stamp` and the sync driver both write.
        let cutoff = Retention::Age(Duration::from_secs(1))
            .cutoff(at("2026-09-07T12:00:00Z"))
            .unwrap();
        assert_eq!(cutoff.as_str(), "2026-09-07T11:59:59.000Z");
        // Lexicographic order is chronological order, which is the property the
        // whole comparison rests on.
        assert!(cutoff.as_str() < "2026-09-07T12:00:00.000Z");
    }

    #[test]
    fn retention_reads_back_as_it_was_written() {
        assert_eq!(Retention::Forever.to_string(), "forever");
        assert_eq!(
            Retention::Age(Duration::from_secs(30 * 86_400)).to_string(),
            "30days"
        );
        assert_eq!(
            Retention::Since(Timestamp("2026-01-01T00:00:00.000Z".into())).to_string(),
            "since 2026-01-01T00:00:00.000Z"
        );
    }

    // ---- what a tick reports ---------------------------------------------

    fn usage(bytes: u64, budget: Option<u64>, evicted: u64) -> Usage {
        Usage {
            bytes,
            entries: 0,
            budget,
            evicted,
        }
    }

    #[test]
    fn a_store_inside_its_budget_is_quiet() {
        assert_eq!(report(&usage(100, Some(1_000), 0), 0), Report::Quiet);
        // ...including one that is unbounded, which has no budget to be over.
        assert_eq!(report(&usage(u64::MAX, None, 0), 0), Report::Quiet);
    }

    #[test]
    fn eviction_since_the_last_tick_is_reported_as_a_delta() {
        // A cumulative total would go on being alarming long after the burst
        // that produced it, and the operator's question is whether the budget is
        // biting *now*.
        assert_eq!(
            report(&usage(100, Some(1_000), 70), 40),
            Report::Evicting { entries: 30 }
        );
    }

    #[test]
    fn a_quiet_interval_after_an_evicting_one_goes_quiet_again() {
        assert_eq!(report(&usage(100, Some(1_000), 70), 70), Report::Quiet);
    }

    #[test]
    fn being_over_budget_outranks_the_eviction_delta() {
        // Both are true when a budget is below the `latest` floor — every write
        // evicts and it is still over — and only one of them is actionable.
        assert_eq!(
            report(&usage(2_000, Some(1_000), 70), 40),
            Report::OverBudget
        );
    }

    // ---- a policy that changes under the sweeper --------------------------

    /// A [`Lifecycle`] that counts what it was asked to do and nothing else.
    ///
    /// The sweep loop is a timer over a port, so a double that records is the
    /// whole of what a test of it needs: how *often* it swept is the only
    /// question the loop answers, and what a sweep removes is the adapter's,
    /// tested in `sismatic-store-memory` over real entries.
    #[derive(Default)]
    struct CountingLifecycle {
        pruned: std::sync::atomic::AtomicU64,
    }

    impl CountingLifecycle {
        fn pruned(&self) -> u64 {
            self.pruned.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Lifecycle for CountingLifecycle {
        async fn prune(&self, _before: Timestamp) -> Result<Swept, sismatic_store::WriteError> {
            self.pruned
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Swept::default())
        }

        async fn usage(&self) -> Result<Usage, sismatic_store::WriteError> {
            Ok(Usage::default())
        }
    }

    fn policy(cleanup: Option<Duration>) -> StoreConfig {
        StoreConfig {
            retain: Retention::Age(Duration::from_secs(3_600)),
            cleanup,
            max_memory: None,
        }
    }

    /// Poll `cond` until it holds, or panic after ~2s — the same shape the sync
    /// driver's tests use, so a sweeper that never ticks fails rather than
    /// racing a fixed sleep.
    async fn wait_for(cond: impl Fn() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not met in time");
    }

    /// The transition the old shape could not express at all: a sweeper that
    /// started disabled has to start sweeping when one is configured. Under the
    /// previous design there was no task to tell — `cleanup: never` started
    /// nothing — so this is the property that decided the handle's shape.
    #[tokio::test]
    async fn a_disabled_sweeper_starts_ticking_when_an_interval_arrives() {
        let store = Arc::new(CountingLifecycle::default());
        let (publish, policies) = watch::channel(policy(None));
        let sweeper = spawn(store.clone(), policies);

        // Nothing at all while it is disabled: the immediate first tick a
        // `tokio` interval fires must not happen when there is no interval.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(store.pruned(), 0, "a disabled sweeper must not sweep");

        publish
            .send(policy(Some(Duration::from_millis(10))))
            .expect("the sweeper is listening");

        wait_for(|| store.pruned() >= 3).await;
        sweeper.shutdown().await;
    }

    /// ...and back again, which is the half that has to *stop* a ticker rather
    /// than build one.
    #[tokio::test]
    async fn a_sweeper_switched_off_stops_sweeping() {
        let store = Arc::new(CountingLifecycle::default());
        let (publish, policies) = watch::channel(policy(Some(Duration::from_millis(5))));
        let sweeper = spawn(store.clone(), policies);

        wait_for(|| store.pruned() >= 3).await;
        publish
            .send(policy(None))
            .expect("the sweeper is listening");

        // Settle first, then measure: a pass already running completes, so what
        // must be true is that the count stops moving rather than that it froze
        // at the instant of the send.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let settled = store.pruned();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(store.pruned(), settled, "a disabled sweeper should be idle");
        sweeper.shutdown().await;
    }

    /// A shortened window is enforced on the next tick rather than at the end of
    /// the interval that was already running — the property that makes
    /// `PATCH /v1/config` worth having for an operator under memory pressure.
    #[tokio::test]
    async fn a_re_paced_sweeper_uses_the_new_interval_at_once() {
        let store = Arc::new(CountingLifecycle::default());
        let (publish, policies) = watch::channel(policy(Some(Duration::from_secs(3_600))));
        let sweeper = spawn(store.clone(), policies);

        // The immediate first tick of an hourly sweeper, and then nothing.
        wait_for(|| store.pruned() >= 1).await;
        assert_eq!(store.pruned(), 1);

        publish
            .send(policy(Some(Duration::from_millis(10))))
            .expect("the sweeper is listening");

        // Four more inside the timeout is impossible at an hour.
        wait_for(|| store.pruned() >= 5).await;
        sweeper.shutdown().await;
    }

    /// A publisher that goes away is not a failure: the sweeper keeps the policy
    /// it has. The deployment `fixed` produces, and the shape every caller had
    /// before the policy could move.
    #[tokio::test]
    async fn a_dropped_publisher_leaves_the_sweeper_running() {
        let store = Arc::new(CountingLifecycle::default());
        let sweeper = spawn(store.clone(), fixed(policy(Some(Duration::from_millis(5)))));

        wait_for(|| store.pruned() >= 5).await;
        sweeper.shutdown().await;
    }
}
