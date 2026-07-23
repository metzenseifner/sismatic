//! Device and group descriptions for the read side.
//!
//! These mirror the *safe*, public-facing subset of `core`'s `DeviceConfig` and
//! `GroupConfig`. Note what is deliberately absent: **no `username`, no
//! `password`.** The wire contract cannot carry a secret it never needs, so the
//! credential simply has no field here — a stronger guarantee than redaction,
//! because there is nothing to accidentally serialize.

use serde::{Deserialize, Serialize};

use crate::reading::Reading;
use crate::{DeviceId, GroupId};

/// Whether the server currently holds a warm connection to a device. Purely
/// informational (a status dot on a dashboard); it says nothing about the
/// credentials or transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    /// A connection is open and being kept alive.
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
pub struct DeviceSummary {
    pub id: DeviceId,
    pub host: String,
    pub port: u16,
    /// Whether this device is configured to be held warm (`eager`).
    pub eager: bool,
    pub status: ConnectionStatus,
}

/// A device plus the most recent reading of each field the store has seen — the
/// payload for a single-device detail view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DeviceDetail {
    pub device: DeviceSummary,
    /// Latest reading per field, most-recent value of each quantity.
    pub latest: Vec<Reading>,
}

/// The device index. Wrapped in an object so it can later carry paging/metadata
/// without a breaking change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DeviceList {
    pub devices: Vec<DeviceSummary>,
}

/// A group: a name over member device ids (design note §4 — a group is only an
/// id and the devices it fans out to).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GroupSummary {
    pub id: GroupId,
    pub members: Vec<DeviceId>,
}

/// The group index.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GroupList {
    pub groups: Vec<GroupSummary>,
}
