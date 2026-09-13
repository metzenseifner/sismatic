//! Eagerly warming connections and keeping them from idling out.
//!
//! An SMP drops its SSH session after a few minutes with no SIS traffic (default
//! 5). [`SisKeepalive`] spawns one background task per *eager* device (see
//! [`DeviceConfig::eager`]) that opens the connection immediately and then
//! re-issues the cheapest SIS round-trip — the `Q` (firmware) query — on the
//! device's [`sis_keepalive`] interval, resetting that idle timer so the warm
//! connection survives between real commands.
//!
//! `eager` is a *standing* intent to hold a warm connection, not a one-shot
//! connect at startup, so the task tracks two states with two cadences:
//!
//! * **Warm** — the last probe reached the device. The next probe waits the
//!   [`sis_keepalive`] interval, keeping the idle timer from expiring.
//! * **Cold** — the last probe could not reach the device (it was down at
//!   startup, or the connection has since dropped). The next probe waits the
//!   shorter [`eager_retry`] interval, re-attempting the SSH handshake until the
//!   device answers and the task flips back to warm.
//!
//! Every probe goes through [`Device::probe`] rather than [`Device::run`], so it
//! dials even when the device's cold gate is shut. That is what keeps the two
//! mechanisms complementary instead of competing: for an eager device this task
//! is the *only* thing that dials while the device is down — on the
//! [`eager_retry`] cadence the operator set — and the gate spares every other
//! caller from repeating the attempt in between.
//!
//! The tasks are best-effort: a failed warm-up, SIS keepalive, or retry is
//! logged, never fatal, and the device's own self-healing reconnect still covers
//! the next real command in between. Non-eager devices get no task and stay fully
//! lazy, exactly as before. Either interval being unset (a bare
//! `sis_keepalive_secs = 0` / `eager_retry_secs = 0`) ends the task in that state:
//! `sis_keepalive = None` warms once and then stops probing, `eager_retry = None`
//! gives up after the first failed connect.
//!
//! Dropping the [`SisKeepalive`] aborts every task, so the keep-warm work stops in
//! step with the registry whose devices it was driving.
//!
//! Each task runs inside its own `sis_keepalive` span carrying the device id and a
//! per-task `sis_keepalive_id` (a v4 UUID), so a log backend can follow one device's
//! warm/cold history end to end; see `keep_warm` for the emitted events.
//!
//! [`DeviceConfig::eager`]: super::config::DeviceConfig::eager
//! [`sis_keepalive`]: super::config::DeviceConfig::sis_keepalive
//! [`eager_retry`]: super::config::DeviceConfig::eager_retry

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tracing::{debug, instrument, warn};
use uuid::Uuid;

use crate::protocol::instructions::query::Query;

use super::device::Device;

/// One device's keep-warm task, and the configuration it was started for.
struct Warm {
    /// The [`uuid`] of the config this task captured.
    ///
    /// A device is immutable, so an edited one is *replaced* and the task is
    /// holding an `Arc<Device>` the registry no longer hands out — pointed at a
    /// connection nothing else can use, and warming it forever. The id alone
    /// cannot see that; this can.
    ///
    /// [`uuid`]: super::config::DeviceConfig::uuid
    device: Uuid,
    task: JoinHandle<()>,
}

/// What one pass of [`SisKeepalive::apply`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeepaliveChange {
    pub started: usize,
    pub stopped: usize,
    /// Replaced devices, whose task was stopped and started against the new
    /// handle.
    pub rebound: usize,
    pub unchanged: usize,
}

/// A set of background tasks keeping eager devices' connections warm. Dropping
/// it aborts them all.
pub struct SisKeepalive {
    /// Kept so [`apply`](Self::apply) can spawn without being handed a runtime
    /// again — a fleet change arrives wherever the reconciler happens to run,
    /// and requiring a `Handle` there would push this type's implementation
    /// detail onto its caller.
    handle: Handle,
    /// One entry per *eager* device with a task running, keyed by device id.
    warm: BTreeMap<String, Warm>,
}

