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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sismatic_api_types::{DeviceSummary, DeviceWrite, ExportFormat, ExportQuery, Removed};
use sismatic_core::devices::config::{
    Barrier, ConfigError, Defaults, DeviceConfig, GroupConfig, Password, RawConfig, RawDevice,
    Resolved, load_raw, resolve_config,
};
use sismatic_core::devices::registry::{Registry, RegistryChange};
use sismatic_core::devices::sis_keepalive::SisKeepalive;
use sismatic_http_api::{InventoryRefusal, LiveInventory};
use sismatic_intent_relay::RelayHandle;
use sismatic_store_memory::{MemoryCatalog, MemoryOutbox, MemoryStore};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::{summarize, summarize_group};

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
    /// The devices document as written, amended in place by the mutation verbs.
    ///
    /// Kept raw rather than resolved because a device added at runtime has to
    /// inherit `[defaults]` exactly as one in the file does. Amending this and
    /// re-running [`resolve_config`] puts every change through the one function
    /// that enforces duplicate ids, group membership, credential presence and
    /// canonical field names — so there is no second validation path to forget
    /// a rule.
    ///
    /// A `std::sync::Mutex` and not an async one: nothing under it awaits, and
    /// what it guards is a document plus the registry it was applied to, which
    /// must not be read or written out of step.
    document: Mutex<RawConfig>,
    /// The secret-free projection the read routes serve, kept in step with the
    /// registry by every mutation.
    catalog: MemoryCatalog,
    /// Held for the two verbs no port carries — cancelling a departed device's
    /// queue, and dropping its reads. The same arrangement `LiveSettings` uses
    /// for `set_budget` and `set_max_attempts`, and for the same reason: this is
    /// the composition root reaching adapters it built.
    outbox: MemoryOutbox,
    store: MemoryStore,
    /// Whether a removed device's recorded reads go with it.
    cleanup_on_remove: bool,
    /// The devices file, for the one operation that reads it again.
    devices_path: PathBuf,
    /// Where runtime changes are persisted, or `None` to keep them in memory.
    ///
    /// Unset by default, and the default is what makes the devices file
    /// authoritative: with nothing persisted, a restart returns to exactly what
    /// the file says and there is no second source of truth to be surprised by.
    ///
    /// Set, it becomes one — which is the point and the hazard both. See
    /// [`persist`](Self::persist).
    state_path: Option<PathBuf>,
}

/// Everything [`LiveFleet::new`] is built from.
///
/// A struct rather than eight positional arguments, and not only to satisfy a
/// lint: five of the eight are adapters or paths the composition root is holding
/// anyway, and three of those have the same shape — two `PathBuf`-ish paths and
/// a `bool` — so a transposed pair would compile and be wrong at runtime. Named
/// fields make that a build error.
pub struct Wiring {
    pub registry: Arc<Registry>,
    /// The devices document as written, which the mutation verbs amend.
    pub document: RawConfig,
    /// The catalog the read routes serve, kept in step with the registry.
    pub catalog: MemoryCatalog,
    pub outbox: MemoryOutbox,
    pub store: MemoryStore,
    /// Whether a removed device's recorded reads go with it.
    pub cleanup_on_remove: bool,
    /// The devices file, for `reset`.
    pub devices_path: PathBuf,
    /// Where runtime changes are persisted, or `None` to keep them in memory.
    pub state_path: Option<PathBuf>,
}

