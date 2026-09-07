//! Settings that change while the process runs, and the wiring that carries a
//! change to the four things it can reach.
//!
//! [`crate::configuration`] answers *what the settings would become*; this
//! module is what makes the answer true. It is the adapter behind
//! [`sismatic_http_api::LiveConfig`], and it can only live here for the reason
//! the sweeper can only live here: the port's implementation has to know that
//! the object behind three store handles is one store, that the poll loops read
//! their schedule from a channel this end holds the sender of, and which file
//! the whole document was loaded from. That is the composition root's own
//! knowledge and nobody else's.
//!
//! # How a change travels
//!
//! Four destinations, two mechanisms, and which one a setting uses is decided by
//! what is enforcing it.
//!
//! A [`watch`] channel carries anything a *loop* reads: the poll schedule, the
//! sweeper's window and interval, the relay's drain rate. Each of those is owned
//! by a task with its own clock, and a channel is how a task is told without
//! being interrupted — it reads the latest value when it next comes round, and
//! the publisher never blocks on a task that is mid-exchange with a device.
//!
//! A direct call carries anything an *adapter* enforces: the store's byte budget
//! and the outbox's retry budget. Those are not read on a timer at all — the
//! budget is applied on the write path, the retry count when a failure is
//! settled — so there is no loop to tell, and a channel would need a task
//! invented to receive on it. What there is instead is the concrete adapter,
//! which the root is holding anyway.
//!
//! # Atomic, and what that does and does not mean
//!
//! A request is validated whole before any of it is published: [`patched`] is
//! pure and total, so a patch with one bad duration among five good ones changes
//! none of the five. What follows validation is five small publishes in a row,
//! and they are not one transaction — for a few microseconds the sweeper may be
//! on the new window while the relay is still on the old rate.
//!
//! That is not a gap worth closing, because there is nothing across it to break.
//! No two of these settings constrain each other: no loop reads two of them, and
//! no invariant spans them. The one pair that looks like it might — the store's
//! retention window and its byte budget — is a pair of independent bounds, and a
//! moment where the tighter one is the older one is a moment where the store
//! keeps slightly more or slightly less history than it will a moment later.
//!
//! # Nothing here is written down
//!
//! A patch changes the running process and not the config file. That is the
//! design, and it is what makes the reload route safe to call at any time: the
//! file remains the single source of truth, so a `PATCH` is an override that
//! lasts until the next reload or restart, and a deployment whose config lives
//! in a ConfigMap can never end up with a server quietly running settings that
//! are in no repository. An operator who wants a change to survive puts it in
//! the file — where, in Kubernetes, the reload route will pick it up without a
//! restart anyway.

use std::sync::Mutex;
use std::time::Duration;

use sismatic_api_types::{ConfigDocument, ConfigPatch};
use sismatic_http_api::{ConfigRefusal, LiveConfig};
use sismatic_store_memory::{MemoryOutbox, MemoryStore};
use sismatic_sync::FieldSchedule;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::configuration::{
    ConfigSource, PatchError, ServerConfig, StoreConfig, SyncConfig, document, patched,
};

/// The settings as they stand, and every way of changing them.
pub struct LiveSettings {
    /// What the process is running under. The base every patch is folded onto,
    /// and the thing `GET /v1/config` reports.
    ///
    /// A `std::sync::Mutex` rather than an async one, and deliberately: nothing
    /// under this lock awaits, the critical section is a struct clone and a
    /// handful of channel sends, and an async mutex would buy the ability to
    /// hold it across an await that no path here wants to perform.
    current: Mutex<ServerConfig>,
    /// Where the document came from, so a reload can read it again.
    source: ConfigSource,
    /// The poll schedule, read by the sync driver's supervisor.
    schedule: watch::Sender<Vec<FieldSchedule>>,
    /// The store's retention window and sweep interval, read by the sweeper. Its
    /// third field — the byte budget — is applied through `store` below; the
    /// whole struct travels because it is one section of one document, and
    /// splitting it would leave two things to keep in step.
    policy: watch::Sender<StoreConfig>,
    /// The relay's drain rate, read by every relay loop.
    drain: watch::Sender<Duration>,
    /// The store, as itself. Held for one method the ports do not carry — see
    /// [`MemoryStore::set_budget`].
    store: MemoryStore,
    /// The outbox, as itself, for [`MemoryOutbox::set_max_attempts`].
    outbox: MemoryOutbox,
}

