//! Store-port failures — the error half of the `ReadStore` / `WriteStore`
//! contract.
//!
//! These are **infrastructure** failures, not "nothing found". Absence is never
//! an error: `ReadStore::latest` returns `Ok(None)` and `ReadStore::between`
//! returns `Ok(vec![])` when there is simply nothing to read. `ReadError` and
//! `WriteError` are reserved for genuine failures of the storage backend, which
//! is why the in-memory adapter effectively never produces one and a database
//! adapter can (connection lost, query failed, disk full, ...).
//!
//! The convention here matches `sismatic-core`'s device errors: a hand-written
//! enum with a manual `Display`/`Error` impl (no `thiserror`). Each maps onto
//! the shared wire envelope ([`ApiError`]) so an http-api handler can turn a
//! store failure into a response with a single `?`.

use std::fmt;

use sismatic_api_types::{ApiError, ErrorCode, Phase};

use crate::outbox::SubmitError;

/// Why a read from the store failed.
///
/// A point read (`latest`) or a range read (`between`) can only fail because the
/// backend itself failed — an empty result is a success, not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The storage backend failed while serving the read (lost connection,
    /// query error, deserialization failure, ...). Carries an adapter-supplied
    /// description; the in-memory adapter never emits this, a DB adapter can.
    Backend(String),
}

impl ReadError {
    /// Convenience constructor from anything string-like, so an adapter writes
    /// `ReadError::backend(e)` at the point a lower-level error is caught.
    pub fn backend(msg: impl Into<String>) -> Self {
        ReadError::Backend(msg.into())
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Backend(e) => write!(f, "store read failed: {e}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// Why a write to the store failed.
///
/// Produced only by `WriteStore::upsert_latest`, which `sismatic-sync` calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// The storage backend failed while persisting the reading (lost
    /// connection, constraint violation, disk full, ...).
    Backend(String),
}

impl WriteError {
    /// Convenience constructor from anything string-like.
    pub fn backend(msg: impl Into<String>) -> Self {
        WriteError::Backend(msg.into())
    }
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Backend(e) => write!(f, "store write failed: {e}"),
        }
    }
}

impl std::error::Error for WriteError {}

/// Both store failures are server-side infrastructure faults, so they map onto
/// the wire envelope as [`ErrorCode::Internal`] (HTTP 500). The mapping lives
/// here — where `store` already meets `api-types` — so http-api handlers can
/// bubble a store failure up with `?` and never hand-roll the translation.
impl From<ReadError> for ApiError {
    fn from(e: ReadError) -> Self {
        ApiError::coded(ErrorCode::Internal, e.to_string())
    }
}

impl From<WriteError> for ApiError {
    fn from(e: WriteError) -> Self {
        ApiError::coded(ErrorCode::Internal, e.to_string())
    }
}

/// A submission failure is the one store error whose two halves are not both
/// ours. A rejection is the caller's problem — it asked for something the
/// device's write-side state does not allow — and a backend failure is ours, so
/// the two take different codes. The mapping lives here, where `store` already
/// meets `api-types`, for the same reason the two above do.
impl From<SubmitError> for ApiError {
    fn from(e: SubmitError) -> Self {
        match e {
            SubmitError::Rejected {
                device,
                rejection,
                phase,
            } => ApiError::rejected(
                rejection,
                // The prose stays, and stays complete, even though the
                // rejection is now a field: this is the string that lands in a
                // log, and a reader there has no other way to learn which
                // device refused. The device because a group submission is
                // refused as a whole by one member, and "which one" is the
                // first thing an operator needs; the phase because it is what
                // the caller would have to change — "not_recording" alone does
                // not say a start has to come first.
                format!(
                    "{rejection} (device '{device}', phase: {})",
                    phase_name(phase)
                ),
            ),
            SubmitError::Malformed(msg) => ApiError::coded(
                // The caller built a nonsensical submission, so this is a bug
                // in the server rather than in the request — no wording of the
                // request could have avoided it.
                ErrorCode::Internal,
                format!("malformed submission: {msg}"),
            ),
            SubmitError::Backend(msg) => ApiError::coded(ErrorCode::Internal, msg),
        }
    }
}

/// The wire spelling of a phase — the same `snake_case` its `Serialize` impl
/// produces, so the message and the `GET .../recording` body agree on what to
/// call a state.
const fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "idle",
        Phase::Recording => "recording",
        Phase::Paused => "paused",
    }
}
