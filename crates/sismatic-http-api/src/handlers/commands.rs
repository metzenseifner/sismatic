//! The write routes: five ways to spell an [`Intent`], one way to submit it —
//! addressed to a device or to a device group.
//!
//! Every path below is relative to the `/v1/commands` scope [`crate::startup`]
//! mounts them under.
//!
//! ```text
//! POST /devices/{id}/recording/start      begin a recording
//! POST /devices/{id}/recording/stop       end one
//! POST /devices/{id}/recording/pause      suspend one
//! PUT  /devices/{id}/metadata/{field}     write a metadata register
//! PUT  /devices/{id}/settings/{field}     write a device setting
//!
//! POST /groups/{id}/recording/start       the same five, addressed to a
//! POST /groups/{id}/recording/stop        device group
//! POST /groups/{id}/recording/pause
//! PUT  /groups/{id}/metadata/{field}
//! PUT  /groups/{id}/settings/{field}
//!
//! GET  /devices/{id}/recording            the write side's phase and epoch
//! GET  /devices/{id}/commands             what this device has been asked
//! GET  /groups/{id}/recording             every member's phase, and the one
//!                                         they agree on
//! GET  /groups/{id}/commands              what each member has been asked
//! GET  /{id}                              what became of one request
//! ```
//!
//! # One namespace, two spaces
//!
//! Devices and groups share one id namespace, and each URL space accepts only
//! its own kind. Every route here resolves its id through
//! [`target`](crate::handlers::target) first, so a group id under `/v1/commands/devices`
//! and a device id under `/v1/commands/groups` are both a `404` naming the URL that
//! would have worked.
//!
//! The `/groups` write routes are not a second code path for that: all ten
//! funnel into one private [`submit`] over an already-resolved target list, so
//! there is still one path from an intent to a response, and the kind check is
//! the only thing that differs between them.
//!
//! The two status reads are why the split is a refusal rather than a fan-out.
//! The outbox keys its logs by *device*, so a group id on
//! `GET /v1/commands/devices/{id}/recording` took a default and reported `idle` at epoch
//! `0`, and on `GET /v1/commands/devices/{id}/commands` an empty list — for a device
//! group whose members were mid-recording with a queue each. Those were not
//! answers about the group; they were answers about a device that does not
//! exist, and no wording of the documentation made them safe.
//! [`read_group_phase`] and [`list_group_commands`] ask the port the question
//! it can actually answer, once per member.
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
//! The multiplication is in the URL space only: all ten funnel into one
//! private [`submit`], so there is one path from an intent to a response.
//!
//! # `{field}` is a parameter, as on the read side
//!
//! A field added to core's `Register` or `Setting` catalog is writable here
//! with no code change in this crate, for the same reason and with the same
//! consequence as on the read routes — see [`crate::handlers::readings`] for the
//! full argument. The cost is the same too: this crate cannot tell a misspelled
//! field from a real one, so an unknown name is refused by
//! `sismatic-intent-relay` at dispatch and surfaces as a `failed` command
//! rather than as a `400`.

use std::time::Duration;

use actix_web::{HttpResponse, web};
use serde::Deserialize;
// `ApiError` is named only by the `#[utoipa::path]` response attributes — the
// handlers return `ApiFailure` and let it render.
use sismatic_api_types::{
    Acceptance, ApiError, CommandList, CommandRecord, DeviceId, GroupCommandList, GroupPhase,
    GroupSummary, Intent, MemberCommands, MemberPhase, RecordingPhase,
};
use sismatic_store::catalog::DeviceCatalog;
use sismatic_store::outbox::{BarrierPolicy, CommandLog, CommandSubmit, Submission};

use crate::handlers::error::ApiFailure;
use crate::handlers::readings::normalize_field;
use crate::handlers::target::{COMMANDS, group_members, reject_group};
use crate::stamp::Stamp;

/// The body of a metadata or setting write.
///
/// A single-field object rather than a bare JSON string, so the body is
/// self-describing and can gain a field later without becoming a different
/// media type. `{"value": "Week 4"}` also survives a value that is a number or
/// a boolean in the caller's language — every SIS write is text on the channel,
/// and the quoting says so.
///
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

