//! How a handler's failure becomes a response.
//!
//! [`ApiFailure`] is the one error type the read side's handlers return, so
//! every failed request leaves this crate as the same JSON envelope
//! ([`ApiError`]) with a status code that matches its [`ErrorCode`]. Handlers
//! never build an error response themselves; they return a variant and let
//! actix's [`ResponseError`] machinery render it.
//!
//! # Why a local type rather than `ApiError` itself
//!
//! `ApiError` lives in `sismatic-api-types`, which depends on `serde` and
//! nothing else — deliberately, since every client links it and none of them
//! should acquire a web framework by doing so. `ResponseError` is an
//! `actix-web` trait, so implementing it on `ApiError` would either require that
//! dependency in the contract crate or run into the orphan rule here. A local
//! type carrying the status code and *converting* into the shared body keeps the
//! framework on this side of the seam and the contract on the other.

use actix_web::http::StatusCode;
use actix_web::{HttpResponse, ResponseError};
use sismatic_api_types::{ApiError, ErrorCode};
use sismatic_store::ReadError;

/// A failed read-side request.
#[derive(Debug)]
pub enum ApiFailure {
    /// Nothing is stored for what was asked for. Note what this is *not*: a
    /// claim that the device or field does not exist. The store holds what the
    /// sync side wrote, so an unknown device, an unpolled field and a device
    /// that has simply not answered yet are indistinguishable from here — all
    /// three are "no reading", and saying so is the honest answer (see
    /// [`ReadStore::latest`](sismatic_store::ReadStore::latest)).
    NotFound(String),
    /// The request contradicted itself and no reading was attempted.
    BadRequest(String),
    /// The storage backend failed. Ours, not the caller's.
    Store(ReadError),
}

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiFailure::NotFound(msg) | ApiFailure::BadRequest(msg) => f.write_str(msg),
            ApiFailure::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ApiFailure {}

/// Lets a handler bubble a store failure with `?` rather than mapping it at
/// every call site — the same convenience `store::error` already provides for
/// the `ReadError -> ApiError` half.
impl From<ReadError> for ApiFailure {
    fn from(e: ReadError) -> Self {
        ApiFailure::Store(e)
    }
}

impl ApiFailure {
    /// The wire body for this failure.
    ///
    /// Kept beside [`status_code`](Self::status_code) so the pairing of code and
    /// status is readable as one table: a variant cannot acquire a 404 status
    /// and a `bad_instruction` code by being edited in two places.
    fn body(&self) -> ApiError {
        let code = match self {
            ApiFailure::NotFound(_) => ErrorCode::NotFound,
            // The only bad request this crate can produce is about a field name,
            // which is what `BadInstruction` classifies. A general-purpose
            // "malformed request" code would be more accurate if there were more
            // than one such case; adding one to the shared contract for a single
            // caller would not be.
            ApiFailure::BadRequest(_) => ErrorCode::BadInstruction,
            ApiFailure::Store(_) => ErrorCode::Internal,
        };
        ApiError::coded(code, self.to_string())
    }
}

impl ResponseError for ApiFailure {
    fn status_code(&self) -> StatusCode {
        match self {
            ApiFailure::NotFound(_) => StatusCode::NOT_FOUND,
            ApiFailure::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiFailure::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code()).json(self.body())
    }
}
