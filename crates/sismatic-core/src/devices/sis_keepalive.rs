//! Eagerly warming connections and keeping them from idling out.
//!
//! An SMP drops its SSH session after a few minutes with no SIS traffic (default
//! 5). [`SisKeepalive`] spawns one background task per *eager* device (see
//! [`DeviceConfig::eager`]) that opens the connection immediately and then
//! re-issues the cheapest SIS round-trip — the `Q` (firmware) query — on the
//! device's [`sis_keepalive`] interval, resetting that idle timer so the warm
//! connection survives between real commands.
//!
//! The tasks are best-effort: a failed warm-up or SIS keepalive is logged, never
//! fatal, and the device's own self-healing reconnect covers the next real
//! command. Non-eager devices get no task and stay fully lazy, exactly as
//! before.
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

/// Warm `device`'s connection now, then re-issue `Q` on its SIS keepalive interval
/// until the task is aborted. Every exchange is best-effort; a failure is logged
/// and the loop keeps ticking, since the next tick (or the next real command)
/// will reconnect on its own.
///
/// The whole task runs inside one span named `sis_keepalive`, tagged with the
/// device's id and a per-task `sis_keepalive_id` (a v4 [`Uuid`]). Every event below
/// therefore inherits both fields, so a log backend can group one device's warm
/// and cold moments by `sis_keepalive_id` without any per-event bookkeeping. Each
/// probe records a boolean `warm` (the SMP answered / it did not) and a
/// `trigger` distinguishing the one-shot startup connect from the recurring
/// timer, giving the two telemetry axes: *is it warm* and *why did we probe*.
#[instrument(
    name = "sis_keepalive",
    skip_all,
    fields(device = %device.id(), sis_keepalive_id = %Uuid::new_v4()),
)]
async fn keep_warm(device: Arc<Device>) {
    let query = Query::Firmware.instruction();

    // Eager warm-up: the probe that turns a cold device warm at startup.
    match device.run(&query).await {
        Ok(_) => debug!(warm = true, trigger = "eager", "device warmed"),
        Err(error) => warn!(warm = false, trigger = "eager", %error, "device cold"),
    }

    let Some(interval) = device.config().sis_keepalive else {
        return; // eager warm-up only; no SIS keepalive loop was requested
    };

    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // the first tick fires immediately; the warm-up covered it
    loop {
        ticker.tick().await;
        match device.run(&query).await {
            Ok(_) => debug!(warm = true, trigger = "periodic", "device warm"),
            Err(error) => warn!(warm = false, trigger = "periodic", %error, "device cold"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crate::devices::config::DeviceConfig;
    use crate::devices::connector::fake::CountingConnector;
    use crate::devices::transport::fake::FakeTransport;

    const FIRMWARE_REPLY: &str = "2.11\r\n";

    /// A device config that is eager, with the given SIS keepalive interval.
    fn eager_config(sis_keepalive: Option<Duration>) -> DeviceConfig {
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
        let device = Arc::new(Device::new(eager_config(None), connector));

        let _sis_keepalive = SisKeepalive::spawn(&Handle::current(), [Arc::clone(&device)]);

        wait_for(|| opens.load(Ordering::SeqCst) == 1).await;
    }

    #[tokio::test]
    async fn a_lazy_device_is_never_touched() {
        let connector = Arc::new(CountingConnector::new(FakeTransport::new));
        let opens = connector.opens_handle();
        let mut config = eager_config(None);
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
            eager_config(Some(Duration::from_millis(20))),
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
            eager_config(Some(Duration::from_millis(20))),
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
}