/// Whether this intent needs the members of a group to act *together*.
///
/// The three lifecycle verbs do: a device group whose members start seconds
/// apart has produced takes that do not line up, which is the whole reason a
/// group is addressable as one thing. The two writes do not — setting the same
/// title on two devices is the same result whenever each one happens — so
/// batching
/// them would buy nothing and cost them a barrier they could time out against.
///
/// Wildcard-free, so a new [`Intent`] variant is a build error here until
/// someone decides which kind it is. That decision is not one a default should
/// make: guessing "no rendezvous" would silently ship a lifecycle verb that
/// stopped keeping a device group in step.
const fn needs_rendezvous(intent: &Intent) -> bool {
    match intent {
        Intent::StartRecording | Intent::StopRecording | Intent::PauseRecording => true,
        Intent::SetMetadata { .. } | Intent::SetSetting { .. } => false,
    }
}

/// The one path from an [`Intent`] to a response. Every route below is this
/// function plus an `Intent` constructor.
///
/// `target` is a device id or a group id — the two share one namespace, and
/// which it is decides only how many rows the submission expands into.
async fn submit(
    port: &dyn CommandSubmit,
    stamp: &Stamp,
    targets: Vec<DeviceId>,
    group: Option<GroupSummary>,
    intent: Intent,
    idempotency_key: Option<String>,
) -> Result<HttpResponse, ApiFailure> {
    // `targets` is already expanded and `group` already resolved, by whichever
    // entry point enforced its own URL space. Expansion happens *there* rather
    // than at dispatch because admission is per device: a device group where one
    // member is already running and one is idle has to be decided against both
    // phases, and only a submission naming both can be.
    let batch = group
        .as_ref()
        .filter(|_| needs_rendezvous(&intent) && targets.len() > 1)
        .map(|_| stamp.next().0);
    let barrier = group.as_ref().map(|group| BarrierPolicy {
        timeout: Duration::from_secs(group.barrier_timeout_secs),
        on_timeout: group.barrier,
    });

    let mut ids = Vec::with_capacity(targets.len());
    let mut at = None;
    for _ in &targets {
        let (id, minted_at) = stamp.next();
        ids.push(id);
        // One instant for the whole submission, not one per row: the members
        // were accepted together, and a barrier armed at the *first* id's
        // instant is the one an operator's stopwatch agrees with.
        at.get_or_insert(minted_at);
    }
    let at = at.expect("members is non-empty");

    let accepted: Acceptance = port
        .submit(Submission {
            ids,
            targets,
            // Only when the target *was* a group. A device-addressed request
            // that happens to name a member of one has not asked anything of
            // the device group, and filing an expectation against it would
            // report the other members as drifted for doing nothing wrong.
            group: group.as_ref().map(|group| group.id.clone()),
            batch,
            barrier,
            intent,
            at,
            idempotency_key,
        })
        .await?;

    let mut response = HttpResponse::Accepted();
    // Only when there is one command to point at. A group produced several, and
    // a header naming an arbitrary one of them would be worse than none — the
    // body carries every id.
    if let [only] = accepted.commands.as_slice() {
        response.insert_header(("Location", format!("/v1/commands/{}", only.id)));
    }
    Ok(response.json(accepted))
}

/// [`submit`] from a `/v1/commands/devices` route: the id must name a device, and one
/// command is recorded.
///
/// A group id is refused here rather than fanned out. It once *was* fanned out,
/// because `/v1/commands/devices/{id}` was the only way to address a device group, and
/// the two status routes below are why that stopped being tenable: they read an
/// outbox keyed by device, so a group id there reports an idle device that does
/// not exist. See [`crate::handlers::target`].
///
/// An id that names nothing is a `404` and not a `202`. The outbox holds what
/// was submitted and no list of what exists, so without this check it would
/// admit a command for a mistyped id against a fresh idle phase — a promise
/// that cannot be kept, which the caller discovers only by polling a command
/// that fails at dispatch, minutes later.
async fn submit_device(
    catalog: &dyn DeviceCatalog,
    port: &dyn CommandSubmit,
    stamp: &Stamp,
    device: DeviceId,
    group_route: &str,
    intent: Intent,
    idempotency_key: Option<String>,
) -> Result<HttpResponse, ApiFailure> {
    reject_group(catalog, &device, COMMANDS, group_route).await?;
    if catalog.device(&device).await.is_none() {
        return Err(ApiFailure::NotFound(format!(
            "no device '{device}' is configured"
        )));
    }

    // No group, so no rendezvous and no barrier: one device acting alone is
    // already in unison with itself.
    submit(port, stamp, vec![device], None, intent, idempotency_key).await
}