impl LiveFleet {
    #[must_use]
    pub fn new(wiring: Wiring) -> Self {
        let Wiring {
            registry,
            document,
            catalog,
            outbox,
            store,
            cleanup_on_remove,
            devices_path,
            state_path,
        } = wiring;
        Self {
            registry,
            generation: watch::channel(0).0,
            document: Mutex::new(document),
            catalog,
            outbox,
            store,
            cleanup_on_remove,
            devices_path,
            state_path,
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

    /// Amend the document with `edit`, resolve it, and apply the result.
    ///
    /// The one path every mutation takes, so validation, catalog upkeep and the
    /// announcement cannot be done for one verb and forgotten for another. The
    /// document is only *kept* if resolution succeeded — a refused edit leaves
    /// the running fleet and the document it was derived from both untouched,
    /// which is the same whole-or-nothing contract `patched` gives settings.
    fn amend(
        &self,
        edit: impl FnOnce(&mut Vec<RawDevice>) -> Result<(), InventoryRefusal>,
    ) -> Result<Resolved, InventoryRefusal> {
        let mut document = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Against a copy, so a resolution failure cannot leave the document
        // describing a fleet that was never applied.
        let mut devices = document.devices.clone();
        edit(&mut devices)?;

        let amended = RawConfig {
            defaults: document.defaults.clone(),
            devices,
            groups: document.groups.clone(),
        };
        let resolved = resolve_config(amended).map_err(refusal_of)?;

        // Rebuild from the amended list, which resolution has now vouched for.
        document.devices = resolved
            .devices
            .iter()
            .map(|device| raw_of(device, &document.defaults))
            .collect();

        self.catalog.replace(
            resolved.devices.iter().map(summarize).collect(),
            resolved.groups.iter().map(summarize_group).collect(),
        );
        self.apply_locked(resolved.clone());
        self.persist(&resolved);
        Ok(resolved)
    }

    /// Write the running fleet to `state_path`, if a deployment asked for one.
    ///
    /// **With credentials**, necessarily: the file's whole purpose is to be
    /// loadable at the next startup, and a device this process cannot
    /// authenticate to is a device it cannot poll. The path is the operator's
    /// choice and so is its mode; a deployment that turns this on is stating
    /// that the location is one secrets may live in.
    ///
    /// A failure is logged and swallowed rather than failing the request. The
    /// change has *already been applied* to the running fleet by the time this
    /// runs, so returning an error here would report a failure for something
    /// that succeeded — and undoing it would mean a second registry swap to
    /// recover from a full disk. What a failed persist costs is that the change
    /// does not survive a restart, which is exactly what the log line says.
    fn persist(&self, resolved: &Resolved) {
        let Some(path) = &self.state_path else {
            return;
        };
        let query = ExportQuery {
            format: format_of(path),
            promote_auto_disabled_fields_to_disabled_fields: false,
            include_secrets: true,
        };
        match render(
            &resolved.devices,
            &resolved.groups,
            &BTreeMap::new(),
            &query,
        )
        .map_err(|e| e.to_string())
        .and_then(|body| std::fs::write(path, body).map_err(|e| e.to_string()))
        {
            Ok(()) => debug!(path = %path.display(), "the device set was persisted"),
            Err(err) => warn!(
                path = %path.display(),
                %err,
                "the device set could not be persisted; the change is live but will not \
                 survive a restart"
            ),
        }
    }

    /// [`apply`](Self::apply)'s body, for a caller already holding the document
    /// lock.
    fn apply_locked(&self, resolved: Resolved) -> RegistryChange {
        let change = self.registry.apply(resolved);
        self.generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        change
    }

    /// The summary the inventory routes serve for `id`, after a change.
    fn summary_of(resolved: &Resolved, id: &str) -> Result<DeviceSummary, InventoryRefusal> {
        resolved
            .devices
            .iter()
            .find(|device| device.id == id)
            .map(summarize)
            .ok_or_else(|| {
                InventoryRefusal::Malformed(format!("device '{id}' was not in the applied fleet"))
            })
    }
}

/// Map a config-layer failure onto the refusal a caller sees.
///
/// Wildcard-free, so a new [`ConfigError`] is a build error here until someone
/// decides whose problem it is. Most are the caller's — they describe the body
/// that was just sent — and the two that are not say so: an id collision is a
/// conflict with the fleet rather than a malformed request, and a group naming
/// a device that no longer exists is a removal this surface must refuse.
fn refusal_of(err: ConfigError) -> InventoryRefusal {
    match err {
        ConfigError::DuplicateId(id) => InventoryRefusal::Duplicate(format!(
            "'{id}' already names a device or group; use PUT /v1/inventory/devices/{id} to \
             replace it"
        )),
        ConfigError::UnknownGroupMember { group, device } => InventoryRefusal::Blocked(format!(
            "group '{group}' still names device '{device}'; remove it from the group first"
        )),
        other @ (ConfigError::Parse(_)
        | ConfigError::MissingField { .. }
        | ConfigError::UnknownDisabledField { .. }
        | ConfigError::EmptyGroup(_)
        | ConfigError::Io(_)
        | ConfigError::UnsupportedFormat(_)) => InventoryRefusal::Malformed(other.to_string()),
    }
}

/// Project a resolved device back onto the raw form, so the kept document
/// describes exactly the fleet that is running.
///
/// Every value is stated explicitly rather than left to inherit, and that is
/// deliberate: a device added through the API resolved against the `[defaults]`
/// of the moment, and leaving its keys blank would let a later edit to those
/// defaults silently re-resolve it into a different device.
///
/// The credential is the one exception, and it has to be: a resolved
/// `DeviceConfig` holds the password it was built with, so stating it here keeps
/// the round trip exact — a device whose credential came from `[defaults]` and
/// one that stated its own are the same device, and both must survive the next
/// amendment.
fn raw_of(config: &DeviceConfig, _defaults: &Defaults) -> RawDevice {
    RawDevice {
        id: config.id.clone(),
        host: config.host.clone(),
        port: Some(config.port),
        username: Some(config.username.clone()),
        password: Some(Password::from(config.password.expose_secret())),
        connect_secs: Some(config.connect_timeout.as_secs()),
        exchange_secs: Some(config.exchange_timeout.as_secs()),
        eager: Some(config.eager),
        sis_keepalive_secs: Some(config.sis_keepalive.map_or(0, |d| d.as_secs())),
        eager_retry_secs: Some(config.eager_retry.map_or(0, |d| d.as_secs())),
        cold_backoff_secs: Some(config.cold_backoff.map_or(0, |d| d.as_secs())),
        disabled_fields: Some(config.disabled_fields.iter().cloned().collect()),
        auto_disable_after: Some(config.auto_disable_after),
        self_heal_secs: Some(config.self_heal.map_or(0, |d| d.as_secs())),
    }
}

/// Turn a caller's [`DeviceWrite`] into the raw device the config layer
/// resolves.
///
/// Nothing is validated here beyond the id being present: every other rule —
/// credentials resolvable, field names real, ids unique — belongs to
/// [`resolve_config`], and re-stating any of them here would be a second place
/// for them to drift.
fn raw_of_write(id: String, write: DeviceWrite) -> RawDevice {
    RawDevice {
        id,
        host: write.host,
        port: write.port,
        username: write.username,
        password: write.password.map(Password::from),
        connect_secs: write.connect_secs,
        exchange_secs: write.exchange_secs,
        eager: write.eager,
        sis_keepalive_secs: write.sis_keepalive_secs,
        eager_retry_secs: write.eager_retry_secs,
        cold_backoff_secs: write.cold_backoff_secs,
        disabled_fields: write.disabled_fields,
        auto_disable_after: write.auto_disable_after,
        self_heal_secs: write.self_heal_secs,
    }
}

#[async_trait::async_trait]
impl LiveInventory for LiveFleet {
    async fn add(&self, write: DeviceWrite) -> Result<DeviceSummary, InventoryRefusal> {
        let id = write.id.clone().ok_or_else(|| {
            InventoryRefusal::Malformed(
                "an added device must state an `id`; the URL does not name one".to_owned(),
            )
        })?;
        let raw = raw_of_write(id.clone(), write);

        // No "does it exist" check: appending a duplicate id is exactly what
        // `resolve_config` refuses, and letting it do so keeps one rule in one
        // place. `refusal_of` turns that into the `409` a caller expects.
        let resolved = self.amend(move |devices| {
            devices.push(raw);
            Ok(())
        })?;
        info!(device = %id, "a device was added to the running fleet");
        Self::summary_of(&resolved, &id)
    }

    async fn replace(
        &self,
        id: &str,
        write: DeviceWrite,
    ) -> Result<DeviceSummary, InventoryRefusal> {
        if let Some(stated) = &write.id
            && stated != id
        {
            return Err(InventoryRefusal::Malformed(format!(
                "the body names device '{stated}' and the path names '{id}'; a replace states \
                 the id once"
            )));
        }
        let raw = raw_of_write(id.to_owned(), write);
        let target = id.to_owned();

        let resolved = self.amend(move |devices| {
            let slot = devices
                .iter_mut()
                .find(|device| device.id == target)
                .ok_or_else(|| {
                    InventoryRefusal::Unknown(format!("no device '{target}' is configured"))
                })?;
            // In place, so the device keeps its position in the document and a
            // replace does not reorder the fleet.
            *slot = raw;
            Ok(())
        })?;
        info!(device = %id, "a device was replaced in the running fleet");
        Self::summary_of(&resolved, id)
    }

    async fn remove(&self, id: &str) -> Result<Removed, InventoryRefusal> {
        let target = id.to_owned();

        // Step one: take it out of the fleet. Everything that holds a device
        // handle reconciles off the announcement this makes, so by the time
        // `amend` returns the poll loops are cancelled and the relay task for
        // this device has been told to stop.
        //
        // A group that still names it is refused *here*, by `resolve_config`,
        // before anything has been cancelled — which is why the refusal path
        // leaves no trace.
        self.amend(move |devices| {
            let before = devices.len();
            devices.retain(|device| device.id != target);
            if devices.len() == before {
                return Err(InventoryRefusal::Unknown(format!(
                    "no device '{target}' is configured"
                )));
            }
            Ok(())
        })?;

        // Step two, and only now: nothing may be dispatched to a device that is
        // no longer in the fleet, and the task that would have dispatched it has
        // already been cancelled. Doing this first would leave a window in which
        // the relay claimed a write between the cancel and the removal.
        let writes_canceled = self
            .outbox
            .cancel_queued(id, "the device was removed from the fleet");

        // Step three, and only if the deployment says so. The write *log* is
        // never purged — a caller polling a cancelled write has to be able to
        // learn it was cancelled, and a `404` says nothing at all.
        let reads_dropped = self.cleanup_on_remove.then(|| self.store.forget_device(id));

        info!(
            device = %id,
            writes_canceled,
            reads_dropped,
            "a device was removed from the running fleet"
        );
        Ok(Removed {
            device: id.to_owned(),
            writes_canceled,
            reads_dropped,
        })
    }

    async fn export(&self, query: &ExportQuery) -> Result<String, InventoryRefusal> {
        // The *resolved* fleet, not the raw document: an export states every key
        // explicitly, and only resolution knows what the blanks became.
        let devices: Vec<DeviceConfig> = self
            .registry
            .devices()
            .into_iter()
            .map(|device| device.config().clone())
            .collect();
        let mut devices = devices;
        devices.sort_by(|a, b| a.id.cmp(&b.id));

        // Only fields actually *disabled*. One still being watched — refused
        // once against a threshold of two — is evidence of nothing yet, and
        // writing it into the document would turn a transient into a permanent
        // fact an operator then has to find and undo.
        let promoted: BTreeMap<String, Vec<String>> = self
            .registry
            .devices()
            .into_iter()
            .map(|device| {
                let inferred = device
                    .auto_disabled()
                    .snapshot()
                    .into_iter()
                    .filter(|field| field.disabled)
                    .map(|field| field.name)
                    .collect();
                (device.id().to_owned(), inferred)
            })
            .collect();

        let groups = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let resolved_groups = resolve_config(groups.clone()).map_err(refusal_of)?.groups;
        drop(groups);

        render(&devices, &resolved_groups, &promoted, query)
    }

    async fn reset(&self) -> Result<sismatic_api_types::DeviceList, InventoryRefusal> {
        let reloaded = load_raw(&self.devices_path).map_err(|e| {
            InventoryRefusal::Source(format!("reading {}: {e}", self.devices_path.display()))
        })?;

        let mut document = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Resolved before anything is adopted, so a file that has gone bad since
        // startup leaves the running fleet exactly as it was.
        let resolved = resolve_config(reloaded.clone()).map_err(refusal_of)?;

        *document = reloaded;
        self.catalog.replace(
            resolved.devices.iter().map(summarize).collect(),
            resolved.groups.iter().map(summarize_group).collect(),
        );
        let change = self.apply_locked(resolved.clone());
        drop(document);

        // Devices the file does not describe are *removed*, which strands their
        // queued writes exactly as a `DELETE` would — so they are cancelled by
        // the same path, rather than left owed to ids nothing can address.
        for id in &change.removed {
            let canceled = self
                .outbox
                .cancel_queued(id, "the device was removed by an inventory reset");
            if self.cleanup_on_remove {
                self.store.forget_device(id);
            }
            if canceled > 0 {
                info!(device = %id, canceled, "a reset cancelled queued writes");
            }
        }

        self.persist(&resolved);
        info!(
            added = change.added.len(),
            removed = change.removed.len(),
            replaced = change.replaced.len(),
            unchanged = change.unchanged,
            path = %self.devices_path.display(),
            "the device set was reset to the devices file"
        );
        Ok(sismatic_api_types::DeviceList {
            devices: resolved.devices.iter().map(summarize).collect(),
        })
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

/// The exported document, as a serializable shape of its own.
///
/// A parallel type rather than `Serialize` on [`RawConfig`], and that is the
/// point: core's [`Password`] deliberately has no `Serialize` impl, so a
/// credential cannot be written out by any code path that merely reaches for
/// the obvious derive. Exporting one has to be *asked for*, and asking is what
/// [`ExportQuery::include_secrets`] is. Deriving `Serialize` over there would
/// remove that guarantee for one convenience.
///
/// Field-for-field the devices file's shape, so a document rendered here is one
/// the loader reads back — which is the whole purpose of the route.
#[derive(Debug, serde::Serialize)]
struct ExportDocument {
    #[serde(skip_serializing_if = "ExportDefaults::is_empty")]
    defaults: ExportDefaults,
    #[serde(rename = "device", skip_serializing_if = "Vec::is_empty")]
    devices: Vec<ExportDevice>,
    #[serde(rename = "group", skip_serializing_if = "Vec::is_empty")]
    groups: Vec<ExportGroup>,
}

#[derive(Debug, Default, serde::Serialize)]
struct ExportDefaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

impl ExportDefaults {
    /// Whether the whole table can be left out. TOML renders an empty table as
    /// a bare `[defaults]` header, which reads as an assertion that there are
    /// none rather than as silence.
    fn is_empty(&self) -> bool {
        self.username.is_none() && self.password.is_none()
    }
}

#[derive(Debug, serde::Serialize)]
struct ExportDevice {
    id: String,
    host: String,
    port: u16,
    username: String,
    /// Present only when the export was asked for secrets.
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    connect_secs: u64,
    exchange_secs: u64,
    eager: bool,
    sis_keepalive_secs: u64,
    eager_retry_secs: u64,
    cold_backoff_secs: u64,
    disabled_fields: Vec<String>,
    auto_disable_after: u32,
    self_heal_secs: u64,
}

#[derive(Debug, serde::Serialize)]
struct ExportGroup {
    id: String,
    devices: Vec<String>,
    barrier_timeout_secs: u64,
    barrier: &'static str,
}

/// Which format a state file's extension implies.
///
/// TOML when it says nothing, matching the devices file's own default — so an
/// `inventory.state_path` written without an extension produces something the
/// loader still reads, rather than something that silently round-trips as the
/// wrong format.
fn format_of(path: &Path) -> ExportFormat {
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => ExportFormat::Json,
        Some("yaml" | "yml") => ExportFormat::Yaml,
        _ => ExportFormat::Toml,
    }
}

/// Render `document` in `query.format`.
///
/// Every key is stated explicitly rather than left to inherit. A device exported
/// with blank keys would re-resolve against whatever `[defaults]` the *importing*
/// file happens to carry, which is a different device — and silently so. An
/// export is a description of what is running, so it says all of it.
fn render(
    fleet: &[DeviceConfig],
    groups: &[GroupConfig],
    promoted: &BTreeMap<String, Vec<String>>,
    query: &ExportQuery,
) -> Result<String, InventoryRefusal> {
    let document = ExportDocument {
        defaults: ExportDefaults::default(),
        devices: fleet
            .iter()
            .map(|device| {
                let mut disabled: BTreeSet<String> = device.disabled_fields.clone();
                if query.promote_auto_disabled_fields_to_disabled_fields
                    && let Some(inferred) = promoted.get(&device.id)
                {
                    disabled.extend(inferred.iter().cloned());
                }
                ExportDevice {
                    id: device.id.clone(),
                    host: device.host.clone(),
                    port: device.port,
                    username: device.username.clone(),
                    password: query
                        .include_secrets
                        .then(|| device.password.expose_secret().to_owned()),
                    connect_secs: device.connect_timeout.as_secs(),
                    exchange_secs: device.exchange_timeout.as_secs(),
                    eager: device.eager,
                    sis_keepalive_secs: device.sis_keepalive.map_or(0, |d| d.as_secs()),
                    eager_retry_secs: device.eager_retry.map_or(0, |d| d.as_secs()),
                    cold_backoff_secs: device.cold_backoff.map_or(0, |d| d.as_secs()),
                    disabled_fields: disabled.into_iter().collect(),
                    auto_disable_after: device.auto_disable_after,
                    self_heal_secs: device.self_heal.map_or(0, |d| d.as_secs()),
                }
            })
            .collect(),
        groups: groups
            .iter()
            .map(|group| ExportGroup {
                id: group.id.clone(),
                devices: group.device_ids.clone(),
                barrier_timeout_secs: group.barrier_timeout.as_secs(),
                // The file's spellings, not the enum's: an export has to load.
                barrier: match group.barrier {
                    Barrier::FailBatch => "fail",
                    Barrier::DispatchReady => "dispatch-ready",
                },
            })
            .collect(),
    };

    let rendered = match query.format {
        ExportFormat::Toml => toml::to_string_pretty(&document).map_err(|e| e.to_string()),
        ExportFormat::Json => serde_json::to_string_pretty(&document).map_err(|e| e.to_string()),
        // No YAML *serializer* in this workspace — `serde-saphyr` reads and does
        // not write — and JSON is a subset of YAML 1.2, so a JSON rendering is a
        // valid YAML document that the loader's YAML path parses unchanged. It
        // is not idiomatic YAML, which is the honest cost of not adding a
        // dependency for one route's output format.
        ExportFormat::Yaml => serde_json::to_string_pretty(&document).map_err(|e| e.to_string()),
    };
    rendered.map_err(|e| InventoryRefusal::Source(format!("rendering the devices document: {e}")))
}

#[cfg(test)]
mod tests {

    use sismatic_api_types::{Intent, Read, ReadValue, Timestamp, WriteStatus};
    use sismatic_core::devices::config::RawGroup;
    use sismatic_core::devices::connector::fake::CountingConnector;
    use sismatic_core::devices::transport::fake::FakeTransport;
    use sismatic_store::outbox::{Submission, WriteLog, WriteSubmit};
    use sismatic_store::{ReadStore, WriteStore};

    use super::*;

    /// The fleet `document(ids)` resolves to — the same devices `live(ids, _)`
    /// built, so a test can re-apply them and mean *no change*.
    ///
    /// Rebuilding them by hand would not: identity is derived from every key, so
    /// a hand-written `DeviceConfig` that differs in a timeout is a different
    /// device and re-applying it reads as a replacement.
    fn resolved_of(ids: &[&str]) -> Resolved {
        resolve_config(document(ids)).expect("the fixture resolves")
    }

    /// A raw document over `ids`, with credentials in `[defaults]` so a
    /// `DeviceWrite` need not state them — which is also what exercises the
    /// inheritance an added device depends on.
    fn document(ids: &[&str]) -> RawConfig {
        RawConfig {
            defaults: Defaults {
                username: Some("admin".to_owned()),
                password: Some(Password::from("extron")),
                ..Defaults::default()
            },
            devices: ids
                .iter()
                .map(|id| RawDevice {
                    id: (*id).to_owned(),
                    host: "10.0.0.1".to_owned(),
                    port: None,
                    username: None,
                    password: None,
                    connect_secs: None,
                    exchange_secs: None,
                    eager: None,
                    sis_keepalive_secs: None,
                    eager_retry_secs: None,
                    cold_backoff_secs: None,
                    disabled_fields: None,
                    auto_disable_after: None,
                    self_heal_secs: None,
                })
                .collect(),
            groups: Vec::new(),
        }
    }

    /// A `LiveFleet` over `ids`, with `cleanup_on_remove` as given.
    fn live(ids: &[&str], cleanup_on_remove: bool) -> (LiveFleet, MemoryOutbox, MemoryStore) {
        let raw = document(ids);
        let resolved = resolve_config(raw.clone()).expect("the fixture resolves");
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads(["2.11\r\n"; 8])
        }));
        let registry = Arc::new(Registry::build(
            resolved.devices.clone(),
            resolved.groups.clone(),
            connector,
        ));
        let catalog = MemoryCatalog::new(
            resolved.devices.iter().map(summarize).collect(),
            resolved.groups.iter().map(summarize_group).collect(),
        );
        let outbox = MemoryOutbox::with_max_attempts(1);
        let store = MemoryStore::default();
        let fleet = LiveFleet::new(Wiring {
            registry,
            document: raw,
            catalog,
            outbox: outbox.clone(),
            store: store.clone(),
            cleanup_on_remove,
            devices_path: PathBuf::from("devices-for-these-tests.toml"),
            state_path: None,
        });
        (fleet, outbox, store)
    }

