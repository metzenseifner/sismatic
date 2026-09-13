//! The device set as something that changes while the process runs, and the
//! task that carries a change to everything holding a device handle.
//!
//! [`crate::dynamic`] is the same idea for *settings*; this is its counterpart
//! for *inventory*, and the two are separate for the reason they are separate
//! in the config file. A setting is a number a running task reads again. A
//! device appearing or leaving changes which tasks should exist at all, and
//! three components own tasks per device: the sync driver (one per
//! `(device, field)`), the intent relay (one per device), and the SIS keepalive
//! (one per *eager* device).
//!
//! # Why a task and not three calls at the call site
//!
//! Because the call site is an HTTP handler, and the handles are not there. A
//! `RelayHandle` and a `SisKeepalive` are owned by [`crate::run`], which is
//! parked on a shutdown future for the life of the process; there is no way for
//! a request to reach them. The [`watch`] channel is how it does, and
//! [`FleetHandle`] is the one place those handles can live in the meantime.
//!
//! The sync driver is deliberately *not* driven from here. It already has a
//! supervisor of its own — it must, because its loops change with the schedule
//! as well as with the fleet — so it takes a receiver on the same channel and
//! reconciles itself. One publisher, two subscribers, and no component learns
//! about the fleet through another.
//!
//! # What the channel carries
//!
//! A generation counter, which nothing reads. Every subscriber holds the
//! `Arc<Registry>` already, so the message is only *look again* and the registry
//! is what gets looked at. That is also what makes a `watch`'s lossiness free:
//! three device changes arriving faster than a subscriber wakes collapse into
//! one reconcile against the same final fleet, which is the right answer rather
//! than an approximation of one.
//!
//! # Ordering, and the one place it matters
//!
//! [`LiveFleet::apply`] mutates the registry and *then* announces. The reverse
//! would let a subscriber wake and reconcile against the fleet as it was, and
//! since the announcement is edge-triggered there would be no second wake-up to
//! correct it.

use std::sync::Arc;

use sismatic_core::devices::config::Resolved;
use sismatic_core::devices::registry::{Registry, RegistryChange};
use sismatic_core::devices::sis_keepalive::SisKeepalive;
use sismatic_intent_relay::RelayHandle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument};

/// The registry, plus the one way to change it.
///
/// Held by the composition root and handed to whatever surface is allowed to
/// edit the fleet. Cloneable handles are deliberately absent: there is one
/// `Arc<Registry>` and one sender, and a caller that wants to change the fleet
/// goes through [`apply`](Self::apply) so that no path can mutate the registry
/// without announcing that it did.
pub struct LiveFleet {
    registry: Arc<Registry>,
    /// Bumped after every applied change. The value is a counter rather than the
    /// fleet itself — see the module docs.
    generation: watch::Sender<u64>,
}

impl LiveFleet {
    #[must_use]
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            generation: watch::channel(0).0,
        }
    }

    /// A receiver for a task that has to react to fleet changes.
    ///
    /// Handed out at startup rather than on demand, and that is not merely
    /// convention: a `watch` sender with no receivers still stores its value, so
    /// a subscriber created after a change would see the current generation and
    /// no `changed()` for the change that produced it. Every subscriber is made
    /// before the first `apply`.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    /// Make the fleet match `resolved`, and tell everything that holds a device
    /// handle.
    ///
    /// Returns what moved, so a caller can report it and — for removals — know
    /// which devices' queued writes it now has to deal with. This function
    /// deliberately does *not* cancel those writes or purge the store: a
    /// registry change is the fleet's business, and what becomes of a departed
    /// device's data is a policy question with a config key behind it.
    pub fn apply(&self, resolved: Resolved) -> RegistryChange {
        let change = self.registry.apply(resolved);
        if change.is_nothing() {
            // Announced anyway. A subscriber's own diff is the authority on
            // whether it has work — the sync driver's schedule may have moved
            // even when the fleet did not — and suppressing the wake-up here
            // would make this function's idea of "nothing" silently override
            // theirs.
            debug!("a device set was applied that changes no device");
        }
        // `send_modify` and not `send_replace(self.generation.borrow() + 1)`,
        // which deadlocks: `borrow` holds the watch's read lock, the temporary
        // lives to the end of the statement, and `send_replace` wants the write
        // lock before it is dropped. `send_modify` takes the write lock once and
        // hands the value in.
        //
        // Not `send` either: a deployment can legitimately have no subscribers —
        // the read side alone, with no sync driver and no relay — and `send`
        // treats that as an error.
        self.generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        change
    }

    /// The registry this fleet publishes changes for.
    #[must_use]
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }
}

/// Owns the per-device tasks that are not the sync driver's, and keeps them
/// matching the fleet.
pub struct FleetHandle {
    task: JoinHandle<()>,
    cancel: CancellationToken,
}

impl FleetHandle {
    /// Stop reconciling, then drain the relay and stop the keepalive.
    ///
    /// The two are shut down in that order inside the task, which is the order
    /// [`crate::run`] used before this type existed and for the same reason:
    /// keeping connections warm is pointless on the way out, and a keepalive
    /// probe starting now would only add an SSH exchange for the drain to wait
    /// behind.
    #[instrument(name = "fleet_shutdown", skip(self))]
    pub async fn shutdown(self) {
        self.cancel.cancel();
        // An `Err` here is a panicked reconciler, which has already taken its
        // handles with it and left nothing to drain.
        let _ = self.task.await;
        info!("fleet reconciler stopped");
    }
}