/// [`submit`] from a `/v1/commands/groups` route: the id must name a device group, and
/// one command per member is recorded.
///
/// The check is the whole difference between the two URL spaces. Without it
/// `/v1/commands/groups/atrium-101/recording/start` would start a single recorder and
/// answer `202`, which is a `/groups` route quietly doing a `/devices` route's
/// job — and the caller would have no way to notice, because the `202` body
/// looks the same either way.
///
/// `device_route` is the tail of the `/v1/commands/devices` route that does the same
/// thing, so a caller who reached for the wrong space is told which URL it
/// wanted rather than only that this one was wrong.
async fn submit_group(
    catalog: &dyn DeviceCatalog,
    port: &dyn CommandSubmit,
    stamp: &Stamp,
    group: DeviceId,
    device_route: &str,
    intent: Intent,
    idempotency_key: Option<String>,
) -> Result<HttpResponse, ApiFailure> {
    let targets = group_members(catalog, &group, COMMANDS, device_route).await?;
    // Resolved rather than re-derived: `group_members` already established that
    // this id names one, so the summary is present.
    let summary = catalog.group(&group).await;
    submit(port, stamp, targets, summary, intent, idempotency_key).await
}

#[utoipa::path(
    post,
    path = "/devices/{id}/recording/start",
    context_path = "/v1/commands",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device id, as configured on the write side."),
        ("Idempotency-Key" = Option<String>, Header,
         description = "Repeat this on a retry and the original command is returned \
             rather than a second recording started."),
    ),
    responses(
        (status = 202, description = "Recorded. No device has been contacted yet; \
             follow the `Location` header for the outcome.", body = Acceptance),
        (status = 409, description = "This device is already recording.", body = ApiError),
        (status = 404, description = "No device has this id, or the id names a \
             device group — which is refused here and carries the `/v1/commands/groups` URL \
             that does the same thing. The catalog is the configured set, so both \
             are claims about the devices file.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn start_recording(
    catalog: web::Data<dyn DeviceCatalog>,
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<String>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    submit_device(
        &**catalog,
        &**port,
        &stamp,
        path.into_inner(),
        "recording/start",
        Intent::StartRecording,
        key.0,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/devices/{id}/recording/stop",
    context_path = "/v1/commands",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device id."),
        ("Idempotency-Key" = Option<String>, Header, description = "See the start route."),
    ),
    responses(
        (status = 202, body = Acceptance),
        (status = 409, description = "No recording is in progress.", body = ApiError),
        (status = 404, description = "No device has this id, or the id names a \
             device group — which is refused here and carries the `/v1/commands/groups` URL \
             that does the same thing. The catalog is the configured set, so both \
             are claims about the devices file.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn stop_recording(
    catalog: web::Data<dyn DeviceCatalog>,
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<String>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    submit_device(
        &**catalog,
        &**port,
        &stamp,
        path.into_inner(),
        "recording/stop",
        Intent::StopRecording,
        key.0,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/devices/{id}/recording/pause",
    context_path = "/v1/commands",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device id."),
        ("Idempotency-Key" = Option<String>, Header, description = "See the start route."),
    ),
    responses(
        (status = 202, body = Acceptance),
        (status = 409, description = "Nothing is recording, or it is already paused.",
         body = ApiError),
        (status = 404, description = "No device has this id, or the id names a \
             device group — which is refused here and carries the `/v1/commands/groups` URL \
             that does the same thing. The catalog is the configured set, so both \
             are claims about the devices file.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn pause_recording(
    catalog: web::Data<dyn DeviceCatalog>,
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<String>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    submit_device(
        &**catalog,
        &**port,
        &stamp,
        path.into_inner(),
        "recording/pause",
        Intent::PauseRecording,
        key.0,
    )
    .await
}

#[utoipa::path(
    put,
    path = "/devices/{id}/metadata/{field}",
    context_path = "/v1/commands",
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
        (status = 202, body = Acceptance),
        (status = 409, description = "A recording is in progress, so this device's \
             metadata is sealed for the current epoch. Stop the recording, or write \
             the field before the next one starts.", body = ApiError),
        (status = 404, description = "No device has this id, or the id names a \
             device group — which is refused here and carries the `/v1/commands/groups` URL \
             that does the same thing. The catalog is the configured set, so both \
             are claims about the devices file.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn set_metadata(
    catalog: web::Data<dyn DeviceCatalog>,
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
    let group_route = format!("metadata/{field}");
    submit_device(
        &**catalog,
        &**port,
        &stamp,
        device,
        &group_route,
        intent,
        key.0,
    )
    .await
}

#[utoipa::path(
    put,
    path = "/devices/{id}/settings/{field}",
    context_path = "/v1/commands",
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
             this is accepted in every phase.", body = Acceptance),
        (status = 404, description = "No device has this id, or the id names a \
             device group — which is refused here and carries the `/v1/commands/groups` URL \
             that does the same thing. The catalog is the configured set, so both \
             are claims about the devices file.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn set_setting(
    catalog: web::Data<dyn DeviceCatalog>,
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
    let group_route = format!("settings/{field}");
    submit_device(
        &**catalog,
        &**port,
        &stamp,
        device,
        &group_route,
        intent,
        key.0,
    )
    .await
}

/// `GET /v1/commands/{id}` — what became of one submitted command.
///
/// The one route in this scope addressed by neither a device nor a group, and
/// so the one mounted directly on the scope root: a command id is globally
/// unique, so it needs no device to address it. That is also why it is
/// registered *last* — `/{id}` is one segment, `/devices/…` and `/groups/…` are
/// two or more, so it can only ever catch what the others did not.
#[utoipa::path(
    get,
    path = "/{id}",
    context_path = "/v1/commands",
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
/// has been contacted. `GET /v1/readings/devices/{id}/fields/RUNNING_STATE` is the other
/// question — what the device said, and when.
#[utoipa::path(
    get,
    path = "/devices/{id}/recording",
    context_path = "/v1/commands",
    tag = "commands",
    params(("id" = String, Path, description = "Device id.")),
    responses(
        (status = 200, description = "The phase the write side has accepted, and the \
             epoch of the current or next recording. An unknown device reports `idle` \
             at epoch 0 — the outbox holds what was submitted and no catalog of what \
             exists.", body = RecordingPhase),
        (status = 404, description = "This id names a device group. The outbox keys \
             its logs by device, so this route would report `idle` at epoch 0 for a \
             group whose members are recording; `/v1/commands/groups/{id}/recording` is the \
             answer.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn read_phase(
    catalog: web::Data<dyn DeviceCatalog>,
    log: web::Data<dyn CommandLog>,
    path: web::Path<String>,
) -> Result<web::Json<RecordingPhase>, ApiFailure> {
    let device = path.into_inner();
    // The route that made the refusal necessary. The outbox has no log under a
    // group id, so without this it answers `idle` at epoch 0 for a device group
    // whose members are recording — see `crate::handlers::target`.
    reject_group(&**catalog, &device, COMMANDS, "recording").await?;
    Ok(web::Json(log.phase(device).await?))
}

/// `GET /devices/{id}/commands` — everything this device has been asked to do,
/// newest first.
#[utoipa::path(
    get,
    path = "/devices/{id}/commands",
    context_path = "/v1/commands",
    tag = "commands",
    params(("id" = String, Path, description = "Device id.")),
    responses(
        (status = 200, description = "Every command recorded for this device, newest \
             first. An unknown device yields an empty list.", body = CommandList),
        (status = 404, description = "This id names a device group, whose members each \
             hold their own queue; `/v1/commands/groups/{id}/commands` is the answer.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn list_commands(
    catalog: web::Data<dyn DeviceCatalog>,
    log: web::Data<dyn CommandLog>,
    path: web::Path<String>,
) -> Result<web::Json<CommandList>, ApiFailure> {
    let device = path.into_inner();
    // As on the phase route: a group id has no log, so the honest answer is a
    // redirection rather than an empty list.
    reject_group(&**catalog, &device, COMMANDS, "commands").await?;
    let commands = log.commands_for(device).await?;
    Ok(web::Json(CommandList { commands }))
}

// ---- the same five verbs, addressed to a device group ---------------------

#[utoipa::path(
    post,
    path = "/groups/{id}/recording/start",
    context_path = "/v1/commands",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device group id, as written in the devices file."),
        ("Idempotency-Key" = Option<String>, Header,
         description = "Repeat this on a retry and the original commands are returned \
             rather than a second recording started. Scoped to the group's first \
             member, so one key answers for the whole submission."),
    ),
    responses(
        (status = 202, description = "Recorded, one command per member. The three \
             lifecycle verbs are expanded under a rendezvous, so `batch` is set and no \
             member is dispatched until every one is ready.", body = Acceptance),
        (status = 409, description = "One member refused, so the whole submission was \
             refused and nothing was recorded — admission is across every member at \
             once. The body names the member and its phase.", body = ApiError),
        (status = 404, description = "No device group has this id. A device id here is \
             this same 404, with the `/v1/commands/devices` URL that would have worked.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn start_group_recording(
    catalog: web::Data<dyn DeviceCatalog>,
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<String>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    submit_group(
        &**catalog,
        &**port,
        &stamp,
        path.into_inner(),
        "recording/start",
        Intent::StartRecording,
        key.0,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/groups/{id}/recording/stop",
    context_path = "/v1/commands",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device group id."),
        ("Idempotency-Key" = Option<String>, Header, description = "See the start route."),
    ),
    responses(
        (status = 202, body = Acceptance),
        (status = 409, description = "A member has no recording in progress, so the \
             whole submission was refused.", body = ApiError),
        (status = 404, description = "No device group has this id.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn stop_group_recording(
    catalog: web::Data<dyn DeviceCatalog>,
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<String>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    submit_group(
        &**catalog,
        &**port,
        &stamp,
        path.into_inner(),
        "recording/stop",
        Intent::StopRecording,
        key.0,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/groups/{id}/recording/pause",
    context_path = "/v1/commands",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device group id."),
        ("Idempotency-Key" = Option<String>, Header, description = "See the start route."),
    ),
    responses(
        (status = 202, body = Acceptance),
        (status = 409, description = "A member is not recording, or is already paused.",
         body = ApiError),
        (status = 404, description = "No device group has this id.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn pause_group_recording(
    catalog: web::Data<dyn DeviceCatalog>,
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<String>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    submit_group(
        &**catalog,
        &**port,
        &stamp,
        path.into_inner(),
        "recording/pause",
        Intent::PauseRecording,
        key.0,
    )
    .await
}

#[utoipa::path(
    put,
    path = "/groups/{id}/metadata/{field}",
    context_path = "/v1/commands",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device group id."),
        ("field" = String, Path, example = "TITLE",
         description = "Metadata register name, normalized as on the read routes."),
        ("Idempotency-Key" = Option<String>, Header, description = "See the start route."),
    ),
    request_body = ValueWrite,
    responses(
        (status = 202, description = "Recorded, one command per member. Expanded \
             *without* a rendezvous: writing the same title to two recorders is the \
             same result whenever each one happens, so `batch` is null and no member \
             waits on a barrier it has no use for.", body = Acceptance),
        (status = 409, description = "A member has a recording in progress, so its \
             metadata is sealed — and the whole submission is refused rather than \
             leaving the group half-written.", body = ApiError),
        (status = 404, description = "No device group has this id.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn set_group_metadata(
    catalog: web::Data<dyn DeviceCatalog>,
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<(String, String)>,
    body: web::Json<ValueWrite>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    let (group, field) = path.into_inner();
    let intent = Intent::SetMetadata {
        field: normalize_field(&field),
        value: body.into_inner().value,
    };
    let device_route = format!("metadata/{field}");
    submit_group(
        &**catalog,
        &**port,
        &stamp,
        group,
        &device_route,
        intent,
        key.0,
    )
    .await
}

#[utoipa::path(
    put,
    path = "/groups/{id}/settings/{field}",
    context_path = "/v1/commands",
    tag = "commands",
    params(
        ("id" = String, Path, description = "Device group id."),
        ("field" = String, Path, example = "TIMEZONE",
         description = "Device setting name, normalized as on the read routes."),
        ("Idempotency-Key" = Option<String>, Header, description = "See the start route."),
    ),
    request_body = ValueWrite,
    responses(
        (status = 202, description = "Recorded, one command per member. Settings carry \
             no recording freeze and no rendezvous, so this is accepted in every \
             phase.", body = Acceptance),
        (status = 404, description = "No device group has this id.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn set_group_setting(
    catalog: web::Data<dyn DeviceCatalog>,
    port: web::Data<dyn CommandSubmit>,
    stamp: web::Data<Stamp>,
    path: web::Path<(String, String)>,
    body: web::Json<ValueWrite>,
    key: IdempotencyKey,
) -> Result<HttpResponse, ApiFailure> {
    let (group, field) = path.into_inner();
    let intent = Intent::SetSetting {
        field: normalize_field(&field),
        value: body.into_inner().value,
    };
    let device_route = format!("settings/{field}");
    submit_group(
        &**catalog,
        &**port,
        &stamp,
        group,
        &device_route,
        intent,
        key.0,
    )
    .await
}

/// `GET /groups/{id}/recording` — every member's phase, and the one they agree
/// on.
///
/// Not an alias of the device route with a group id, and cannot be: the outbox
/// keys its logs by device, so that route answers `idle` at epoch `0` for a
/// group id — a claim about a device that does not exist. This one asks the
/// port once per member and reports what it actually said.
///
/// `phase` is `null` when the members are not all in the same one, which is a
/// finding rather than a missing value: a group whose members have diverged is
/// one where a start reached some of them. Epochs are never rolled up — see
/// [`GroupPhase::members`].
#[utoipa::path(
    get,
    path = "/groups/{id}/recording",
    context_path = "/v1/commands",
    tag = "commands",
    params(("id" = String, Path, description = "Device group id.")),
    responses(
        (status = 200, description = "Every member's accepted phase and epoch, plus \
             the phase they agree on — `null` when they do not. This is what the \
             *outbox* accepted, which moves before any device is contacted; the \
             group's `RUNNING_STATE` field is what the members reported.",
         body = GroupPhase),
        (status = 404, description = "No device group has this id.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn read_group_phase(
    catalog: web::Data<dyn DeviceCatalog>,
    log: web::Data<dyn CommandLog>,
    path: web::Path<String>,
) -> Result<web::Json<GroupPhase>, ApiFailure> {
    let group = path.into_inner();
    let member_ids = group_members(&**catalog, &group, COMMANDS, "recording").await?;

    let mut members = Vec::with_capacity(member_ids.len());
    for device in member_ids {
        let recording = log.phase(device.clone()).await?;
        members.push(MemberPhase {
            device,
            phase: recording.phase,
            epoch: recording.epoch,
        });
    }

    // `None` for an empty group as well as for a divided one: there is no phase
    // every member is in when there is no member.
    let phase = members
        .first()
        .map(|first| first.phase)
        .filter(|phase| members.iter().all(|m| m.phase == *phase));

    Ok(web::Json(GroupPhase {
        group,
        phase,
        members,
    }))
}

/// `GET /groups/{id}/commands` — what each member has been asked to do, newest
/// first within each member.
///
/// Partitioned rather than merged, for the reason the device route is a flat
/// list and this one is not: two submissions can share an instant, so a merged
/// list would have no total order to present them in. A row's `batch` is what
/// ties one group-addressed request back together across members.
#[utoipa::path(
    get,
    path = "/groups/{id}/commands",
    context_path = "/v1/commands",
    tag = "commands",
    params(("id" = String, Path, description = "Device group id.")),
    responses(
        (status = 200, description = "One command list per member, newest first, in \
             configured order. A member that has been asked nothing carries an empty \
             list rather than being omitted.", body = GroupCommandList),
        (status = 404, description = "No device group has this id.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn list_group_commands(
    catalog: web::Data<dyn DeviceCatalog>,
    log: web::Data<dyn CommandLog>,
    path: web::Path<String>,
) -> Result<web::Json<GroupCommandList>, ApiFailure> {
    let group = path.into_inner();
    let member_ids = group_members(&**catalog, &group, COMMANDS, "commands").await?;

    let mut members = Vec::with_capacity(member_ids.len());
    for device in member_ids {
        let commands = log.commands_for(device.clone()).await?;
        members.push(MemberCommands { device, commands });
    }

    Ok(web::Json(GroupCommandList { group, members }))
}
