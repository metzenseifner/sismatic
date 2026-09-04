//! `GET /health_check` — is this process alive and serving?

use actix_web::HttpResponse;

/// `GET /health_check` — is this process alive and serving?
///
/// Answers `200 OK` with an empty body, and the status *is* the payload. A
/// probe — a load balancer deciding whether to keep sending traffic here, a
/// supervisor deciding whether to restart the process, a deployment script
/// waiting for the new version to come up — reads a status code, and a body
/// would only add a shape to parse and a schema to version for a question whose
/// answer is one bit.
///
/// # Liveness, not readiness
///
/// The signature is the contract: no extractors, so no state, so nothing this
/// handler can consult and nothing it can be slow because of. It answers exactly
/// one question — *did this process reach the point of accepting and routing a
/// request?* — and that question is answerable by having answered it.
///
/// The store is deliberately not consulted, even though [`run`] does put it
/// within reach. A health check that read from the store would report a store
/// outage as *this process is dead*, and the supervisor watching this endpoint
/// would answer a dependency's failure by killing a process that is working
/// perfectly — turning one fault into two. Which devices are reachable, and how
/// stale their last reads are, are real questions and belong on real routes
/// over the store, where a caller can ask about a device rather than about the
/// server.
///
/// [`run`]: crate::run
#[utoipa::path(
    get,
    path = "/health_check",
    tag = "health",
    responses(
        (status = 200, description = "The process is accepting and routing requests. \
             The body is empty; the status is the whole answer."),
    ),
)]
pub async fn health_check() -> HttpResponse {
    HttpResponse::Ok().finish()
}