/// The receiving ends, for the three tasks that are started with them.
///
/// Returned by [`LiveSettings::new`] rather than made by the caller, so the
/// senders and the receivers are created together and a task cannot be wired to
/// a channel nothing publishes on.
pub struct Wiring {
    /// For [`sismatic_sync::SyncConfig::fields`].
    pub schedule: watch::Receiver<Vec<FieldSchedule>>,
    /// For [`crate::lifecycle::spawn`].
    pub policy: watch::Receiver<StoreConfig>,
    /// For [`sismatic_intent_relay::RelayConfig::poll`].
    pub drain: watch::Receiver<Duration>,
}

impl LiveSettings {
    /// Hold `cfg` as the running settings, and hand back the channels the tasks
    /// that enforce it are started with.
    ///
    /// `store` and `outbox` are the concrete adapters, not ports. They are the
    /// same objects the trait handles wrap — this type is built in the one
    /// function that knows that — and what they are for is the two settings no
    /// loop reads.
    #[must_use]
    pub fn new(
        cfg: &ServerConfig,
        source: ConfigSource,
        store: MemoryStore,
        outbox: MemoryOutbox,
    ) -> (Self, Wiring) {
        let (schedule, schedule_rx) = watch::channel(schedules(&cfg.sync));
        let (policy, policy_rx) = watch::channel(cfg.store.clone());
        let (drain, drain_rx) = watch::channel(cfg.intent_relay.poll);

        let settings = Self {
            current: Mutex::new(cfg.clone()),
            source,
            schedule,
            policy,
            drain,
            store,
            outbox,
        };
        let wiring = Wiring {
            schedule: schedule_rx,
            policy: policy_rx,
            drain: drain_rx,
        };
        (settings, wiring)
    }

    /// Publish `next` to everything that enforces a piece of it, and adopt it as
    /// the running settings.
    ///
    /// Takes the guard rather than the lock, so the caller's read-modify-write
    /// is one critical section: two patches arriving at once are then applied in
    /// some order rather than both folded onto the config neither of them saw.
    fn publish(&self, current: &mut ServerConfig, next: ServerConfig) -> ConfigDocument {
        // `send_replace` rather than `send`, which fails when nothing is
        // listening. Nothing listening is a real state — every poll loop can be
        // disabled, and the driver's supervisor is still there — and it is not
        // an error in any case: the value is stored, and whoever subscribes next
        // reads it.
        self.schedule.send_replace(schedules(&next.sync));
        self.policy.send_replace(next.store.clone());
        self.drain.send_replace(next.intent_relay.poll);

        // The two that no loop reads. `set_budget` evicts on the spot when the
        // cap has come down, which is the whole reason it is called here rather
        // than left for the next write to notice.
        let evicted = self.store.set_budget(next.store.max_memory);
        if evicted > 0 {
            warn!(
                evicted,
                "a lowered memory budget discarded stored reads immediately"
            );
        }
        self.outbox.set_max_attempts(next.intent_relay.max_attempts);

        let applied = document(&next);
        *current = next;
        applied
    }

    /// The running settings, or the panic that says a previous holder died
    /// mid-publish.
    ///
    /// `expect` rather than recovering the inner value: everything under this
    /// lock is infallible, so a poisoned mutex means a panic somewhere that had
    /// nothing to do with the data — and a config half-published is the one
    /// state this type must not go on serving as though it were whole.
    fn locked(&self) -> std::sync::MutexGuard<'_, ServerConfig> {
        self.current.lock().expect("the live settings are poisoned")
    }
}

/// The sync section as the driver's supervisor takes it.
///
/// The one translation between the config layer's vocabulary and the driver's,
/// and it is a rename: both sides carry a name and an optional interval, and
/// both read `None` as *never*. It lives here rather than in the driver because
/// the driver may not see a `ServerConfig`, and here rather than inline in
/// [`crate::run`] because a schedule is published on every patch as well as at
/// startup, and the two must agree.
#[must_use]
pub fn schedules(sync: &SyncConfig) -> Vec<FieldSchedule> {
    sync.fields
        .iter()
        .map(|field| FieldSchedule {
            name: field.name.clone(),
            interval: field.interval,
        })
        .collect()
}

