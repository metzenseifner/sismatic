//! Device and group descriptions for the read side.
//!
//! These mirror the *safe*, public-facing subset of `core`'s `DeviceConfig` and
//! `GroupConfig`. Note what is deliberately absent: **no `username`, no
//! `password`.** The wire contract cannot carry a secret it never needs, so the
//! credential simply has no field here — a stronger guarantee than redaction,
//! because there is nothing to accidentally serialize.

use serde::{Deserialize, Serialize};

use crate::command::Barrier;
use crate::reading::Reading;
use crate::{DeviceId, GroupId};

/// Whether the server currently holds a warm connection to a device. Purely
/// informational (a status dot on a dashboard); it says nothing about the
/// credentials or transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    /// A connection is open.
    Warm,
    /// No connection is currently open.
    Cold,
    /// The server has not yet determined the state.
    Unknown,
}

/// The at-a-glance description of one device: enough to list and address it,
/// with every secret omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceSummary {
    // See `Reading::device` for why the alias is spelled out for utoipa.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub id: DeviceId,
    pub host: String,
    pub port: u16,
    /// Whether this device is configured to connect on startup.
    pub eager: bool,
    pub status: ConnectionStatus,
}

/// A device plus the most recent reading of each field the store has seen — the
/// payload for a single-device detail view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceDetail {
    pub device: DeviceSummary,
    /// Latest reading per field, most-recent value of each quantity.
    pub latest: Vec<Reading>,
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
    // See `Reading::device` for why the alias is spelled out for utoipa.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub id: GroupId,
    #[cfg_attr(feature = "openapi", schema(value_type = Vec<String>))]
    pub members: Vec<DeviceId>,
    /// How long a command addressed to this group waits for every member to be
    /// ready before [`barrier`] decides, in seconds.
    ///
    /// Reported because it is the one configured number that changes what a
    /// caller should expect from a `202`: a group with a fifteen-second barrier
    /// can leave a command pending that long before anything reaches a device,
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
