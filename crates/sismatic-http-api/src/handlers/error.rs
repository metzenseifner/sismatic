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
//! A local
//! type carrying the status code and *converting* into the shared body (`ApiError`) keeps the
//! framework on this side of the seam and the contract on the other.

use actix_web::http::StatusCode;
use actix_web::{HttpResponse, ResponseError};
use sismatic_api_types::{ApiError, ErrorCode};
use sismatic_store::ReadError;
use sismatic_store::outbox::SubmitError;

use crate::config::ConfigRefusal;

/// A failed read-side request.
#[derive(Debug)]
pub enum ApiFailure {
    /// Nothing is stored for what was asked for. Note what this is *not*: a
    /// claim that the device or field does not exist. The store holds what the
    /// sync side wrote, so an unknown device, an unpolled field and a device
    /// that has simply not answered yet are indistinguishable from here — all
    /// three are "no read", and saying so is the honest answer (see
    /// [`ReadStore::latest`](sismatic_store::ReadStore::latest)).
    ///
    /// A caller that has to tell them apart asks a different question rather
    /// than reading more into this one: `GET /v1/inventory/devices/{id}` says
    /// whether the device is configured, and `GET /v1/reads` says whether the
    /// field is a name this server knows — see
    /// [`crate::handlers::instructions`]. Neither is consulted here, because
    /// answering "no read" is not a claim that needs them.
    NotFound(String),
    /// The request contradicted itself and no read was attempted.
    BadRequest(String),
    /// The storage backend failed. Ours, not the caller's.
    Store(ReadError),
    /// A submission the outbox refused, or the backend failure that stopped it
    /// being recorded.
    ///
    /// Carried whole rather than split into a message and a status here,
    /// because `sismatic-store` already knows which half is the caller's fault:
    /// [`SubmitError`]'s `From` impl for [`ApiError`] is the one place that
    /// decision is made, and this variant defers to it for both the body and —
    /// via the code it chose — the status.
    Submit(SubmitError),
    /// A change to the settings that was not made.
    ///
    /// Carried whole for the reason [`Submit`](Self::Submit) is: the port has
    /// already classified it, into three cases that are three different people's
    /// problems, and re-deciding here is how the two could come to disagree.
    Config(ConfigRefusal),
}

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiFailure::NotFound(msg) | ApiFailure::BadRequest(msg) => f.write_str(msg),
            ApiFailure::Store(e) => write!(f, "{e}"),
            // Rendered through the same conversion the body uses, so the
            // message a log line carries is the message the caller received.
            ApiFailure::Submit(e) => f.write_str(&ApiError::from(e.clone()).error),
            ApiFailure::Config(e) => write!(f, "{e}"),
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

/// The write-side counterpart, so a `writes` handler bubbles a refused
/// submission with `?` exactly as a reads handler bubbles a store failure.
impl From<SubmitError> for ApiFailure {
    fn from(e: SubmitError) -> Self {
        ApiFailure::Submit(e)
    }
}

/// The same convenience for the config scope's two fallible routes.
impl From<ConfigRefusal> for ApiFailure {
    fn from(e: ConfigRefusal) -> Self {
        ApiFailure::Config(e)
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
            // Every bad request the *routes* can produce is about a field name,
            // which is what `BadInstruction` classifies. The config scope's are
            // about values rather than names and carry `BadRequest` instead —
            // see the arm below, and `ErrorCode` for why the older, narrower
            // code stayed as it is.
            ApiFailure::BadRequest(_) => ErrorCode::BadInstruction,
            ApiFailure::Store(_) => ErrorCode::Internal,
            // The two variants that do not pick their own code: the port they
            // came from already classified them, and re-deciding here is how the
            // two could disagree.
            ApiFailure::Submit(e) => return ApiError::from(e.clone()),
            ApiFailure::Config(e) => {
                let code = match e {
                    ConfigRefusal::Malformed(_) => ErrorCode::BadRequest,
                    ConfigRefusal::Fixed(_) => ErrorCode::Conflict,
                    ConfigRefusal::Source(_) => ErrorCode::Internal,
                };
                return ApiError::coded(code, e.to_string());
            }
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
            // Derived from the code `store` chose rather than matched on the
            // variant a second time. A rejection is a `409` because it is a
            // conflict with the device's state; a backend failure is ours.
            ApiFailure::Submit(_) => match self.body().code {
                Some(ErrorCode::Conflict) => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
            // Three cases, three people. A value we could not read is the
            // caller's; a setting that needs a restart is a conflict with the
            // running process rather than a mistake; a config file that will not
            // load is the deployment's, and there is nothing the caller could
            // have sent instead — which is what a 500 says.
            ApiFailure::Config(e) => match e {
                ConfigRefusal::Malformed(_) => StatusCode::BAD_REQUEST,
                ConfigRefusal::Fixed(_) => StatusCode::CONFLICT,
                ConfigRefusal::Source(_) => StatusCode::INTERNAL_SERVER_ERROR,
            },
        }
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code()).json(self.body())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{DesiredRecordingState, Rejection};