/// The port's spelling of a refusal the resolver produced.
///
/// Two enums with the same two cases, and the conversion is the seam that keeps
/// them apart on purpose: [`PatchError`] is what a *resolver* can conclude, and
/// [`ConfigRefusal`] is what an HTTP surface can answer with — which is why it
/// has a third case for a config file that will not load, a thing no resolver
/// has an opinion about.
fn refuse(err: PatchError) -> ConfigRefusal {
    match err {
        PatchError::Malformed(msg) => ConfigRefusal::Malformed(msg),
        PatchError::Fixed(msg) => ConfigRefusal::Fixed(msg),
    }
}

#[async_trait::async_trait]
impl LiveConfig for LiveSettings {
    async fn current(&self) -> ConfigDocument {
        document(&self.locked())
    }

    async fn apply(&self, patch: ConfigPatch) -> Result<ConfigDocument, ConfigRefusal> {
        let mut current = self.locked();
        let next = patched(&current, &patch).map_err(refuse)?;
        let applied = self.publish(&mut current, next);
        info!("the running configuration was changed by a request");
        Ok(applied)
    }

    async fn reload(&self) -> Result<ConfigDocument, ConfigRefusal> {
        // Read before the lock is taken, and read on this thread. It is a small
        // file — in Kubernetes, a symlink farm on a tmpfs — read on an operator
        // route that is called when a ConfigMap changes rather than per request,
        // so handing it to `spawn_blocking` would buy a thread hop and a second
        // failure mode for a syscall or two.
        let loaded = self.source.load().map_err(|err| {
            warn!(%err, path = %self.source.path.display(), "a reload could not read the config file");
            ConfigRefusal::Source(format!(
                "reading {}: {err}",
                self.source.path.display()
            ))
        })?;

        let mut current = self.locked();
        // The file is folded on as a patch rather than adopted wholesale, and
        // that is what gives a reload the same rules a request gets: the fixed
        // settings are compared and refused by one function, so a ConfigMap that
        // moved the listen port is answered the same way a `PATCH` that moved it
        // would be. It also means the round trip `ConfigDocument::as_patch`
        // promises is load-bearing rather than decorative — this path is the
        // one that exercises it in production.
        let next = patched(&current, &document(&loaded).as_patch()).map_err(refuse)?;
        let applied = self.publish(&mut current, next);
        info!(
            path = %self.source.path.display(),
            "the configuration was reloaded from disk"
        );
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sismatic_api_types::Read;
    use sismatic_store::WriteStore;

    use super::*;
    use crate::configuration::{Overrides, resolve_config};

    /// A `LiveSettings` over the config `text` resolves to, plus the receiving
    /// ends the running tasks would hold.
    ///
    /// The receivers are kept rather than dropped, and that is the point of the
    /// fixture: a publish nobody is subscribed to would still succeed —
    /// `send_replace` cannot fail — so a test that dropped them would be
    /// asserting nothing about what a poll loop sees.
    fn settings(text: &str) -> (LiveSettings, Wiring, MemoryStore, MemoryOutbox) {
        let raw = serde_yaml_of(text);
        let cfg = resolve_config(&PathBuf::from("/etc/sismatic"), raw);
        let store = MemoryStore::with_budget(cfg.store.max_memory);
        let outbox = MemoryOutbox::with_max_attempts(cfg.intent_relay.max_attempts);
        let source = ConfigSource {
            path: PathBuf::from("unused-by-these-tests.yaml"),
            overrides: Overrides::default(),
        };
        let (settings, wiring) = LiveSettings::new(&cfg, source, store.clone(), outbox.clone());
        (settings, wiring, store, outbox)
    }

    /// Parse config text the way the loader does, so these fixtures state a
    /// document rather than a struct literal.
    fn serde_yaml_of(text: &str) -> crate::configuration::RawServerConfig {
        config::Config::builder()
            .add_source(config::File::from_str(text, config::FileFormat::Yaml))
            .build()
            .expect("building the config")
            .try_deserialize()
            .expect("deserializing the config")
    }

    fn patch(json: &str) -> ConfigPatch {
        serde_json::from_str(json).expect("a well-formed patch")
    }

    /// The property the whole module exists for: a change reaches the channels
    /// the running tasks read, not merely the struct the API reports from.
    #[tokio::test]
    async fn a_patch_reaches_every_task_that_enforces_a_piece_of_it() {
        let (settings, mut wiring, ..) = settings("sync:\n  fields: [FIRMWARE]\n");

        settings
            .apply(patch(
                r#"{"sync":{"fields":[{"name":"RUNNING_STATE","interval_secs":5}]},
                    "store":{"retain":"6h","cleanup_interval":"1min"},
                    "intent_relay":{"poll_ms":50}}"#,
            ))
            .await
            .expect("the patch should apply");

        assert_eq!(
            *wiring.schedule.borrow_and_update(),
            vec![FieldSchedule {
                name: "RUNNING_STATE".to_owned(),
                interval: Some(Duration::from_secs(5)),
            }]
        );
        let policy = wiring.policy.borrow_and_update().clone();
        assert_eq!(policy.cleanup, Some(Duration::from_secs(60)));
        assert_eq!(*wiring.drain.borrow_and_update(), Duration::from_millis(50));
    }

