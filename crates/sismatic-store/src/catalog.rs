//! Which devices and groups the server was configured with.
//!
//! The one question the store cannot answer and the registry can. `ReadStore`
//! holds what the sync side *wrote*, so it conflates "no such device" with
//! "this device has answered nothing yet" and says so:
//!
//! > `None` deliberately conflates "no such device", "no such field" and
//! > "known pair, never yet polled" […] A caller that must distinguish them
//! > needs the device registry or the instruction catalog, neither of which is
//! > this port's job.
//!
//! This is that registry, reduced to DTOs so a front-end can consult it without
//! a compile path to `sismatic-core`. It closes two gaps at once.
//!
//! # Why the write side needs it
//!
//! Without a catalog, `POST /v1/commands/devices/typo/recording/start` is accepted: the
//! outbox holds what was submitted and no list of what exists, so it admits the
//! command against a fresh idle phase and answers `202`. The caller then learns
//! its recording never started by polling a command that fails at dispatch,
//! minutes later, with a message about an unknown device. A `404` at submission
//! is the same information, delivered when it is still actionable.
//!
//! That is a deliberate divergence from the read side, which answers an unknown
//! device with an empty list rather than a `404`. The two differ because the
//! failures differ: an empty reading list is *true* of a device that exists and
//! has not answered, whereas a `202` for a device that does not exist is a
//! promise that cannot be kept.
//!
//! # Static, and what that costs
//!
//! Populated once at startup and never changed, because the device set comes
//! from a file read before the server binds. Hence the infallible signatures:
//! there is no I/O behind them and no failure to report. A catalog backed by an
//! inventory database would need `Result`s, which is a port change — cheap
//! here, and better than every caller today handling an error that cannot
//! happen.
//!
//! For the same reason [`DeviceSummary::status`] is whatever the composition
//! root put there and not a live observation: this port describes
//! configuration, and whether a connection is warm right now is a question for
//! the registry, which is on the other side of the seam.
//!
//! [`DeviceSummary::status`]: sismatic_api_types::DeviceSummary::status

use std::sync::Arc;

use sismatic_api_types::{DeviceId, DeviceSummary, GroupSummary};

pub type DynDeviceCatalog = Arc<dyn DeviceCatalog>;

/// The configured device and group set, as DTOs.
#[async_trait::async_trait]
pub trait DeviceCatalog: Send + Sync {
    /// Every configured device, ordered by id.
    ///
    /// Ordered for the same reason [`ReadStore::latest_all`] is: a rendered
    /// index should diff cleanly between requests rather than reflecting an
    /// adapter's hash iteration order.
    ///
    /// [`ReadStore::latest_all`]: crate::ReadStore::latest_all
    async fn devices(&self) -> Vec<DeviceSummary>;

    /// Every configured group, ordered by id.
    async fn groups(&self) -> Vec<GroupSummary>;

    /// One device by id, or `None` if no device has it.
    async fn device(&self, id: &str) -> Option<DeviceSummary>;

    /// One group by id, or `None` if no group has it.
    async fn group(&self, id: &str) -> Option<GroupSummary>;

    /// Whether `id` names something a command can be addressed to.
    ///
    /// A provided method rather than a fourth thing an adapter implements, so
    /// "this id is addressable" cannot come to mean something different from
    /// "this id resolves". Devices and groups share one id namespace — the
    /// config layer guarantees they never collide — so at most one lookup can
    /// match.
    async fn contains(&self, id: &str) -> bool {
        self.device(id).await.is_some() || self.group(id).await.is_some()
    }

    /// The devices `id` addresses: the device itself, or a group's members in
    /// the order they were configured. `None` if `id` names neither.
    ///
    /// A device answers with a one-element list rather than `None` so a caller
    /// fanning a command out does not have to ask which kind of id it holds —
    /// which is the shape the group write path needs.
    async fn members(&self, id: &str) -> Option<Vec<DeviceId>> {
        if self.device(id).await.is_some() {
            return Some(vec![id.to_owned()]);
        }
        self.group(id).await.map(|group| group.members)
    }
}
