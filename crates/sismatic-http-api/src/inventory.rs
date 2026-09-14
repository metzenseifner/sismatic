//! The inventory port: changing which devices exist while the server runs.
//!
//! [`LiveConfig`](crate::config::LiveConfig)'s counterpart for the fleet, and
//! declared here for the same reason: what is behind it is the composition root
//! — the registry, the outbox whose queues a departing device leaves behind, the
//! store that may or may not keep its history — and no crate but the root can
//! supply any of that.
//!
//! # Why the reads and the writes are different ports
//!
//! [`DeviceCatalog`] answers what is configured, and every read route in this
//! crate holds it. This trait changes what is configured, and one scope holds
//! it. Folding the two together would hand every reads handler a method that
//! removes a recorder, which is the narrowing the rest of this crate is built
//! on — the write routes cannot drain a queue, the reads routes cannot write a
//! read, and a handler that renders an index cannot delete a device.
//!
//! # A device is replaced, never edited
//!
//! There is no `patch`. A [`DeviceConfig`] is immutable: changing any key
//! produces a different device, with a different identity, whose warm SSH
//! session is a new one. So the verbs are add, replace and remove, and
//! [`replace`](LiveInventory::replace) takes a whole [`DeviceWrite`] rather than
//! a delta — a body stating only what moved would describe something this system
//! has no representation for.
//!
//! # Removal is a sequence, not a deletion
//!
//! [`remove`](LiveInventory::remove) is the one method here with an ordering
//! requirement, and it is the implementation's to keep: the device's relay task
//! has to be told to stop *before* its queue is cancelled, or the relay can
//! claim a write in the gap and dispatch it to a recorder that is on its way out
//! of the fleet. See the adapter for the whole sequence.
//!
//! [`DeviceCatalog`]: sismatic_store::catalog::DeviceCatalog
//! [`DeviceConfig`]: https://docs.rs/sismatic-core

use std::sync::Arc;

use sismatic_api_types::{
    DeviceSummary, DeviceWrite, ExportQuery, GroupSummary, GroupWrite, Removed,
};

/// A convenient object-safe handle, as `DynLiveConfig` is.
pub type DynLiveInventory = Arc<dyn LiveInventory>;

/// Why a change to the fleet was not made.
///
/// Three cases, three different people's problems — the same split
/// [`ConfigRefusal`](crate::config::ConfigRefusal) makes, and for the same
/// reason: one string would make a handler guess at a status code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryRefusal {
    /// The body did not describe a device this server can build: a required key
    /// absent from both the body and the file's `[defaults]`, or a
    /// `disabled_fields` entry naming nothing.
    ///
    /// The caller's, and a `400`.
    Malformed(String),
    /// An id that already exists, where the route needed one that did not.
    ///
    /// A `409`, not a `400`: the body is well-formed and the request is exactly
    /// what a caller would send to *replace* the device — which is a `PUT` — so
    /// the message names that route.
    Duplicate(String),
    /// No device has this id.
    ///
    /// A `404`. Distinct from `Duplicate` in the obvious way and worth its own
    /// case because a `PUT` and a `DELETE` can each produce it.
    Unknown(String),
    /// The change would break something the server will not break on the
    /// caller's behalf — today, removing a device a group still names.
    ///
    /// A `409`, and the message says what to do instead. Cascading would
    /// silently change what a group means, which is the same silent-partial
    /// failure the write side's barrier defaults against.
    Blocked(String),
    /// Something on this server's side failed: the devices file a reset has to
    /// read is gone, or an export could not be serialized.
    ///
    /// A `500`. The caller sent nothing wrong and there is no other request it
    /// could have made — the same reading [`ConfigRefusal::Source`] gets, and
    /// for the same reason.
    ///
    /// [`ConfigRefusal::Source`]: crate::config::ConfigRefusal::Source
    Source(String),
}