    /// The pairing the two `match`es above could get out of step. A rejection
    /// is the caller's fault and a backend failure is ours, and the status has
    /// to say which.
    #[test]
    fn a_refused_submission_is_a_conflict_and_a_backend_failure_is_ours() {
        let refused = ApiFailure::Submit(SubmitError::Rejected {
            device: "atrium-101".to_owned(),
            rejection: Rejection::MetadataFrozen,
            desired_recording_state: DesiredRecordingState::Recording,
        });
        assert_eq!(refused.status_code(), StatusCode::CONFLICT);
        assert_eq!(refused.body().code, Some(ErrorCode::Conflict));
        // The typed field is what a client branches on; the prose is what a
        // log reader reads. Both are asserted, because the two could drift.
        assert_eq!(refused.body().rejection, Some(Rejection::MetadataFrozen));
        assert!(
            refused.to_string().contains("metadata_frozen")
                && refused.to_string().contains("atrium-101"),
            "got: {refused}"
        );

        let broken = ApiFailure::Submit(SubmitError::Backend("disk full".into()));
        assert_eq!(broken.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(broken.body().code, Some(ErrorCode::Internal));
        // Not a rejection, so the field is absent — and therefore not even
        // serialized. A client that sees `rejection` knows it was refused by a
        // precondition and not by a broken disk.
        assert_eq!(broken.body().rejection, None);
    }

    /// Every rejection reaches a caller as a 409 — none of the four is a
    /// server-side fault, and one landing on a 500 would tell an operator to
    /// look at the wrong side.
    #[test]
    fn every_rejection_is_a_conflict() {
        for rejection in [
            Rejection::MetadataFrozen,
            Rejection::AlreadyRecording,
            Rejection::AlreadyPaused,
            Rejection::NotRecording,
        ] {
            let failure = ApiFailure::Submit(SubmitError::Rejected {
                device: "atrium-101".to_owned(),
                rejection,
                desired_recording_state: DesiredRecordingState::Idle,
            });
            assert_eq!(
                failure.status_code(),
                StatusCode::CONFLICT,
                "{rejection:?} did not read as a conflict"
            );
            // ...and reaches the caller as itself, not flattened into the code.
            assert_eq!(failure.body().rejection, Some(rejection));
        }
    }

    /// The config scope's three refusals reach a caller as three different
    /// statuses, which is the whole reason the port does not return one string.
    /// A client scripting a ConfigMap reload branches on exactly this: 400 means
    /// fix the file, 409 means roll the deployment, 500 means the file is not
    /// readable at all.
    #[test]
    fn a_config_refusal_carries_the_status_its_case_calls_for() {
        let malformed = ApiFailure::Config(ConfigRefusal::Malformed(
            "'5 fortnights' is not a duration".into(),
        ));
        assert_eq!(malformed.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(malformed.body().code, Some(ErrorCode::BadRequest));

        let fixed = ApiFailure::Config(ConfigRefusal::Fixed("http.port is 8080".into()));
        assert_eq!(fixed.status_code(), StatusCode::CONFLICT);
        assert_eq!(fixed.body().code, Some(ErrorCode::Conflict));
        // Not a write rejection, so the typed field a client branches on for
        // *those* stays absent — the two 409s are told apart by its presence.
        assert_eq!(fixed.body().rejection, None);

        let source = ApiFailure::Config(ConfigRefusal::Source("no such file".into()));
        assert_eq!(source.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(source.body().code, Some(ErrorCode::Internal));

        // The message reaches the caller intact in every case: it is the only
        // thing that says which setting or which text was the problem.
        assert!(malformed.body().error.contains("fortnights"));
    }
}
