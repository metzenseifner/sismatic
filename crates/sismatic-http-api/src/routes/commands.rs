//! The write routes: five ways to spell an [`Intent`], one way to submit it.
//!
//! ```text
//! POST /devices/{id}/recording/start      begin a recording
//! POST /devices/{id}/recording/stop       end one
//! POST /devices/{id}/recording/pause      suspend one
//! PUT  /devices/{id}/metadata/{field}     write a metadata register
//! PUT  /devices/{id}/settings/{field}     write a device setting
//!
//! GET  /devices/{id}/recording            the write side's phase and epoch
//! GET  /devices/{id}/commands             what this device has been asked
//! GET  /commands/{id}                     what became of one request
//! ```
//!
//! # Nothing here reaches a device
//!
//! Each write handler records an intent and answers `202 Accepted`;
//! `sismatic-intent-relay` contacts the device afterwards. The `202` is
//! therefore accurate rather than optimistic — the request *has* been accepted
//! and nothing *has* been attempted — and it is what lets this crate keep the
//! property its module docs claim: a handler that called `Device::run` would
//! need an `Instruction`, which lives in `sismatic-core`, which no front-end
//! may have a compile path to. The intent is a value built from `String`s, so
//! the seam holds.
//!
//! Every `202` carries a `Location: /v1/commands/{id}`, which is the route that
//! answers what happened.
//!
//! # Why five routes rather than one `POST /commands`
//!
//! One endpoint taking a polymorphic body would be fewer lines here and worse
//! everywhere else. The classification of metadata-versus-setting would move
//! from the URL into the body, so an access log would read `POST .../commands`
//! for a title edit and for a recording start alike; the generated OpenAPI
//! would have one operation with a `oneOf` body instead of five named ones; and
//! coarse authorization by path prefix — the cheapest way to let one credential
//! start recordings and another edit metadata — would stop being possible. The
//! `.wrap(YourAuthMiddleware::default())` placeholder in [`crate::startup`] is
//! what makes that last point concrete rather than hypothetical.
//!
//! The multiplication is in the URL space only: all five funnel into one
//! private `submit`, so there is one path from an intent to a response.
//!
//! # `{field}` is a parameter, as on the read side
//!
//! A field added to core's `Register` or `Setting` catalog is writable here
//! with no code change in this crate, for the same reason and with the same
//! consequence as on the read routes — see [`crate::routes::readings`] for the
//! full argument. The cost is the same too: this crate cannot tell a misspelled
//! field from a real one, so an unknown name is refused by
//! `sismatic-intent-relay` at dispatch and surfaces as a `failed` command
//! rather than as a `400`.

use actix_web::{HttpResponse, web};
use serde::Deserialize;
// `ApiError` is named only by the `#[utoipa::path]` response attributes — the
// handlers return `ApiFailure` and let it render.
use sismatic_api_types::{
    Accepted, ApiError, CommandList, CommandRecord, DeviceId, Intent, RecordingPhase,
};
use sismatic_store::outbox::{CommandLog, CommandSubmit, Submission};

use crate::routes::error::ApiFailure;
use crate::routes::readings::normalize_field;
use crate::stamp::Stamp;

/// The body of a metadata or setting write.
///
/// A single-field object rather than a bare JSON string, so the body is
/// self-describing and can gain a field later without becoming a different
/// media type. `{"value": "Week 4"}` also survives a value that is a number or
/// a boolean in the caller's language — every SIS write is text on the channel,
/// and the quoting says so.
/// Derived unconditionally, unlike the DTOs in `sismatic-api-types`: those are
/// shared with clients that have no use for utoipa, so their derive is behind a
/// feature. This body is this crate's own and this crate always builds the
/// document.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ValueWrite {
    #[schema(example = "Week 4 — Lecture")]
    pub value: String,
}