impl std::fmt::Display for InventoryRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InventoryRefusal::Malformed(msg)
            | InventoryRefusal::Duplicate(msg)
            | InventoryRefusal::Unknown(msg)
            | InventoryRefusal::Blocked(msg)
            | InventoryRefusal::Source(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for InventoryRefusal {}

/// The configured device set, changed while the server runs.
#[async_trait::async_trait]
pub trait LiveInventory: Send + Sync {
    /// Add a device that does not exist yet, and report it as the inventory
    /// routes would.
    ///
    /// The id comes from the body here — it is the one route where the caller
    /// has not already named it in the URL.
    async fn add(&self, device: DeviceWrite) -> Result<DeviceSummary, InventoryRefusal>;

    /// Replace the device at `id` wholesale.
    ///
    /// Every key the body omits takes the server's default rather than the
    /// value the old device had. That is what makes this a `PUT`: the result is
    /// a function of the body alone, so the same request applied twice leaves
    /// the same device — which a merge against the previous value would not.
    async fn replace(
        &self,
        id: &str,
        device: DeviceWrite,
    ) -> Result<DeviceSummary, InventoryRefusal>;

    /// Remove the device at `id`, and report what went with it.
    ///
    /// Refused while any group still names it. What becomes of its stored reads
    /// is the deployment's `store.cleanup_on_remove` to decide, which is why
    /// [`Removed::reads_dropped`] is an `Option` rather than a count this port
    /// can always produce.
    async fn remove(&self, id: &str) -> Result<Removed, InventoryRefusal>;

    /// Add a device group that does not exist yet.
    ///
    /// Every member must name a device that exists, and the id must be free in
    /// the namespace devices and groups share. Both are the config layer's
    /// rules, enforced by the same function that enforces them for the file.
    async fn add_group(&self, group: GroupWrite) -> Result<GroupSummary, InventoryRefusal>;

    /// Replace the group at `id` wholesale.
    ///
    /// The same replace-not-merge contract [`replace`](Self::replace) has, and
    /// the one that matters most here: a body stating `devices` replaces the
    /// membership entirely rather than adding to it, so removing a member is
    /// sending the list without it.
    async fn replace_group(
        &self,
        id: &str,
        group: GroupWrite,
    ) -> Result<GroupSummary, InventoryRefusal>;

    /// Remove the group at `id`.
    ///
    /// Unlike removing a device this strands nothing: a group owns no queue of
    /// its own — writes addressed to one are expanded into per-device rows at
    /// submission — so the members keep whatever was accepted for them and it
    /// still dispatches. What goes with the group is the record of what it was
    /// last told, which is a statement about a group that no longer exists.
    ///
    /// This is also what makes a device removal's refusal actionable: a device
    /// a group still names is refused, and this is the route that clears the
    /// way.
    async fn remove_group(&self, id: &str) -> Result<(), InventoryRefusal>;

    /// Render the running configuration as a devices document.
    ///
    /// Text rather than a DTO, because the artifact *is* text: what makes this
    /// route useful is that its output can be saved under the right extension
    /// and loaded back, so the format has to survive the port rather than be
    /// re-derived by a handler that would then own a second copy of the
    /// serialization rules.
    ///
    /// Infallible in the ways that matter — the document is already in memory
    /// and already valid — so the error case is a serializer failing, which is
    /// this process's problem and not a refusal a caller can act on. It is
    /// returned as one anyway rather than panicking, because a poisoned export
    /// should not take the server down.
    async fn export(&self, query: &ExportQuery) -> Result<String, InventoryRefusal>;

    /// Discard runtime changes and adopt the devices file as it stands on disk.
    ///
    /// The escape hatch for a fleet that has drifted: every add, replace and
    /// remove since startup is undone at once, and what is left is exactly what
    /// the file describes. A deployment whose runtime state is persisted also
    /// has that file rewritten, so the reset survives a restart rather than
    /// being re-shadowed by the state it just discarded.
    ///
    /// Reported as a [`DeviceList`] — the fleet as the index now shows it —
    /// because "what am I running after that" is the only question a caller has
    /// next, and answering it here saves a round trip against a fleet that may
    /// have changed a great deal.
    ///
    /// [`DeviceList`]: sismatic_api_types::DeviceList
    async fn reset(&self) -> Result<sismatic_api_types::DeviceList, InventoryRefusal>;
}