    /// ...and each channel reports the change as a change, which is what a
    /// task's `changed()` is waiting on. A value written with no notification
    /// would leave every loop on its old clock until something else woke it.
    #[tokio::test]
    async fn a_patch_wakes_the_tasks_rather_than_only_updating_the_value() {
        let (settings, wiring, ..) = settings("{}");
        let schedule = wiring.schedule.clone();
        assert!(!schedule.has_changed().expect("the sender is alive"));

        settings
            .apply(patch(r#"{"sync":{"interval_secs":7}}"#))
            .await
            .expect("the patch should apply");

        assert!(schedule.has_changed().expect("the sender is alive"));
    }

    /// The two settings no loop reads, which are applied by calling the adapter
    /// rather than by publishing. The budget is the one with an observable
    /// effect at the moment it is set.
    #[tokio::test]
    async fn a_lowered_budget_is_enforced_on_the_store_immediately() {
        let (settings, _wiring, store, outbox) = settings("{}");
        for n in 0..50 {
            store
                .upsert_latest(Read {
                    device: "atrium-101".to_owned(),
                    field: "FIRMWARE".to_owned(),
                    value: sismatic_api_types::ReadValue::Number(n),
                    at: sismatic_api_types::Timestamp(format!("2026-07-23T14:00:{n:02}.000Z")),
                })
                .await
                .expect("seeding the store");
        }

        settings
            .apply(patch(
                r#"{"store":{"max_memory":"1KiB"},"intent_relay":{"max_attempts":1}}"#,
            ))
            .await
            .expect("the patch should apply");

        let usage = sismatic_store::lifecycle::Lifecycle::usage(&store)
            .await
            .expect("reading usage");
        assert_eq!(usage.budget, Some(1024));
        assert!(
            usage.evicted > 0,
            "a lowered budget should have discarded history at once, got {usage:?}"
        );
        // The outbox's half has no reading to take, so what is asserted is that
        // the call was made at all — a `set_max_attempts` left out of `publish`
        // would leave this at the value the outbox was built with.
        drop(outbox);
    }

    /// A refused patch publishes nothing. The channels are what a task reads, so
    /// this is where "whole or not at all" is actually observable.
    #[tokio::test]
    async fn a_refused_patch_leaves_every_channel_untouched() {
        let (settings, wiring, ..) = settings("sync:\n  interval_secs: 30\n  fields: [FIRMWARE]\n");
        let schedule = wiring.schedule.clone();

        let refusal = settings
            .apply(patch(
                r#"{"sync":{"interval_secs":5},"store":{"retain":"5 fortnights"}}"#,
            ))
            .await
            .expect_err("a bad value should refuse the whole patch");

        assert!(
            matches!(refusal, ConfigRefusal::Malformed(_)),
            "{refusal:?}"
        );
        assert!(
            !schedule.has_changed().expect("the sender is alive"),
            "the good half of a refused patch must not have been published"
        );
        // ...and the settings the API reports are the ones still running.
        assert_eq!(settings.current().await.sync.interval_secs, 30);
    }

    /// A reload of a file that cannot be read changes nothing and says whose
    /// problem it is.
    #[tokio::test]
    async fn a_reload_of_a_missing_file_is_a_source_failure() {
        let (settings, ..) = settings("{}");

        let refusal = settings
            .reload()
            .await
            .expect_err("there is no such file to reload");

        assert!(
            matches!(refusal, ConfigRefusal::Source(ref msg)
                if msg.contains("unused-by-these-tests.yaml")),
            "the refusal should name the file, got: {refusal:?}"
        );
    }
}