impl SisKeepalive {
    /// Spawn a keep-warm task on `handle` for every device whose config marks it
    /// [`eager`]; non-eager devices are skipped and stay lazy. `handle` must
    /// belong to a running runtime — in practice the same one the devices'
    /// commands execute on.
    ///
    /// [`eager`]: super::config::DeviceConfig::eager
    pub fn spawn(handle: &Handle, devices: impl IntoIterator<Item = Arc<Device>>) -> Self {
        let mut keepalive = Self {
            handle: handle.clone(),
            warm: BTreeMap::new(),
        };
        keepalive.apply(devices);
        keepalive
    }

    /// Make the running tasks match `devices`, and report what moved.
    ///
    /// Idempotent: applying the same fleet twice is a no-op, because a device
    /// whose [`uuid`] is unchanged keeps the task it has. That is what stops a
    /// reload from dropping and re-warming every eager connection in the fleet
    /// to apply a change to one of them.
    ///
    /// A device that stops being `eager` is indistinguishable here from one that
    /// left the fleet — both are "no task should be running for this id" — and
    /// both are correct: the connection is left for the next real command to
    /// re-open lazily, which is exactly what a non-eager device does.
    ///
    /// [`uuid`]: super::config::DeviceConfig::uuid
    pub fn apply(&mut self, devices: impl IntoIterator<Item = Arc<Device>>) -> KeepaliveChange {
        let wanted: BTreeMap<String, Arc<Device>> = devices
            .into_iter()
            .filter(|device| device.config().eager)
            .map(|device| (device.id().to_owned(), device))
            .collect();

        let mut change = KeepaliveChange::default();

        // Stop first, so a replaced device's old task has already been told to
        // go before its successor starts dialing the same address.
        let stale: Vec<String> = self
            .warm
            .iter()
            .filter(|(id, warm)| {
                wanted
                    .get(*id)
                    .is_none_or(|device| device.config().uuid != warm.device)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(warm) = self.warm.remove(&id) {
                // Abort rather than a cooperative cancel, which is what `Drop`
                // has always done here. A keepalive probe holds the device's
                // connection lock, but the device being abandoned is one nothing
                // else references — so the worst an aborted probe can leave
                // mid-exchange is a connection that is about to be dropped.
                warm.task.abort();
            }
            if wanted.contains_key(&id) {
                change.rebound += 1;
            } else {
                change.stopped += 1;
            }
        }

        // Anything the pass above removed is gone from `warm`, so a device still
        // present here is one whose task is correct and stays untouched.
        let mut spawned = 0usize;
        for (id, device) in wanted {
            if self.warm.contains_key(&id) {
                change.unchanged += 1;
                continue;
            }
            let uuid = device.config().uuid;
            let task = self.handle.spawn(keep_warm(device));
            self.warm.insert(id, Warm { device: uuid, task });
            spawned += 1;
        }
        // A replaced device was stopped above and spawned just now, so it is in
        // `spawned` too. It is charged once, to the more specific of the two.
        change.started = spawned - change.rebound;
        change
    }

    /// How many devices are being kept warm.
    #[must_use]
    pub fn len(&self) -> usize {
        self.warm.len()
    }

    /// Whether nothing is being kept warm.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.warm.is_empty()
    }
}

impl Drop for SisKeepalive {
    fn drop(&mut self) {
        for warm in self.warm.values() {
            warm.task.abort();
        }
    }
}