    async fn queue_a_write(outbox: &MemoryOutbox, device: &str, id: &str) {
        outbox
            .submit(Submission {
                ids: vec![id.to_owned()],
                targets: vec![device.to_owned()],
                group: None,
                batch: None,
                barrier: None,
                intent: Intent::SetSetting {
                    field: "TIMEZONE".to_owned(),
                    value: "Europe/Vienna".to_owned(),
                },
                at: Timestamp("2026-08-17T00:00:00.000Z".to_owned()),
                idempotency_key: None,
            })
            .await
            .expect("the submission");
    }

    async fn seed_a_read(store: &MemoryStore, device: &str) {
        store
            .upsert_latest(Read {
                device: device.to_owned(),
                field: "FIRMWARE".to_owned(),
                value: ReadValue::Text("2.11".to_owned()),
                at: Timestamp("2026-08-17T00:00:00.000Z".to_owned()),
            })
            .await
            .expect("seeding");
    }

    // ---- the mutation verbs ----------------------------------------------

    /// An added device inherits `[defaults]` exactly as one in the file does.
    /// That is the whole reason the raw document survives startup, rather than
    /// the server keeping only the resolved fleet.
    #[tokio::test]
    async fn an_added_device_inherits_the_files_defaults() {
        let (fleet, ..) = live(&["first"], false);

        let summary = fleet
            .add(DeviceWrite {
                id: Some("second".to_owned()),
                host: "10.0.0.2".to_owned(),
                ..blank_write()
            })
            .await
            .expect("a device stating no credentials should inherit them");

        assert_eq!(summary.id, "second");
        assert_eq!(fleet.registry().len(), 2);
        // Resolved through core, so the id is addressable immediately.
        assert!(fleet.registry().device("second").is_some());
    }

