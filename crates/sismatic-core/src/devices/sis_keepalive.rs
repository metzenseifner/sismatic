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

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tracing::{debug, instrument, warn};
use uuid::Uuid;

use crate::protocol::instructions::query::Query;

use super::device::Device;

/// A set of background tasks keeping eager devices' connections warm. Dropping
/// it aborts them all.
pub struct SisKeepalive {
    tasks: Vec<JoinHandle<()>>,
}

impl SisKeepalive {
    /// Spawn a keep-warm task on `handle` for every device whose config marks it
    /// [`eager`]; non-eager devices are skipped and stay lazy. `handle` must
    /// belong to a running runtime — in practice the same one the devices'
    /// commands execute on.
    ///
    /// [`eager`]: super::config::DeviceConfig::eager
    pub fn spawn(handle: &Handle, devices: impl IntoIterator<Item = Arc<Device>>) -> Self {
        let tasks = devices
            .into_iter()
            .filter(|device| device.config().eager)
            .map(|device| handle.spawn(keep_warm(device)))
            .collect();
        Self { tasks }
    }
}

impl Drop for SisKeepalive {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
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
        let (wait, next_trigger) = match device.run(&query).await {
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
            command_timeout: Duration::from_millis(500),
            eager: true,
            sis_keepalive,
            eager_retry,
        }
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
