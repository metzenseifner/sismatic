//! The error envelope and service health, shared by server and client so both
//! agree on the failure shape.

use serde::{Deserialize, Serialize};

use crate::write::Rejection;

/// A machine-readable classification of a failed request, letting a client
/// branch on the *kind* of error without string-matching the message. Mirrors
/// the variants the current web backend maps onto status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No device or group has the requested id (HTTP 404).
    UnknownDevice,
    /// The instruction name is not in the catalog (HTTP 400).
    BadInstruction,
    /// The device was reached but the exchange failed (HTTP 502).
    DeviceError,
    /// A generic not-found (e.g. no reads for the given span).
    NotFound,
    /// The request contradicts the device's current write-side state, most
    /// often a metadata write during a recording (HTTP 409).
    ///
    /// One code for all four [`Rejection`]s rather than one each, because they
    /// share a status and differ only in which precondition refused. *Which*
    /// one is carried by [`ApiError::rejection`] as a typed field beside this
    /// code — see that struct for why the two are separate fields.
    Conflict,
    /// An unexpected server-side failure (HTTP 500).
    Internal,
}

/// The body every failed request returns: a human `error` message, plus an
/// optional machine `code`. Serializes as `{ "error": "..." }` when `code` is
/// absent, staying compatible with the current backend's error shape.
///
/// # Why there is a second machine-readable field
///
/// [`code`](ApiError::code) classifies the *kind* of failure, and each kind has
/// its own status: `unknown_device` is a 404, `bad_instruction` a 400,
/// `device_error` a 502. [`rejection`](ApiError::rejection) says which
/// precondition refused a write, and every one of those is a 409. They are two
/// axes, and folding the second into `ErrorCode` would have made that enum a
/// union of two taxonomies — four of its variants sharing one status, with
/// nothing in the type to say which combine with which. It would also grow the
/// envelope enum every client shares each time an admission rule is added.
///
/// A fixed core plus typed optional members is the ordinary shape for this;
/// it is what RFC 7807 problem-details calls an extension member. Both optional
/// fields are skipped when absent, so an error that has neither serializes
/// exactly as it always did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ApiError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
    /// Which precondition refused a write. Present only alongside
    /// [`ErrorCode::Conflict`], and the reason a client can tell "your metadata
    /// edit was discarded" from "the device is already doing what you asked" —
    /// which call for very different handling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection: Option<Rejection>,
}

impl ApiError {
    /// A message-only error (no machine code), matching the legacy shape.
    pub fn message(msg: impl Into<String>) -> Self {
        Self {
            error: msg.into(),
            code: None,
            rejection: None,
        }
    }

    /// A classified error carrying both a message and a machine code.
    pub fn coded(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self {
            error: msg.into(),
            code: Some(code),
            rejection: None,
        }
    }

    /// A refused write: [`ErrorCode::Conflict`] plus the precondition that
    /// refused it.
    ///
    /// The code is not a parameter. Every rejection is a conflict, and letting
    /// a caller pair a rejection with some other code is the one way these two
    /// fields could come to disagree.
    pub fn rejected(rejection: Rejection, msg: impl Into<String>) -> Self {
        Self {
            error: msg.into(),
            code: Some(ErrorCode::Conflict),
            rejection: Some(rejection),
        }
    }
}

/// Liveness of the read-side service, returned by `GET /health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ServiceStatus {
    Ok,
    Degraded,
}

/// The `GET /health` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Health {
    pub status: ServiceStatus,
}