    /// A duplicate id reaches the caller as a conflict, and — the part worth
    /// pinning — the running fleet is untouched. `amend` resolves a *copy*, so a
    /// refused edit leaves neither the document nor the registry moved.
    #[tokio::test]
    async fn a_refused_add_leaves_the_fleet_untouched() {
        let (fleet, ..) = live(&["taken"], false);

        let refusal = fleet
            .add(DeviceWrite {
                id: Some("taken".to_owned()),
                host: "10.0.0.2".to_owned(),
                ..blank_write()
            })
            .await
            .expect_err("a duplicate id");

        assert!(
            matches!(refusal, InventoryRefusal::Duplicate(_)),
            "{refusal:?}"
        );
        assert_eq!(fleet.registry().len(), 1);
        assert_eq!(
            fleet
                .registry()
                .device("taken")
                .expect("still there")
                .config()
                .host,
            "10.0.0.1",
            "the original device must not have been overwritten"
        );
    }

    /// A veto naming nothing is refused rather than ignored — the same rule the
    /// devices file is held to, reaching the API through the same function.
    #[tokio::test]
    async fn an_added_device_with_an_unknown_veto_is_refused() {
        let (fleet, ..) = live(&["first"], false);

        let refusal = fleet
            .add(DeviceWrite {
                id: Some("typo".to_owned()),
                host: "10.0.0.2".to_owned(),
                disabled_fields: Some(vec!["STREAM_9_NAME".to_owned()]),
                ..blank_write()
            })
            .await
            .expect_err("an unknown field name");

        assert!(
            matches!(refusal, InventoryRefusal::Malformed(_)),
            "{refusal:?}"
        );
        assert_eq!(fleet.registry().len(), 1);
    }