/// Probe `device` now, then keep probing on a cadence that depends on whether it is
/// warm, until the task is aborted. A warm probe waits [`sis_keepalive`] before the
/// next keepalive tick; a cold one waits [`eager_retry`] before the next reconnect
/// attempt. Every exchange is best-effort; a failure is logged and the loop keeps
/// going, since the next probe (or the next real command) will reconnect on its own.
/// The loop only ends early if the interval it would wait is unset — `sis_keepalive`
/// warms once and stops, `eager_retry` gives up after the first failed connect.
///
/// The whole task runs inside one span named `sis_keepalive`, tagged with the
/// device's id and a per-task `sis_keepalive_id` (a v4 [`Uuid`]). Every event below
/// therefore inherits both fields, so a log backend can group one device's warm
/// and cold moments by `sis_keepalive_id` without any per-event bookkeeping. Each
/// probe records a boolean `warm` (the SMP answered / it did not) and a `trigger`
/// naming why we probed — `eager` for the one-shot startup connect, `periodic` for a
/// keepalive tick on a warm device, `retry` for a reconnect attempt on a cold one —
/// giving the two telemetry axes: *is it warm* and *why did we probe*.
///
/// [`sis_keepalive`]: super::config::DeviceConfig::sis_keepalive
/// [`eager_retry`]: super::config::DeviceConfig::eager_retry
#[instrument(
    name = "sis_keepalive",
    skip_all,
    fields(device = %device.id(), sis_keepalive_id = %Uuid::new_v4()),
)]
async fn keep_warm(device: Arc<Device>) {
    let query = Query::Firmware.instruction();
    let sis_keepalive = device.config().sis_keepalive;
    let eager_retry = device.config().eager_retry;

    // The first probe is the startup warm-up; every later probe is labelled by the
    // wait that scheduled it — a keepalive tick when warm, a reconnect when cold.
    let mut trigger = "eager";
    loop {
        // Choose the next wait from *this* probe's outcome: a warm device waits
        // `sis_keepalive` before its next keepalive tick, a cold one waits the
        // shorter `eager_retry` before trying to reconnect. Either wait being unset
        // ends the task, leaving the device to self-heal on its next real command.
        // `probe`, not `run`: this task *is* the retry mechanism for a cold
        // device, so it dials through the cold gate rather than being held off
        // by a window its own success is what closes. See `Device::probe`.
        let (wait, next_trigger) = match device.probe(&query).await {
            Ok(_) => {
                debug!(warm = true, trigger, "device warm");
                (sis_keepalive, "periodic")
            }
            Err(error) => {
                warn!(warm = false, trigger, %error, "device cold");
                (eager_retry, "retry")
            }
        };

        let Some(interval) = wait else { return };
        tokio::time::sleep(interval).await;
        trigger = next_trigger;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::devices::config::DeviceConfig;
    use crate::devices::connector::fake::CountingConnector;
    use crate::devices::connector::{ConnectError, Connector};
    use crate::devices::transport::Transport;
    use crate::devices::transport::fake::FakeTransport;

    const FIRMWARE_REPLY: &str = "2.11\r\n";

    /// A device config that is eager, with the given SIS keepalive and eager-retry
    /// intervals.
    fn eager_config(
        sis_keepalive: Option<Duration>,
        eager_retry: Option<Duration>,
    ) -> DeviceConfig {
        DeviceConfig {
            id: "warm".into(),
            host: "10.0.0.1".into(),
            port: 22023,
            username: "admin".into(),
            password: "extron".into(),
            connect_timeout: Duration::from_millis(500),
            exchange_timeout: Duration::from_millis(500),
            eager: true,
            sis_keepalive,
            eager_retry,
            // Gated hard, to prove these tasks dial *through* the gate: with
            // `probe` swapped back to `run`, the cold-side tests below stall.
            cold_backoff: Some(Duration::from_secs(3600)),
            uuid: Uuid::nil(),
            disabled_fields: BTreeSet::new(),
            auto_disable_after: 0,
            self_heal: None,
        }
        .derive_uuid()
    }

    /// Poll `cond` until it holds, or panic after ~2s. Lets a spawned SIS keepalive
    /// task make progress without racing on a fixed sleep.
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
    async fn eager_device_opens_its_connection_without_a_command() {
        // One connection that can answer the eager warm-up; no SIS keepalive loop.
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([FIRMWARE_REPLY])
        }));
        let opens = connector.opens_handle();
        let device = Arc::new(Device::new(eager_config(None, None), connector));

        let _sis_keepalive = SisKeepalive::spawn(&Handle::current(), [Arc::clone(&device)]);

        wait_for(|| opens.load(Ordering::SeqCst) == 1).await;
    }

    // ---- reconciling against a fleet that changes -------------------------

    /// An eager device at `id`, over a connector that answers every warm-up.
    fn eager_at(id: &str, connector: Arc<CountingConnector>) -> Arc<Device> {
        let config = DeviceConfig {
            id: id.into(),
            ..eager_config(None, None)
        }
        .derive_uuid();
        Arc::new(Device::new(config, connector))
    }

    fn answering_connector() -> Arc<CountingConnector> {
        Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([FIRMWARE_REPLY; 8])
        }))
    }

    /// The property a reload rests on: applying the same fleet twice touches
    /// nothing, so a change to one device does not drop and re-warm every eager
    /// connection in the fleet.
    #[tokio::test]
    async fn re_applying_the_same_fleet_keeps_every_task() {
        let connector = answering_connector();
        let opens = connector.opens_handle();
        let device = eager_at("warm", connector);

        let mut keepalive = SisKeepalive::spawn(&Handle::current(), [Arc::clone(&device)]);
        wait_for(|| opens.load(Ordering::SeqCst) == 1).await;

        let change = keepalive.apply([Arc::clone(&device)]);

        assert_eq!(change.unchanged, 1);
        assert_eq!(change.started, 0);
        assert_eq!(change.stopped, 0);
        assert_eq!(change.rebound, 0);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            opens.load(Ordering::SeqCst),
            1,
            "an unchanged device must not be re-dialed"
        );
    }

    #[tokio::test]
    async fn a_device_added_to_the_fleet_is_warmed() {
        let connector = answering_connector();
        let opens = connector.opens_handle();
        let first = eager_at("first", Arc::clone(&connector));

        let mut keepalive = SisKeepalive::spawn(&Handle::current(), [Arc::clone(&first)]);
        wait_for(|| opens.load(Ordering::SeqCst) == 1).await;

        let second = eager_at("second", connector);
        let change = keepalive.apply([first, second]);

        assert_eq!(change.started, 1);
        assert_eq!(change.unchanged, 1);
        assert_eq!(keepalive.len(), 2);
        wait_for(|| opens.load(Ordering::SeqCst) == 2).await;
    }

    #[tokio::test]
    async fn a_device_removed_from_the_fleet_stops_being_warmed() {
        let connector = answering_connector();
        let device = eager_at("goner", connector);

        let mut keepalive = SisKeepalive::spawn(&Handle::current(), [device]);
        assert_eq!(keepalive.len(), 1);

        let change = keepalive.apply([]);

        assert_eq!(change.stopped, 1);
        assert_eq!(change.started, 0);
        assert!(keepalive.is_empty());
    }

    /// A device that stops being `eager` is the same instruction as one that
    /// left: no task should run for it, and the connection is left for the next
    /// real command to open lazily.
    #[tokio::test]
    async fn a_device_that_stops_being_eager_loses_its_task() {
        let connector = answering_connector();
        let eager = eager_at("settled", Arc::clone(&connector));

        let mut keepalive = SisKeepalive::spawn(&Handle::current(), [eager]);
        assert_eq!(keepalive.len(), 1);

        let lazy = Arc::new(Device::new(
            DeviceConfig {
                id: "settled".into(),
                eager: false,
                ..eager_config(None, None)
            }
            .derive_uuid(),
            connector,
        ));
        let change = keepalive.apply([lazy]);

        assert_eq!(change.stopped, 1);
        assert!(keepalive.is_empty());
    }

    /// A replaced device — same id, edited config — must be re-warmed against
    /// the new handle. The old task is holding a `Device` nothing else
    /// references, so left alone it would warm a connection forever that no
    /// command can ever use.
    #[tokio::test]
    async fn a_replaced_device_is_rebound_to_the_new_handle() {
        let connector = answering_connector();
        let opens = connector.opens_handle();
        let before = eager_at("edited", Arc::clone(&connector));

        let mut keepalive = SisKeepalive::spawn(&Handle::current(), [before]);
        wait_for(|| opens.load(Ordering::SeqCst) == 1).await;

        let after = Arc::new(Device::new(
            DeviceConfig {
                id: "edited".into(),
                connect_timeout: Duration::from_millis(900),
                ..eager_config(None, None)
            }
            .derive_uuid(),
            connector,
        ));
        let change = keepalive.apply([after]);

        assert_eq!(change.rebound, 1);
        assert_eq!(change.started, 0, "a rebind is charged once, not twice");
        assert_eq!(change.unchanged, 0);
        assert_eq!(keepalive.len(), 1);
        wait_for(|| opens.load(Ordering::SeqCst) == 2).await;
    }

    #[tokio::test]
    async fn a_lazy_device_is_never_touched() {
        let connector = Arc::new(CountingConnector::new(FakeTransport::new));
        let opens = connector.opens_handle();
        let mut config = eager_config(None, None);
        config.eager = false;
        let device = Arc::new(Device::new(config, connector));

        let _sis_keepalive = SisKeepalive::spawn(&Handle::current(), [Arc::clone(&device)]);

        // Give any (erroneously spawned) task time to act, then confirm it did not.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            opens.load(Ordering::SeqCst),
            0,
            "lazy device must stay cold"
        );
    }

    #[tokio::test]
    async fn sis_keepalive_reissues_the_query_on_its_interval() {
        // Each connection answers a single `Q` then closes, so every SIS keepalive
        // tick forces a self-healing reconnect — a convenient way to count ticks
        // through the open counter.
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([FIRMWARE_REPLY])
        }));
        let opens = connector.opens_handle();
        let device = Arc::new(Device::new(
            eager_config(Some(Duration::from_millis(20)), None),
            connector,
        ));

        let _sis_keepalive = SisKeepalive::spawn(&Handle::current(), [Arc::clone(&device)]);

        // Warm-up is one open; each subsequent tick adds another.
        wait_for(|| opens.load(Ordering::SeqCst) >= 3).await;
    }

    #[tokio::test]
    async fn dropping_the_guard_stops_the_sis_keepalive() {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads([FIRMWARE_REPLY])
        }));
        let opens = connector.opens_handle();
        let device = Arc::new(Device::new(
            eager_config(Some(Duration::from_millis(20)), None),
            connector,
        ));

        let sis_keepalive = SisKeepalive::spawn(&Handle::current(), [Arc::clone(&device)]);
        wait_for(|| opens.load(Ordering::SeqCst) >= 2).await;

        drop(sis_keepalive);
        let settled = opens.load(Ordering::SeqCst);

        // After aborting, no further ticks should fire (tolerate one already in
        // flight at the moment of the drop).
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            opens.load(Ordering::SeqCst) <= settled + 1,
            "SIS keepalive kept running after the guard was dropped"
        );
    }

    /// A connector that refuses its first `failures` connect attempts, then yields a
    /// transport answering one firmware query. Every attempt bumps a shared counter,
    /// so a test can watch cold-side reconnects accumulate.
    struct FlakyConnector {
        attempts: Arc<AtomicUsize>,
        failures: usize,
    }

    impl FlakyConnector {
        fn new(failures: usize) -> Self {
            Self {
                attempts: Arc::new(AtomicUsize::new(0)),
                failures,
            }
        }

        fn attempts_handle(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.attempts)
        }
    }

    #[async_trait]
    impl Connector for FlakyConnector {
        async fn connect(
            &self,
            _config: &DeviceConfig,
        ) -> Result<Box<dyn Transport>, ConnectError> {
            let prior = self.attempts.fetch_add(1, Ordering::SeqCst);
            if prior < self.failures {
                Err(ConnectError::Failed("down".into()))
            } else {
                Ok(Box::new(FakeTransport::with_reads([FIRMWARE_REPLY])))
            }
        }
    }

    #[tokio::test]
    async fn a_cold_eager_device_retries_until_it_answers() {
        // The device refuses its first two connects, then accepts. With retry on a
        // short interval and the SIS keepalive disabled, the task must keep
        // reconnecting through the failures rather than give up after the first.
        let connector = Arc::new(FlakyConnector::new(2));
        let attempts = connector.attempts_handle();
        let device = Arc::new(Device::new(
            eager_config(None, Some(Duration::from_millis(20))),
            connector,
        ));

        let _sis_keepalive = SisKeepalive::spawn(&Handle::current(), [Arc::clone(&device)]);

        // Two refused attempts plus the successful third.
        wait_for(|| attempts.load(Ordering::SeqCst) >= 3).await;

        // Warm now, and with no keepalive the task stops: no further connects.
        let settled = attempts.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            settled,
            "a warmed device with no keepalive must stop retrying"
        );
    }

    #[tokio::test]
    async fn a_cold_eager_device_with_retry_disabled_gives_up() {
        // Retry off (eager_retry = None) and the device never answers: the task must
        // make exactly one connect attempt and then stop, as before this feature.
        let connector = Arc::new(FlakyConnector::new(usize::MAX));
        let attempts = connector.attempts_handle();
        let device = Arc::new(Device::new(eager_config(None, None), connector));

        let _sis_keepalive = SisKeepalive::spawn(&Handle::current(), [Arc::clone(&device)]);

        wait_for(|| attempts.load(Ordering::SeqCst) >= 1).await;
        let settled = attempts.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            settled,
            "with retry disabled a cold device must not reconnect"
        );
    }
}
