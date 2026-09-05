//! Device and group descriptions for the read side.
//!
//! These mirror the *safe*, public-facing subset of `core`'s `DeviceConfig` and
//! `GroupConfig`. Note what is deliberately absent: **no `username`, no
//! `password`.** The wire contract cannot carry a secret it never needs, so the
//! credential simply has no field here — a stronger guarantee than redaction,
//! because there is nothing to accidentally serialize.

use serde::{Deserialize, Serialize};

use crate::read::Read;
use crate::write::Barrier;
use crate::{DeviceId, GroupId};

/// What the server's connection to a device looks like right now.
///
/// Purely informational — a status dot on a dashboard — and stale the instant
/// it is read: nothing here reserves a connection, so a caller that wants to
/// *use* the device still issues an exchange and handles the failure.
///
/// The wire mirror of `sismatic_core::devices::device::Connectivity`, plus
/// [`Unknown`](ConnectionStatus::Unknown), which core has no equivalent of
/// because core always knows. The composition root maps the four states with a
/// wildcard-free match, so a fifth is a build error at the seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    /// A connection is open and idle. The next exchange reuses it.
    Warm,
    /// An exchange is in flight. The device either holds a connection or is
    /// opening one, and telling those apart would mean waiting for the exchange
    /// to finish — which a status read must not do.
    Busy,
    /// No connection is open, and nothing says one would fail. The resting
    /// state of a device that is not marked `eager`.
    Cold,
    /// A recent dial failed and the cold-backoff window is still open, so a
    /// an exchange issued now fails without even dialing.
    ///
    /// The one value that says the device is *down* rather than merely idle,
    /// which is the distinction [`Cold`](ConnectionStatus::Cold) cannot draw.
    Gated,
    /// The server has not determined the state — no status port is wired, or
    /// the id is configured but absent from the running registry.
    Unknown,
}

/// The at-a-glance description of one device: enough to list and address it,
/// with every secret omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceSummary {
    // See `Read::device` for why the alias is spelled out for utoipa.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub id: DeviceId,
    pub host: String,
    pub port: u16,
    /// Whether this device is configured to connect on startup.
    pub eager: bool,
    pub status: ConnectionStatus,
}

/// A device plus the most recent read of each field the store has seen — the
/// payload for a single-device detail view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceDetail {
    pub device: DeviceSummary,
    /// Latest read per field, most-recent value of each quantity.
    pub latest: Vec<Read>,
}

/// The device index. Wrapped in an object so it can later carry paging/metadata
/// without a breaking change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceList {
    pub devices: Vec<DeviceSummary>,
}

/// A group: a name over member device ids (design note §4 — a group is only an
/// id and the devices it fans out to).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GroupSummary {
    // See `Read::device` for why the alias is spelled out for utoipa.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub id: GroupId,
    #[cfg_attr(feature = "openapi", schema(value_type = Vec<String>))]
    pub members: Vec<DeviceId>,
    /// How long a write addressed to this group waits for every member to be
    /// ready before [`barrier`] decides, in seconds.
    ///
    /// Reported because it is the one configured number that changes what a
    /// caller should expect from a `202`: a group with a fifteen-second barrier
    /// can leave a write pending that long before anything reaches a device,
    /// and a client showing a spinner needs to know which.
    ///
    /// [`barrier`]: GroupSummary::barrier
    pub barrier_timeout_secs: u64,
    pub barrier: Barrier,
}

/// The group index.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GroupList {
    pub groups: Vec<GroupSummary>,
}