    /// Replacing mints a different device — which is what makes the poll loops,
    /// the relay task and the keepalive rebind to it.
    #[tokio::test]
    async fn replacing_a_device_changes_its_identity() {
        let (fleet, ..) = live(&["edited"], false);
        let before = fleet.registry().device("edited").expect("configured");

        let summary = fleet
            .replace(
                "edited",
                DeviceWrite {
                    host: "10.0.0.9".to_owned(),
                    ..blank_write()
                },
            )
            .await
            .expect("the replacement");

        assert_ne!(summary.uuid, before.config().uuid.to_string());
        assert_eq!(
            fleet.registry().device("edited").unwrap().config().host,
            "10.0.0.9"
        );
    }

    /// Omitted keys take the server's default, not the previous device's value.
    /// That is what `PUT` means here, and it is the difference a caller has to
    /// know: read the device first and send back what you want kept.
    #[tokio::test]
    async fn a_replace_does_not_merge_with_the_previous_device() {
        let (fleet, ..) = live(&["edited"], false);
        fleet
            .replace(
                "edited",
                DeviceWrite {
                    host: "10.0.0.1".to_owned(),
                    eager: Some(true),
                    ..blank_write()
                },
            )
            .await
            .expect("the first replacement");
        assert!(fleet.registry().device("edited").unwrap().config().eager);

        fleet
            .replace(
                "edited",
                DeviceWrite {
                    host: "10.0.0.1".to_owned(),
                    ..blank_write()
                },
            )
            .await
            .expect("the second replacement");

        assert!(
            !fleet.registry().device("edited").unwrap().config().eager,
            "an omitted key must fall back to the default, not to what was there"
        );
    }

    // ---- the removal sequence --------------------------------------------

