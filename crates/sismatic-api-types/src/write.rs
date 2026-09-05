//! Write-side DTOs: what a caller asks a device to do, and what became of the
//! request.
//!
//! An `Intent` is a *request to act*, not an act. It is accepted, recorded, and
//! answered with `202 Accepted` before any device has been contacted; the
//! device is reached later by `sismatic-relay`. `WriteRecord` is how a caller
//! learns what happened afterwards.
//!
//! # A write, not a command
//!
//! Every accepted request is a *write*, whatever it asks for. Only three of
//! the five [`Intent`]s are commands in the sense `sismatic-core` uses the word
//! — `StartRecording`, `StopRecording` and `PauseRecording` are its `Command`
//! enum — while `SetMetadata` and `SetSetting` write a register and are not
//! commands at all. Naming the whole class after one third of it is what made
//! `GET /v1/commands` ambiguous, so the class is named for what all five have
//! in common: they go out on the write path. [`WritesCatalog`] is where both
//! words survive together, and there `commands` means only the lifecycle trio.
//!
//! [`WritesCatalog`]: crate::WritesCatalog

use serde::{Deserialize, Serialize};

use crate::value::ReadValue;
use crate::{DeviceId, FieldName, Timestamp};

/// A submitted write's identifier. A `String` for the same reason
/// [`DeviceId`] is: it travels as JSON and is opaque to every reader. The
/// server mints a v4 UUID.
pub type WriteId = String;

/// Ties together the rows one group-addressed request was expanded into.
///
/// A `String` for the same reason [`WriteId`] is, and a *separate* id rather
/// than the group's: a group can be addressed many times, and "these rows are
/// one take's worth" is a statement about one request, not about the device
/// group.
pub type BatchId = String;

/// What to do when a batch's barrier does not fill within its timeout.
///
/// The wire mirror of `sismatic_core::devices::config::Barrier`, declared here
/// for the same reason [`RecordingState`](crate::RecordingState) is: the outbox
/// enforces this policy and the outbox cannot see `core`. The composition root
/// maps between the two with a wildcard-free match, so a third variant is a
/// build error at the seam rather than a silent default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Barrier {
    /// Run the members that arrived. One recorder is better than none.
    DispatchReady,
    /// Fail every row in the batch. Two recordings or neither.
    FailBatch,
}

/// What a caller wants done.
///
/// Internally tagged (`{"kind": "set_metadata", "field": "TITLE", ...}`) rather
/// than adjacently tagged like [`ReadValue`]: two variants carry no payload
/// at all, and the adjacent form would render those as `"value": null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Intent {
    StartRecording,
    StopRecording,
    PauseRecording,
    /// Write one metadata register. Admissible only while the device's
    /// [`DesiredRecordingState`] is [`DesiredRecordingState::Idle`] — see
    /// `sismatic_store::outbox::admit`.
    SetMetadata {
        #[cfg_attr(feature = "openapi", schema(value_type = String, example = "TITLE"))]
        field: FieldName,
        value: String,
    },
    /// Write one device setting. Admissible in every desired recording state.
    SetSetting {
        #[cfg_attr(feature = "openapi", schema(value_type = String, example = "TIMEZONE"))]
        field: FieldName,
        value: String,
    },
}

/// The write side's belief about whether a recording is in progress.
///
/// Distinct from [`RecordingState`](crate::value::RecordingState), which is what
/// a device *reported* at some past instant. `DesiredRecordingState` is what the
/// outbox has accepted and not yet seen fail, which is the thing an admission
/// decision has to be taken against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum DesiredRecordingState {
    Idle,
    Recording,
    Paused,
}

/// Why a submission was refused. Carried in the `409 Conflict` body as
/// [`ApiError::rejection`](crate::ApiError::rejection), so a client branches on
/// a value rather than on prose.
///
/// A field of its own rather than four more [`ErrorCode`](crate::ErrorCode)
/// variants, because all four are conflicts and differ only in which
/// precondition refused — one axis is "what kind of failure and what status",
/// the other is "which rule". See [`ApiError`](crate::ApiError) for the whole
/// argument.
///
/// The distinction earns its keep: three of these four say the device is
/// already in the state that was asked for, which a caller can often treat as
/// benign, while [`MetadataFrozen`](Rejection::MetadataFrozen) says an edit was
/// discarded and something has to be done about it. A client that could not
/// tell them apart would either treat every `409` as an error or silently lose
/// metadata edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    /// A metadata write arrived while a recording was in progress.
    MetadataFrozen,
    AlreadyRecording,
    AlreadyPaused,
    NotRecording,
}

