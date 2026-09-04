//! `sismatic-api-types` — the Sismatic HTTP API serialization contract.
//!
//! This crate is nothing but `serde`-derived Data Transfer Objects (DTOs): the
//! request and response shapes exchanged over the JSON API. It holds **no
//! logic and no I/O**, and — by design — depends on **nothing internal**.
//!
//! # Why it depends on neither `core` nor `db`
//!
//! The load-bearing rule is that **no frontend has a compile path to `sismatic-core`**. Because
//! `api-types` sits at the bottom of that chain, a single edge from here to `core` would give
//! *every* client such a path and silently break the invariant. So this crate re-declares the
//! serialization value model ([`value`]) instead of re-exporting core's `Value`. The translation
//! between the two is done exactly where the two subgraphs already meet (`sismatic-sync` /
//! `sismatic-db`), never here.
//!
//! The same single source of truth is what keeps server and client from
//! disagreeing on JSON: both `serde`-derive from these types, so a renamed field
//! is a *compile* error on both sides rather than a runtime 500 (design note,
//! Deep dive B).
//!
//! # Layout
//!
//! - [`value`] — the decoded value model ([`ReadValue`], [`RecordingState`], …)
//! - [`mod@read`] — [`Read`], [`Timestamp`], and the history-query DTOs
//! - [`device`] — [`DeviceSummary`], [`GroupSummary`], and their list/detail forms
//! - [`group`] — reading a group: [`GroupExpectation`], [`SyncState`], and the
//!   member-wise response shapes
//! - [`mod@write`] — write-side request bodies and instruction results
//! - [`instruction`] — which names the routes above accept: [`FieldCatalog`],
//!   [`WritesCatalog`]
//! - [`error`] — the [`ApiError`] envelope and [`Health`]
//!
//! Enable the `ts` feature to derive `ts_rs::TS` on every DTO and emit
//! TypeScript definitions for the web frontend.
//!
//! ```
//! use sismatic_api_types::{Read, ReadValue, Timestamp};
//!
//! let r = Read {
//!     device: "atrium-101".into(),
//!     field: "SSH_PORT".into(),
//!     value: ReadValue::Port(22023),
//!     at: Timestamp("2026-07-23T14:03:11Z".into()),
//! };
//! let json = serde_json::to_string(&r).unwrap();
//! assert_eq!(
//!     json,
//!     r#"{"device":"atrium-101","field":"SSH_PORT","value":{"type":"port","value":22023},"at":"2026-07-23T14:03:11Z"}"#
//! );
//! ```

pub mod device;
pub mod error;
pub mod group;
pub mod instruction;
pub mod read;
pub mod value;
pub mod write;

/// A device's id. An alias, not a newtype, to match `core`'s `String` ids and
/// stay ergonomic in JSON, while still documenting intent at every use site.
pub type DeviceId = String;

/// A group's id. Groups and devices share one id namespace (design note §4).
pub type GroupId = String;

/// The canonical name of a queryable field (e.g. `"RUNNING_STATE"`) — the
/// `name()` of a `core` `Query`. Kept a string so this crate need not track the
/// instruction catalog.
pub type FieldName = String;

pub use device::{
    ConnectionStatus, DeviceDetail, DeviceList, DeviceSummary, GroupList, GroupSummary,
};
pub use error::{ApiError, ErrorCode, Health, ServiceStatus};
pub use group::{
    GroupDesiredRecordingState, GroupExpectation, GroupFieldState, GroupFieldStateList,
    GroupHistory, GroupWriteList, MemberDesiredRecordingState, MemberHistory, MemberState,
    MemberWrites, SyncState,
};
pub use instruction::{FieldCatalog, InstructionSummary, WritesCatalog};
pub use read::{Read, ReadList, ReadQuery, TimeSpan, Timestamp};
pub use value::{Alarm, MacAddr, ReadValue, RecordingState};
pub use write::{
    Acceptance, Accepted, Barrier, BatchId, DesiredRecordingState, DeviceDesiredRecordingState,
    Intent, Rejection, WriteId, WriteList, WriteRecord, WriteStatus,
};