    /// The sequence, end to end: the device leaves the fleet, its queued writes
    /// are cancelled, and — with `cleanup_on_remove` off — its reads are kept.
    #[tokio::test]
    async fn removing_a_device_cancels_its_queue_and_keeps_its_reads() {
        let (fleet, outbox, store) = live(&["goner", "keeper"], false);
        queue_a_write(&outbox, "goner", "doomed").await;
        queue_a_write(&outbox, "keeper", "kept").await;
        seed_a_read(&store, "goner").await;

        let removed = fleet.remove("goner").await.expect("the removal");

        assert_eq!(removed.device, "goner");
        assert_eq!(removed.writes_canceled, 1);
        assert_eq!(
            removed.reads_dropped, None,
            "off means the history was kept, which is not the same as dropping none"
        );
        assert!(fleet.registry().device("goner").is_none());

        let doomed = outbox.write("doomed".to_owned()).await.unwrap().unwrap();
        assert!(
            matches!(doomed.status, WriteStatus::Canceled { .. }),
            "{doomed:?}"
        );
        let kept = outbox.write("kept".to_owned()).await.unwrap().unwrap();
        assert_eq!(
            kept.status,
            WriteStatus::Pending,
            "another device's queue is not this device's to cancel"
        );
        assert_eq!(
            store.latest_all("goner".to_owned()).await.unwrap().len(),
            1,
            "the reads must still be there"
        );
    }

    /// With the policy on, the reads go too — and the count is reported rather
    /// than `null`, which is what distinguishes it from the case above.
    #[tokio::test]
    async fn cleanup_on_remove_drops_the_departed_devices_reads() {
        let (fleet, _outbox, store) = live(&["goner"], true);
        seed_a_read(&store, "goner").await;

        let removed = fleet.remove("goner").await.expect("the removal");

        assert_eq!(removed.reads_dropped, Some(1));
        assert!(
            store
                .latest_all("goner".to_owned())
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The write *log* survives whatever `cleanup_on_remove` says. A caller
    /// polling a write it submitted has to be able to learn it was cancelled,
    /// and a purge would turn that poll into a `404` saying nothing at all.
    #[tokio::test]
    async fn a_cancelled_write_stays_readable_even_with_cleanup_on() {
        let (fleet, outbox, store) = live(&["goner"], true);
        queue_a_write(&outbox, "goner", "doomed").await;
        seed_a_read(&store, "goner").await;

        fleet.remove("goner").await.expect("the removal");

        let doomed = outbox.write("doomed".to_owned()).await.unwrap();
        assert!(
            doomed.is_some(),
            "cleanup_on_remove is about reads; the write log is evidence"
        );
    }

    /// A device a group still names is refused — and refused *before* anything
    /// is cancelled, which is why the refusal path leaves no trace. Resolution
    /// is what catches it, so the check cannot be forgotten by a caller.
    #[tokio::test]
    async fn removing_a_group_member_is_refused_and_cancels_nothing() {
        let mut raw = document(&["member", "other"]);
        raw.groups = vec![RawGroup {
            id: "room".to_owned(),
            devices: vec!["member".to_owned(), "other".to_owned()],
            barrier_timeout_secs: None,
            barrier: None,
        }];
        let resolved = resolve_config(raw.clone()).expect("the fixture resolves");
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads(["2.11\r\n"; 8])
        }));
        let registry = Arc::new(Registry::build(
            resolved.devices.clone(),
            resolved.groups.clone(),
            connector,
        ));
        let catalog = MemoryCatalog::new(
            resolved.devices.iter().map(summarize).collect(),
            resolved.groups.iter().map(summarize_group).collect(),
        );
        let outbox = MemoryOutbox::with_max_attempts(1);
        let fleet = LiveFleet::new(Wiring {
            registry,
            document: raw,
            catalog,
            outbox: outbox.clone(),
            store: MemoryStore::default(),
            cleanup_on_remove: true,
            devices_path: PathBuf::from("devices-for-these-tests.toml"),
            state_path: None,
        });
        queue_a_write(&outbox, "member", "still-owed").await;

        let refusal = fleet.remove("member").await.expect_err("a group holds it");