/// Prose for the `409` body. Written out rather than `{:?}`-formatted: the
/// message is the only place a caller learns *which* rejection applied, so it
/// has to say what to do about it rather than name a Rust variant.
impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejection::MetadataFrozen => f.write_str(
                "metadata_frozen: a recording is in progress, so this device's metadata \
                 is sealed until it stops",
            ),
            Rejection::AlreadyRecording => {
                f.write_str("already_recording: this device is already recording")
            }
            Rejection::AlreadyPaused => {
                f.write_str("already_paused: this device's recording is already paused")
            }
            Rejection::NotRecording => {
                f.write_str("not_recording: this device has no recording in progress")
            }
        }
    }
}

/// Where a submitted write has got to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WriteStatus {
    /// Recorded, no device contacted yet.
    Pending,
    /// A relay task has claimed it and the SIS exchange is running.
    InFlight,
    /// The device answered. `value` is the decoded echo — for a register write
    /// the device echoes the value it stored, which is worth returning.
    Succeeded { value: ReadValue },
    /// Terminal failure after `attempts` tries.
    Failed { reason: String },
}

/// A device's write-side state, as `GET /v1/writes/devices/{id}/recording` reports it.
/// A product type because the two fields are only meaningful together: an epoch
/// without a desired recording state does not say whether metadata is
/// writable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceDesiredRecordingState {
    pub desired_recording_state: DesiredRecordingState,
    /// Increments on each transition into [`DesiredRecordingState::Recording`]
    /// from [`DesiredRecordingState::Idle`].
    pub epoch: u64,
}

/// One row of the outbox as a caller sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WriteRecord {
    // Annotated for the same reason `device` is: a derive sees the alias name
    // it was written with, so an un-annotated `WriteId` makes utoipa invent a
    // component called `String` and every generated client grows a wrapper type
    // around a plain string. `the_string_aliases_are_documented_as_strings` in
    // `sismatic-http-api` is what notices when one of these is missing.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub id: WriteId,
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub device: DeviceId,
    pub intent: Intent,
    /// The batch this row belongs to, when the request that produced it was
    /// addressed to a group *and* the intent needs the members to act together.
    ///
    /// `None` covers both a device-addressed request and a group-addressed
    /// metadata or setting write: those are idempotent per device and gain
    /// nothing from a rendezvous, so making them wait would only expose them to
    /// a barrier timeout they have no use for.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub batch: Option<BatchId>,
    /// The recording epoch this write was admitted against.
    pub epoch: u64,
    pub status: WriteStatus,
    pub attempts: u32,
    pub enqueued_at: Timestamp,
    pub updated_at: Timestamp,
    // protects a write from quick exhaustion of retries
    pub not_before: Timestamp,
}

/// One write a submission produced. `epoch` is returned so a caller writing
/// several metadata fields can check that all of them landed on one recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Accepted {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub id: WriteId,
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub device: DeviceId,
    pub epoch: u64,
}

/// The `202 Accepted` body.
///
/// Always a list, even for a device-addressed request that can only ever
/// produce one write. The alternative — a bare [`Accepted`] for a device and
/// a list for a group — would make the response shape depend on which kind of
/// id the caller happened to use, so every client would need both parsers and
/// the generated document would carry a `oneOf` for a route that does one
/// thing. One shape costs a wrapper object and buys a contract that does not
/// branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Acceptance {
    /// Set when the members were expanded under a rendezvous, so a caller can
    /// tell "these five will act together" from "these five were merely
    /// submitted at the same moment".
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub batch: Option<BatchId>,
    /// One entry per target, in the order the group lists its members.
    pub writes: Vec<Accepted>,
}

/// A page of writes, wrapped for the same reason
/// [`ReadList`](crate::ReadList) is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WriteList {
    pub writes: Vec<WriteRecord>,
}