/// The optional `Idempotency-Key` header.
///
/// An extractor rather than an `HttpRequest` read inside each handler, so the
/// header appears in each handler's signature — which is where a reader looks
/// to find out what a route consumes.
///
/// What it buys: a client whose `POST /recording/start` times out and is
/// retried gets the original `202` back instead of a `409 already_recording`
/// for a command it believes never landed. Scope is per device, so two
/// recorders may be started under one key.
pub struct IdempotencyKey(pub Option<String>);

impl actix_web::FromRequest for IdempotencyKey {
    type Error = actix_web::Error;
    type Future = std::future::Ready<Result<Self, Self::Error>>;

    /// Infallible: a missing or non-ASCII header is *no key*, not a bad
    /// request. Refusing the request would turn an unreadable optimisation into
    /// an outage.
    fn from_request(req: &actix_web::HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        let key = req
            .headers()
            .get("Idempotency-Key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        std::future::ready(Ok(IdempotencyKey(key)))
    }
}

/// The one path from an [`Intent`] to a response. Every route below is this
/// function plus an `Intent` constructor.
async fn submit(
    port: &dyn CommandSubmit,
    stamp: &Stamp,
    device: DeviceId,
    intent: Intent,
    idempotency_key: Option<String>,
) -> Result<HttpResponse, ApiFailure> {
    let (id, at) = stamp.next();
    let accepted: Accepted = port
        .submit(Submission {
            id,
            device,
            intent,
            at,
            idempotency_key,
        })
        .await?;
    Ok(HttpResponse::Accepted()
        // The id is in the body as well, but a `Location` is what makes the
        // `202` self-describing to a client that follows headers rather than
        // parsing one more body shape.
        .insert_header(("Location", format!("/v1/commands/{}", accepted.id)))
        .json(accepted))
}

#[utoipa::path(
    post,
    path = "/devices/{id}/recording/start",
    context_path = "/v1",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device id, as configured on the write side."),
        ("Idempotency-Key" = Option<String>, Header,
         description = "Repeat this on a retry and the original command is returned \
             rather than a second recording started."),
    ),
    responses(
        (status = 202, description = "Recorded. No device has been contacted yet; \
             follow the `Location` header for the outcome.", body = Accepted),
        (status = 409, description = "This device is already recording.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn start_recording(
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<String>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    submit(
        &**port,
        &stamp,
        path.into_inner(),
        Intent::StartRecording,
        key.0,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/devices/{id}/recording/stop",
    context_path = "/v1",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device id."),
        ("Idempotency-Key" = Option<String>, Header, description = "See the start route."),
    ),
    responses(
        (status = 202, body = Accepted),
        (status = 409, description = "No recording is in progress.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn stop_recording(
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<String>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    submit(
        &**port,
        &stamp,
        path.into_inner(),
        Intent::StopRecording,
        key.0,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/devices/{id}/recording/pause",
    context_path = "/v1",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device id."),
        ("Idempotency-Key" = Option<String>, Header, description = "See the start route."),
    ),
    responses(
        (status = 202, body = Accepted),
        (status = 409, description = "Nothing is recording, or it is already paused.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn pause_recording(
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<String>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    submit(
        &**port,
        &stamp,
        path.into_inner(),
        Intent::PauseRecording,
        key.0,
    )
    .await
}

#[utoipa::path(
    put,
    path = "/devices/{id}/metadata/{field}",
    context_path = "/v1",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device id."),
        ("field" = String, Path, example = "TITLE",
         description = "Metadata register name. Case-insensitive, and `-` is read as \
             `_`, as on the read routes."),
        ("Idempotency-Key" = Option<String>, Header, description = "See the start route."),
    ),
    request_body = ValueWrite,
    responses(
        (status = 202, body = Accepted),
        (status = 409, description = "A recording is in progress, so this device's \
             metadata is sealed for the current epoch. Stop the recording, or write \
             the field before the next one starts.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn set_metadata(
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<(String, String)>,
    body: web::Json<ValueWrite>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    let (device, field) = path.into_inner();
    let intent = Intent::SetMetadata {
        // The same normalization the read routes apply, so `metadata/title`,
        // `metadata/TITLE` and `metadata/Title` name one register — and so the
        // name that reaches the catalog is the canonical spelling it lists.
        field: normalize_field(&field),
        value: body.into_inner().value,
    };
    submit(&**port, &stamp, device, intent, key.0).await
}

#[utoipa::path(
    put,
    path = "/devices/{id}/settings/{field}",
    context_path = "/v1",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device id."),
        ("field" = String, Path, example = "TIMEZONE",
         description = "Device setting name, normalized as above."),
        ("Idempotency-Key" = Option<String>, Header, description = "See the start route."),
    ),
    request_body = ValueWrite,
    responses(
        (status = 202, description = "Recorded. Settings carry no recording freeze, so \
             this is accepted in every phase.", body = Accepted),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn set_setting(
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<(String, String)>,
    body: web::Json<ValueWrite>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    let (device, field) = path.into_inner();
    let intent = Intent::SetSetting {
        field: normalize_field(&field),
        value: body.into_inner().value,
    };
    submit(&**port, &stamp, device, intent, key.0).await
}

/// `GET /commands/{id}` — what became of one submitted command.
///
/// Unversioned in its device-independence but not in its path: a command id is
/// globally unique, so it needs no device to address it.
#[utoipa::path(
    get,
    path = "/commands/{id}",
    context_path = "/v1",
    tag = "commands",
    params(("id" = String, Path, description = "Command id, as returned by a 202.")),
    responses(
        (status = 200, description = "The command and its current status.",
         body = CommandRecord),
        (status = 404, description = "No command has this id. Unlike the readings \
             routes' 404, this one *is* a claim about existence: an id is only ever \
             minted by an accepted submission.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn read_command(
    log: web::Data<dyn CommandLog>,
    path: web::Path<String>,
) -> Result<web::Json<CommandRecord>, ApiFailure> {
    let id = path.into_inner();
    log.command(id.clone())
        .await?
        .map(web::Json)
        .ok_or_else(|| ApiFailure::NotFound(format!("no command '{id}'")))
}

/// `GET /devices/{id}/recording` — the write side's phase and epoch.
///
/// What the *outbox* believes, which is not the same as what the device last
/// reported: the phase moves the moment a start is accepted, before any device
/// has been contacted. `GET /devices/{id}/fields/RUNNING_STATE` is the other
/// question — what the device said, and when.
#[utoipa::path(
    get,
    path = "/devices/{id}/recording",
    context_path = "/v1",
    tag = "commands",
    params(("id" = String, Path, description = "Device id.")),
    responses(
        (status = 200, description = "The phase the write side has accepted, and the \
             epoch of the current or next recording. An unknown device reports `idle` \
             at epoch 0 — the outbox holds what was submitted and no catalog of what \
             exists.", body = RecordingPhase),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn read_phase(
    log: web::Data<dyn CommandLog>,
    path: web::Path<String>,
) -> Result<web::Json<RecordingPhase>, ApiFailure> {
    Ok(web::Json(log.phase(path.into_inner()).await?))
}

/// `GET /devices/{id}/commands` — everything this device has been asked to do,
/// newest first.
#[utoipa::path(
    get,
    path = "/devices/{id}/commands",
    context_path = "/v1",
    tag = "commands",
    params(("id" = String, Path, description = "Device id.")),
    responses(
        (status = 200, description = "Every command recorded for this device, newest \
             first. An unknown device yields an empty list.", body = CommandList),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn list_commands(
    log: web::Data<dyn CommandLog>,
    path: web::Path<String>,
) -> Result<web::Json<CommandList>, ApiFailure> {
    let commands = log.commands_for(path.into_inner()).await?;
    Ok(web::Json(CommandList { commands }))
}
