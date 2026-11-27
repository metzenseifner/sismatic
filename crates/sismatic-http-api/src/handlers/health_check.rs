//! `GET /health_check` — is this process alive and serving?

use actix_web::HttpResponse;

/// `GET /health_check` — is this process alive and serving?
///
/// `200 OK` with an empty body. Liveness only: it consults no state, the store
/// included, so a store outage does not read as a dead process.
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