/// Start the reconciler, handing it the per-device task sets it will own.
///
/// Must be called from within a Tokio runtime.
pub fn spawn(
    registry: Arc<Registry>,
    relay: RelayHandle,
    keepalive: SisKeepalive,
    generation: watch::Receiver<u64>,
) -> FleetHandle {
    let cancel = CancellationToken::new();
    let task = tokio::spawn(reconcile(
        registry,
        relay,
        keepalive,
        generation,
        cancel.clone(),
    ));
    FleetHandle { task, cancel }
}

/// Hold the relay and the keepalive to the registry's device set until
/// cancelled, then shut them both down.
async fn reconcile(
    registry: Arc<Registry>,
    mut relay: RelayHandle,
    mut keepalive: SisKeepalive,
    mut generation: watch::Receiver<u64>,
    cancel: CancellationToken,
) {
    // So a generation published between `spawn` and the first `changed()` is not
    // applied twice — the same reason the sync supervisor does this.
    generation.borrow_and_update();

    // Whether anyone can still publish. A closed channel is a deployment whose
    // fleet was decided once, not a failure, and the answer is to stop asking
    // rather than to spin on a `changed()` that returns immediately forever.
    let mut watching = true;

    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            changed = generation.changed(), if watching => match changed {
                Ok(()) => {
                    generation.borrow_and_update();
                    let relayed = relay.apply();
                    let warmed = keepalive.apply(registry.devices());
                    info!(
                        relay_started = relayed.started,
                        relay_stopped = relayed.stopped,
                        relay_rebound = relayed.rebound,
                        keepalive_started = warmed.started,
                        keepalive_stopped = warmed.stopped,
                        keepalive_rebound = warmed.rebound,
                        devices = registry.len(),
                        "the device fleet changed"
                    );
                }
                Err(_) => watching = false,
            },
        }
    }

    // Dropped before the drain, not after: see `FleetHandle::shutdown`.
    drop(keepalive);
    relay.shutdown().await;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use sismatic_core::devices::config::{DeviceConfig, Uuid};
    use sismatic_core::devices::connector::fake::CountingConnector;
    use sismatic_core::devices::transport::fake::FakeTransport;

    use super::*;

    fn device_config(id: &str) -> DeviceConfig {
        DeviceConfig {
            id: id.into(),
            host: "10.0.0.1".into(),
            port: 22023,
            username: "admin".into(),
            password: "extron".into(),
            connect_timeout: Duration::from_millis(200),
            exchange_timeout: Duration::from_millis(200),
            eager: false,
            sis_keepalive: None,
            eager_retry: None,
            cold_backoff: None,
            uuid: Uuid::nil(),
            disabled_fields: BTreeSet::new(),
            auto_disable_after: 0,
            self_heal: None,
        }
        .derive_uuid()
    }

    fn registry_of(ids: &[&str]) -> Arc<Registry> {
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads(["2.11\r\n"; 8])
        }));
        Arc::new(Registry::build(
            ids.iter().map(|id| device_config(id)).collect(),
            vec![],
            connector,
        ))
    }

    /// Subscribers created at startup see the *change*, not merely the value it
    /// left behind. A receiver made after the fact would read the current
    /// generation with nothing pending.
    #[tokio::test]
    async fn applying_a_change_wakes_every_subscriber() {
        let fleet = LiveFleet::new(registry_of(&["a"]));
        let first = fleet.subscribe();
        let second = fleet.subscribe();

        fleet.apply(Resolved {
            devices: vec![device_config("a"), device_config("b")],
            groups: vec![],
        });

        assert!(first.has_changed().expect("the sender is alive"));
        assert!(second.has_changed().expect("the sender is alive"));
        assert_eq!(fleet.registry().len(), 2);
    }

    /// The registry is mutated before the announcement, so a subscriber that
    /// wakes immediately reads the fleet the announcement is about. Announcing
    /// first would be an edge with no second chance behind it.
    #[tokio::test]
    async fn the_registry_is_current_by_the_time_subscribers_wake() {
        let fleet = LiveFleet::new(registry_of(&["a"]));
        let mut watcher = fleet.subscribe();
        let registry = Arc::clone(fleet.registry());

        let seen = tokio::spawn(async move {
            watcher.changed().await.expect("a change");
            registry.ids().len()
        });
        tokio::task::yield_now().await;

        fleet.apply(Resolved {
            devices: vec![device_config("a"), device_config("b")],
            groups: vec![],
        });

        assert_eq!(seen.await.expect("the watcher task"), 2);
    }

    /// A no-op apply still announces. Whether a subscriber has work is its own
    /// diff's business — the sync driver's schedule may have moved even when the
    /// fleet did not — so this must not decide on its behalf.
    #[tokio::test]
    async fn an_unchanged_fleet_still_announces() {
        let fleet = LiveFleet::new(registry_of(&["a"]));
        let watcher = fleet.subscribe();

        let change = fleet.apply(Resolved {
            devices: vec![device_config("a")],
            groups: vec![],
        });

        assert!(change.is_nothing());
        assert!(watcher.has_changed().expect("the sender is alive"));
    }
}
