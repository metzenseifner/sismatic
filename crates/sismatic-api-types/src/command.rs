//! Write-side (command) DTOs: the request bodies and results for driving a
//! device *through* the API.
//!
//! These type the shapes the current `sismatic-web` backend builds by hand for
//! its `query`/`command`/`register` routes. They belong in `api-types` because
//! a write-back UI and the server must agree on them, even though the
//! *execution* of a command is a core concern that lives far from this crate.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::value::ReadingValue;
use crate::{DeviceId, FieldName, GroupId};

/// Body of a register write (`POST /devices/{id}/register/{name}`): the new
/// value to store. A single-field object rather than a bare string so the body
/// is self-describing and can gain fields (a units hint, a dry-run flag) later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RegisterWrite {
    pub value: String,
}

/// The result of running one instruction against a single device — the typed
/// form of the web backend's `{ "device", "name", "value" }` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DeviceResult {
    pub device: DeviceId,
    pub field: FieldName,
    pub value: ReadingValue,
}

/// The result of fanning one instruction out across a group: each member's
/// device id mapped to its value. A `BTreeMap` gives a stable, sorted key order
/// so the JSON is deterministic (easier diffs, stable snapshot tests).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GroupResult {
    pub group: GroupId,
    pub field: FieldName,
    pub results: BTreeMap<DeviceId, ReadingValue>,
}