        assert!(
            matches!(refusal, InventoryRefusal::Blocked(_)),
            "{refusal:?}"
        );
        assert!(
            fleet.registry().device("member").is_some(),
            "a refused removal must leave the device in the fleet"
        );
        let owed = outbox
            .write("still-owed".to_owned())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            owed.status,
            WriteStatus::Pending,
            "nothing may be cancelled for a removal that did not happen"
        );
    }

    #[tokio::test]
    async fn removing_a_device_that_does_not_exist_is_unknown() {
        let (fleet, ..) = live(&["first"], false);
        let refusal = fleet.remove("ghost").await.expect_err("no such device");
        assert!(
            matches!(refusal, InventoryRefusal::Unknown(_)),
            "{refusal:?}"
        );
    }

    /// Every mutation announces, so the sync driver and the fleet reconciler
    /// both learn about it. A verb that changed the registry without publishing
    /// would leave poll loops bound to a device nothing hands out.
    #[tokio::test]
    async fn every_mutation_wakes_the_subscribers() {
        let (fleet, ..) = live(&["first"], false);

        for (label, watcher) in [
            ("add", fleet.subscribe()),
            ("replace", fleet.subscribe()),
            ("remove", fleet.subscribe()),
        ] {
            match label {
                "add" => {
                    fleet
                        .add(DeviceWrite {
                            id: Some("second".to_owned()),
                            host: "10.0.0.2".to_owned(),
                            ..blank_write()
                        })
                        .await
                        .expect("add");
                }
                "replace" => {
                    fleet
                        .replace(
                            "second",
                            DeviceWrite {
                                host: "10.0.0.3".to_owned(),
                                ..blank_write()
                            },
                        )
                        .await
                        .expect("replace");
                }
                _ => {
                    fleet.remove("second").await.expect("remove");
                }
            }
            assert!(
                watcher.has_changed().expect("the sender is alive"),
                "{label} must announce"
            );
        }
    }

    // ---- export ----------------------------------------------------------

    fn export_query(format: ExportFormat) -> ExportQuery {
        ExportQuery {
            format,
            promote_auto_disabled_fields_to_disabled_fields: false,
            include_secrets: false,
        }
    }

    /// The property that makes the route worth having: an export loads.
    ///
    /// Round-tripped through core's own parser rather than checked for
    /// substrings, because "it contains the word host" is not the claim — the
    /// claim is that the devices-file loader reads it back as the same fleet.
    #[tokio::test]
    async fn an_export_round_trips_through_the_devices_file_parser() {
        let (fleet, ..) = live(&["atrium", "annex"], false);

        let exported = fleet
            .export(&ExportQuery {
                include_secrets: true,
                ..export_query(ExportFormat::Toml)
            })
            .await
            .expect("the export");

        let reloaded = sismatic_core::devices::config::from_toml_str(&exported)
            .expect("an export must be a loadable devices file");
        let mut ids: Vec<&str> = reloaded.devices.iter().map(|d| d.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["annex", "atrium"]);
        // Identity survives, which is the strongest statement available: it is
        // derived from every key, so two fleets agreeing on it agree on all of
        // them.
        let before = fleet.registry().device("atrium").unwrap().config().uuid;
        assert_eq!(
            reloaded
                .devices
                .iter()
                .find(|d| d.id == "atrium")
                .unwrap()
                .uuid,
            before,
            "an exported device must resolve back to the same device"
        );
    }

    /// Credentials are omitted unless asked for. The omission is what makes the
    /// default export unloadable, and that is the trade: an export lands in
    /// shell history and CI logs.
    #[tokio::test]
    async fn an_export_omits_credentials_unless_asked() {
        let (fleet, ..) = live(&["atrium"], false);

        let without = fleet
            .export(&export_query(ExportFormat::Toml))
            .await
            .expect("the export");
        assert!(
            !without.contains("extron"),
            "the password must not be in a default export:\n{without}"
        );
        assert!(without.contains("admin"), "the username is not a secret");

        let with = fleet
            .export(&ExportQuery {
                include_secrets: true,
                ..export_query(ExportFormat::Toml)
            })
            .await
            .expect("the export");
        assert!(with.contains("extron"), "asked for, it is there");
    }

    /// The discovery loop: what the fleet inferred becomes what the document
    /// declares, so committing the export makes it permanent — and free, since a
    /// declared veto starts no poll loop at all.
    #[tokio::test]
    async fn promotion_writes_inferred_vetoes_into_the_document() {
        let (fleet, ..) = live(&["refuser"], false);
        let device = fleet.registry().device("refuser").expect("configured");
        // Disabled: two refusals against a threshold of two.
        device.auto_disabled().refused("STREAM_2_NAME", 2, None);
        device.auto_disabled().refused("STREAM_2_NAME", 2, None);
        // Watched only: one refusal, below the threshold.
        device.auto_disabled().refused("STREAM_3_NAME", 2, None);

        let promoted = fleet
            .export(&ExportQuery {
                promote_auto_disabled_fields_to_disabled_fields: true,
                ..export_query(ExportFormat::Toml)
            })
            .await
            .expect("the export");

        assert!(promoted.contains("STREAM_2_NAME"), "{promoted}");
        assert!(
            !promoted.contains("STREAM_3_NAME"),
            "a field still being watched is evidence of nothing yet, and writing it \
             down would turn a transient into a permanent fact:\n{promoted}"
        );

        // ...and without the switch, neither is written.
        let plain = fleet
            .export(&export_query(ExportFormat::Toml))
            .await
            .expect("the export");
        assert!(!plain.contains("STREAM_2_NAME"), "{plain}");
    }

    /// The JSON rendering is also the YAML one — JSON is a subset of YAML 1.2 —
    /// so both load through core's YAML path. Pinned because it is the one
    /// format whose serializer is not its own.
    #[tokio::test]
    async fn a_yaml_export_loads_through_the_yaml_parser() {
        let (fleet, ..) = live(&["atrium"], false);

        let exported = fleet
            .export(&ExportQuery {
                include_secrets: true,
                ..export_query(ExportFormat::Yaml)
            })
            .await
            .expect("the export");

        let reloaded = sismatic_core::devices::config::from_yaml_str(&exported)
            .expect("a YAML export must load");
        assert_eq!(reloaded.devices.len(), 1);
    }

    // ---- reset and persistence -------------------------------------------

    /// A `LiveFleet` whose devices file is a real one on disk, so `reset` has
    /// something to read.
    fn live_on_disk(
        file_ids: &[&str],
        running_ids: &[&str],
        state: Option<std::path::PathBuf>,
    ) -> (LiveFleet, std::path::PathBuf, MemoryOutbox) {
        let dir = std::env::temp_dir().join(format!(
            "sismatic-fleet-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("devices.toml");

        // The file's fleet, rendered through the very function the export route
        // uses — so the fixture cannot drift from what this server writes.
        let on_disk = resolve_config(document(file_ids)).expect("the file resolves");
        let rendered = render(
            &on_disk.devices,
            &on_disk.groups,
            &BTreeMap::new(),
            &ExportQuery {
                format: ExportFormat::Toml,
                promote_auto_disabled_fields_to_disabled_fields: false,
                include_secrets: true,
            },
        )
        .expect("rendering the fixture");
        std::fs::write(&path, rendered).expect("writing the fixture");

        let raw = document(running_ids);
        let resolved = resolve_config(raw.clone()).expect("the running fleet resolves");
        let connector = Arc::new(CountingConnector::new(|| {
            FakeTransport::with_reads(["2.11\r\n"; 8])
        }));
        let registry = Arc::new(Registry::build(
            resolved.devices.clone(),
            resolved.groups.clone(),
            connector,
        ));
        let catalog = MemoryCatalog::new(
            resolved.devices.iter().map(summarize).collect(),
            resolved.groups.iter().map(summarize_group).collect(),
        );
        let outbox = MemoryOutbox::with_max_attempts(1);
        let fleet = LiveFleet::new(Wiring {
            registry,
            document: raw,
            catalog,
            outbox: outbox.clone(),
            store: MemoryStore::default(),
            cleanup_on_remove: false,
            devices_path: path.clone(),
            state_path: state,
        });
        (fleet, path, outbox)
    }

    /// The escape hatch: runtime drift is undone and the file is authoritative
    /// again, without a restart.
    #[tokio::test]
    async fn a_reset_adopts_the_file_and_discards_runtime_changes() {
        let (fleet, ..) = live_on_disk(&["from-file"], &["from-file", "added-at-runtime"], None);
        assert_eq!(fleet.registry().len(), 2);

        let after = fleet.reset().await.expect("the reset");

        assert_eq!(
            after
                .devices
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            ["from-file"]
        );
        assert!(fleet.registry().device("added-at-runtime").is_none());
    }

    /// A reset removes devices, and a removal strands queued writes — so it
    /// cancels them by the same path a `DELETE` does, rather than leaving them
    /// owed to an id nothing can address.
    #[tokio::test]
    async fn a_reset_cancels_writes_for_devices_the_file_does_not_describe() {
        let (fleet, _path, outbox) =
            live_on_disk(&["from-file"], &["from-file", "added-at-runtime"], None);
        queue_a_write(&outbox, "added-at-runtime", "stranded").await;
        queue_a_write(&outbox, "from-file", "kept").await;

        fleet.reset().await.expect("the reset");

        let stranded = outbox.write("stranded".to_owned()).await.unwrap().unwrap();
        assert!(
            matches!(stranded.status, WriteStatus::Canceled { .. }),
            "{stranded:?}"
        );
        let kept = outbox.write("kept".to_owned()).await.unwrap().unwrap();
        assert_eq!(kept.status, WriteStatus::Pending);
    }

    /// A devices file that has gone missing since startup leaves the running
    /// fleet exactly as it was — the reset is refused, not half-applied.
    #[tokio::test]
    async fn a_reset_that_cannot_read_the_file_changes_nothing() {
        let (fleet, path, _) = live_on_disk(&["a"], &["a", "b"], None);
        std::fs::remove_file(&path).expect("removing the fixture");

        let refusal = fleet.reset().await.expect_err("the file is gone");

        assert!(
            matches!(refusal, InventoryRefusal::Source(_)),
            "{refusal:?}"
        );
        assert_eq!(fleet.registry().len(), 2, "the fleet must be untouched");
    }

    /// With `state_path` set, a mutation is written to disk — which is what
    /// makes it survive a restart.
    #[tokio::test]
    async fn a_mutation_is_persisted_when_a_state_path_is_configured() {
        let state = std::env::temp_dir().join(format!(
            "sismatic-state-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&state);
        let (fleet, ..) = live_on_disk(&["first"], &["first"], Some(state.clone()));

        fleet
            .add(DeviceWrite {
                id: Some("second".to_owned()),
                host: "10.0.0.2".to_owned(),
                ..blank_write()
            })
            .await
            .expect("the add");

        let written = std::fs::read_to_string(&state).expect("the state file");
        let reloaded = sismatic_core::devices::config::from_toml_str(&written)
            .expect("persisted state must be loadable");
        let mut ids: Vec<&str> = reloaded.devices.iter().map(|d| d.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["first", "second"]);
        assert!(
            written.contains("extron"),
            "the state file carries credentials, because it exists to be loadable"
        );
        let _ = std::fs::remove_file(&state);
    }

    /// Without one, nothing is written. The default is what keeps the devices
    /// file unambiguously authoritative.
    #[tokio::test]
    async fn no_state_path_means_no_file_is_written() {
        let (fleet, path, _) = live_on_disk(&["first"], &["first"], None);
        let dir = path.parent().expect("a directory").to_owned();
        let before: Vec<_> = std::fs::read_dir(&dir)
            .expect("listing")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();

        fleet
            .add(DeviceWrite {
                id: Some("second".to_owned()),
                host: "10.0.0.2".to_owned(),
                ..blank_write()
            })
            .await
            .expect("the add");

        let after: Vec<_> = std::fs::read_dir(&dir)
            .expect("listing")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            before.len(),
            after.len(),
            "nothing should have been written"
        );
    }

    /// A `DeviceWrite` with only the two keys that have no default.
    fn blank_write() -> DeviceWrite {
        DeviceWrite {
            id: None,
            host: String::new(),
            port: None,
            username: None,
            password: None,
            connect_secs: None,
            exchange_secs: None,
            eager: None,
            sis_keepalive_secs: None,
            eager_retry_secs: None,
            cold_backoff_secs: None,
            disabled_fields: None,
            auto_disable_after: None,
            self_heal_secs: None,
        }
    }

    /// Subscribers created at startup see the *change*, not merely the value it
    /// left behind. A receiver made after the fact would read the current
    /// generation with nothing pending.
    #[tokio::test]
    async fn applying_a_change_wakes_every_subscriber() {
        let (fleet, ..) = live(&["a"], false);
        let first = fleet.subscribe();
        let second = fleet.subscribe();

        fleet.apply(resolved_of(&["a", "b"]));

        assert!(first.has_changed().expect("the sender is alive"));
        assert!(second.has_changed().expect("the sender is alive"));
        assert_eq!(fleet.registry().len(), 2);
    }

    /// The registry is mutated before the announcement, so a subscriber that
    /// wakes immediately reads the fleet the announcement is about. Announcing
    /// first would be an edge with no second chance behind it.
    #[tokio::test]
    async fn the_registry_is_current_by_the_time_subscribers_wake() {
        let (fleet, ..) = live(&["a"], false);
        let mut watcher = fleet.subscribe();
        let registry = Arc::clone(fleet.registry());

        let seen = tokio::spawn(async move {
            watcher.changed().await.expect("a change");
            registry.ids().len()
        });
        tokio::task::yield_now().await;

        fleet.apply(resolved_of(&["a", "b"]));

        assert_eq!(seen.await.expect("the watcher task"), 2);
    }

    /// A no-op apply still announces. Whether a subscriber has work is its own
    /// diff's business — the sync driver's schedule may have moved even when the
    /// fleet did not — so this must not decide on its behalf.
    #[tokio::test]
    async fn an_unchanged_fleet_still_announces() {
        let (fleet, ..) = live(&["a"], false);
        let watcher = fleet.subscribe();

        // The very fleet the fixture is running, re-applied.
        let change = fleet.apply(resolved_of(&["a"]));

        assert!(change.is_nothing(), "{change:?}");
        assert!(watcher.has_changed().expect("the sender is alive"));
    }
}
