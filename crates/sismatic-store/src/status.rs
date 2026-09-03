//! What the server's connections to its devices look like right now.
//!
//! The third question about a device, after "what is configured" ([`catalog`])
//! and "what did it last report" ([`ReadStore`]). Those two are answered from
//! values written earlier; this one is answered from the live registry, and it
//! is the only read in the system whose answer changes without anything being
//! written.
//!
//! # Why it is a port and not a field on the catalog
//!
//! [`DeviceSummary::status`] is on the catalog's summary, and the catalog is a
//! snapshot of configuration taken before the process connects to anything — so
//! it can only ever fill that field with
//! [`Unknown`](sismatic_api_types::ConnectionStatus::Unknown). Making it
//! truthful means reading the registry, and the registry is a `sismatic-core`
//! type that no front-end may name. A second port, taking and returning DTOs,
//! is how the HTTP surface learns the answer without learning the type.
//!
//! # What it does not promise
//!
//! Nothing is reserved and nothing is dialed. Two calls a millisecond apart may
//! disagree, and a device reported [`Warm`] can fail the very next exchange
//! because the far end closed the connection while it was idle — which is
//! precisely the case `Device::run` heals transparently. Treating a status as a
//! precondition would be a race with no upside; the only correct use is
//! display.
//!
//! [`catalog`]: crate::catalog
//! [`ReadStore`]: crate::ReadStore
//! [`DeviceSummary::status`]: sismatic_api_types::DeviceSummary::status
//! [`Warm`]: sismatic_api_types::ConnectionStatus::Warm

use std::collections::BTreeMap;
use std::sync::Arc;

use sismatic_api_types::{ConnectionStatus, DeviceId};

pub type DynDeviceStatus = Arc<dyn DeviceStatus>;

/// Live connection state, keyed by device.
///
/// Infallible, like [`DeviceCatalog`](crate::DeviceCatalog) and for a related
/// reason: there is no I/O behind these, only a read of in-process state. An
/// implementation that had to ask something remote would be answering a
/// different question — "can we reach it", which requires dialing, and dialing
/// is what this exists to avoid.
#[async_trait::async_trait]
pub trait DeviceStatus: Send + Sync {
    /// One device's state. An id the registry does not hold reports
    /// [`ConnectionStatus::Unknown`] rather than an error: the caller has
    /// already established the device exists (the catalog said so), and a
    /// disagreement between the two is this process's problem to log, not the
    /// caller's to handle.
    async fn status(&self, id: &str) -> ConnectionStatus;

    /// Every device's state, in one pass.
    ///
    /// A method of its own rather than a loop over [`status`](Self::status),
    /// because the index route needs the whole fleet and an adapter can walk
    /// its registry once. A `BTreeMap` so the caller can look up by id without
    /// re-scanning, and so iteration order is stable for a rendered page.
    async fn all(&self) -> BTreeMap<DeviceId, ConnectionStatus>;
}
