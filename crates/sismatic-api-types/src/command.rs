//! Write-side DTOs: what a caller asks a device to do, and what became of the
//! request.
//!
//! An `Intent` is a *request to act*, not an act. It is accepted, recorded, and
//! answered with `202 Accepted` before any device has been contacted; the
//! device is reached later by `sismatic-relay`. `CommandRecord` is how a caller
//! learns what happened afterwards.

use serde::{Deserialize, Serialize};

use crate::value::ReadingValue;
use crate::{DeviceId, FieldName, Timestamp};

/// A submitted command's identifier. A `String` for the same reason
/// [`DeviceId`] is: it travels as JSON and is opaque to every reader. The
/// server mints a v4 UUID.
pub type CommandId = String;

/// What a caller wants done.
///
/// Internally tagged (`{"kind": "set_metadata", "field": "TITLE", ...}`) rather
/// than adjacently tagged like [`ReadingValue`]: two variants carry no payload
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
    /// [`Phase`] is [`Phase::Idle`] — see `sismatic_store::outbox::admit`.
    SetMetadata {
        #[cfg_attr(feature = "openapi", schema(value_type = String, example = "TITLE"))]
        field: FieldName,
        value: String,
    },
    /// Write one device setting. Admissible in every phase.
    SetSetting {
        #[cfg_attr(feature = "openapi", schema(value_type = String, example = "TIMEZONE"))]
        field: FieldName,
        value: String,
    },
}

/// The write side's belief about whether a recording is in progress.
///
/// Distinct from [`RecordingState`](crate::value::RecordingState), which is what
/// a device *reported* at some past instant. `Phase` is what the outbox has
/// accepted and not yet seen fail, which is the thing an admission decision has
/// to be taken against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Recording,
    Paused,
}

/// Why a submission was refused.
///
/// Reaches a client inside the `409 Conflict` body's message, under the single
/// [`ErrorCode::Conflict`](crate::ErrorCode::Conflict). That is weaker than
/// branching on a code: [`ApiError`](crate::ApiError) carries one `code` and no
/// slot for a rejection, so telling `metadata_frozen` from `already_recording`
/// over the wire means matching prose. Closing that needs either a field on
/// `ApiError` or one `ErrorCode` per variant here, and both change the envelope
/// every client shares — so it is left stated rather than decided.
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

/// Where a submitted command has got to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CommandStatus {
    /// Recorded, no device contacted yet.
    Pending,
    /// A relay task has claimed it and the SIS exchange is running.
    InFlight,
    /// The device answered. `value` is the decoded echo — for a register write
    /// the device echoes the value it stored, which is worth returning.
    Succeeded { value: ReadingValue },
    /// Terminal failure after `attempts` tries.
    Failed { reason: String },
}

/// A device's write-side state, as `GET /v1/devices/{id}/recording` reports it.
/// A product type because the two fields are only meaningful together: an epoch
/// without a phase does not say whether metadata is writable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RecordingPhase {
    pub phase: Phase,
    /// Increments on each transition into [`Phase::Recording`] from
    /// [`Phase::Idle`].
    pub epoch: u64,
}

/// One row of the outbox as a caller sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CommandRecord {
    // Annotated for the same reason `device` is: a derive sees the alias name
    // it was written with, so an un-annotated `CommandId` makes utoipa invent a
    // component called `String` and every generated client grows a wrapper type
    // around a plain string. `the_string_aliases_are_documented_as_strings` in
    // `sismatic-http-api` is what notices when one of these is missing.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub id: CommandId,
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub device: DeviceId,
    pub intent: Intent,
    /// The recording epoch this command was admitted against.
    pub epoch: u64,
    pub status: CommandStatus,
    pub attempts: u32,
    pub enqueued_at: Timestamp,
    pub updated_at: Timestamp,
    // protects a command from quick exhaustion of retries
    pub not_before: Timestamp,
}

/// The `202 Accepted` body. `epoch` is returned so a caller writing several
/// metadata fields can check that all of them landed on one recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Accepted {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub id: CommandId,
    pub epoch: u64,
}

/// A page of commands, wrapped for the same reason
/// [`ReadingList`](crate::ReadingList) is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CommandList {
    pub commands: Vec<CommandRecord>,
}
