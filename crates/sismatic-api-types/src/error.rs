//! The error envelope and service health, shared by server and client so both
//! agree on the failure shape.

use serde::{Deserialize, Serialize};

/// A machine-readable classification of a failed request, letting a client
/// branch on the *kind* of error without string-matching the message. Mirrors
/// the variants the current web backend maps onto status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No device or group has the requested id (HTTP 404).
    UnknownDevice,
    /// The instruction name is not in the catalog (HTTP 400).
    BadInstruction,
    /// The device was reached but the exchange failed (HTTP 502).
    DeviceError,
    /// A generic not-found (e.g. no readings for the given span).
    NotFound,
    /// An unexpected server-side failure (HTTP 500).
    Internal,
}

/// The body every failed request returns: a human `error` message, plus an
/// optional machine `code`. Serializes as `{ "error": "..." }` when `code` is
/// absent, staying compatible with the current backend's error shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ApiError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
}

impl ApiError {
    /// A message-only error (no machine code), matching the legacy shape.
    pub fn message(msg: impl Into<String>) -> Self {
        Self {
            error: msg.into(),
            code: None,
        }
    }

    /// A classified error carrying both a message and a machine code.
    pub fn coded(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self {
            error: msg.into(),
            code: Some(code),
        }
    }
}

/// Liveness of the read-side service, returned by `GET /health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ServiceStatus {
    Ok,
    Degraded,
}

/// The `GET /health` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Health {
    pub status: ServiceStatus,
}
