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

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use sismatic_api_types::{Reading, Timestamp};
use sismatic_core::devices::device::Device;
use sismatic_core::devices::registry::Registry;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_store::DynWriteStore;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        while self.tasks.join_next().await.is_some() {}
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

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                match device.run(&instruction).await {
                    Ok(value) => {
                        let reading = Reading {
                            device: device.id().to_string(),
                            field: field.clone(),
                            value: dto::to_dto(value),
                            at: Timestamp(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
                        };
                        if let Err(err) = write.upsert_latest(reading).await {
                            warn!(device = device.id(), field, %err, "failed to persist reading");
                        }
                    }
                    // A device being down is normal — log and tick again.
                    Err(err) => {
                        warn!(device = device.id(), field, %err, "poll failed; will retry next tick");
                    }
                }
            }
        }
    }

    info!(device = device.id(), field, "poll loop stopped");
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sismatic_api_types::Reading;
    use sismatic_core::devices::config::DeviceConfig;
    use sismatic_core::devices::connector::fake::CountingConnector;
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
}
