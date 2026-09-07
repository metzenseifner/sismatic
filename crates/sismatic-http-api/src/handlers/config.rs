//! The config routes: what this server is running under, and how to change it.
//!
//! ```text
//! GET   /v1/config           every setting as it stands
//! PATCH /v1/config           change some of them
//! POST  /v1/config/reload    read the config file again and apply it
//! ```
//!
//! # The one scope that changes the server rather than a device
//!
//! Every other route here is about a recorder: what one reported, what one was
//! asked to do. These three are about the process answering the request — how
//! often it polls, how much history it keeps, how fast it drains its write
//! queue. Nothing they change reaches a device directly, and everything they
//! change alters what the rest of this API will do next.
//!
//! That is worth stating plainly because of what this API does *not* have. There
//! is no authentication in front of these routes — see the `TODO` on the reads
//! scope in [`crate::startup`] — so on a network where anything can reach the
//! port, anything can re-time the fleet's polling. Until that middleware exists,
//! a deployment that exposes this server beyond its own subnet should keep
//! `/v1/config` behind whatever fronts it.
//!
//! # Why a reload route exists beside the patch route
//!
//! They are for two different operators. A patch is imperative and immediate:
//! one value, one request, taking effect before the response is written — which
//! is what an operator chasing a device wants at three in the morning.
//!
//! A reload is declarative. In Kubernetes the settings live in a ConfigMap
//! mounted as a file, kubelet rewrites that file within a minute or so of the
//! ConfigMap changing, and *nothing tells the process*. `POST /v1/config/reload`
//! is what a sidecar or a `kubectl exec` calls at that point, and it is
//! deliberately not a second way to describe the change: it re-runs the very
//! load the process ran at startup — file, then environment, then the flags the
//! command line carried — so the file stays the single source of truth and the
//! running settings converge on it. A deployment that patches and then reloads
//! gets the file's answer back, which is the correct behaviour for a
//! GitOps-managed config and the reason a patch is not persisted anywhere.
//!
//! # What neither of them can do
//!
//! The listen socket and the devices file are fixed until the process restarts,
//! and a request that would change either is refused whole rather than applied
//! in part — see [`sismatic_api_types::config`] for the whole argument, and
//! [`crate::config::ConfigRefusal`] for how that refusal is spelled. In
//! Kubernetes those are exactly the changes that come with a rolling restart
//! anyway.

use actix_web::{HttpResponse, web};
// `ApiError` is named only by the `#[utoipa::path]` response attributes — the
// handlers return `ApiFailure` and let it render.
use sismatic_api_types::{ApiError, ConfigDocument, ConfigPatch};

use crate::config::LiveConfig;
use crate::handlers::error::ApiFailure;

/// `GET /v1/config` — every setting this server is running under.
///
/// The body is a complete statement rather than a diff against the built-in
/// defaults: a setting nobody has ever named appears here at the value it
/// resolved to, so the answer to "what is this process actually doing" needs no
/// second document beside it. It is also a valid `PATCH` body, which is what
/// makes read-modify-write safe to script.
#[utoipa::path(
    get,
    path = "",
    context_path = "/v1/config",
    tag = "config",
    responses(
        (status = 200, description = "Every setting, as it stands now. \
             `sync` and `store` and `intent_relay` are editable through \
             `PATCH /v1/config`; `http` and `devices_config_path` are reported \
             and fixed until the process restarts.", body = ConfigDocument),
    ),
)]
pub async fn read_config(config: web::Data<dyn LiveConfig>) -> HttpResponse {
    HttpResponse::Ok().json(config.current().await)
}

/// `PATCH /v1/config` — change some of the settings, leaving the rest alone.
///
/// Applied whole or not at all, and applied *live*: by the time the response is
/// written, the poll loops are on the new schedule and the sweeper on the new
/// window. The body that comes back is every setting as it now stands, so a
/// caller never has to re-read to see what its own request produced.
#[utoipa::path(
    patch,
    path = "",
    context_path = "/v1/config",
    tag = "config",
    request_body = ConfigPatch,
    responses(
        (status = 200, description = "Applied. The body is every setting as it \
             now stands, not just the ones that moved.", body = ConfigDocument),
        (status = 400, description = "A value could not be read — a duration \
             that does not parse, a size with an unknown unit, a schedule naming \
             a field that is not in `GET /v1/reads`. Nothing was applied, and \
             the message names the offending text.", body = ApiError),
        (status = 409, description = "The patch would change a setting that is \
             fixed until the process restarts (`http`, `devices_config_path`). \
             Naming one at the value it already has is not a change and is \
             accepted; changing it needs a restart. Nothing was applied.",
         body = ApiError),
    ),
)]
pub async fn patch_config(
    config: web::Data<dyn LiveConfig>,
    patch: web::Json<ConfigPatch>,
) -> Result<HttpResponse, ApiFailure> {
    let applied = config.apply(patch.into_inner()).await?;
    Ok(HttpResponse::Ok().json(applied))
}

/// `POST /v1/config/reload` — read the config file again and apply what it says.
///
/// The route a mounted ConfigMap is picked up by. It takes no body: the file —
/// with the environment and the command line layered over it exactly as at
/// startup — is the whole of what it reads, so there is nothing for a caller to
/// state and no way for a request to disagree with the file it is asking the
/// server to honour.
#[utoipa::path(
    post,
    path = "/reload",
    context_path = "/v1/config",
    tag = "config",
    responses(
        (status = 200, description = "Reloaded. The body is every setting as it \
             now stands. A file identical to what is running produces this too — \
             the route is idempotent, which is what makes it safe for a watcher \
             to call on every write to the mounted volume.", body = ConfigDocument),
        (status = 400, description = "The file parsed but a value could not be \
             applied. Nothing was reloaded.", body = ApiError),
        (status = 409, description = "The file's `http` or `devices_config_path` \
             no longer matches what this process started with. Those need a \
             restart, so nothing was reloaded — roll the deployment rather than \
             reloading it.", body = ApiError),
        (status = 500, description = "The config file could not be read or \
             parsed. Nothing was reloaded and the server keeps running under the \
             settings it had.", body = ApiError),
    ),
)]
pub async fn reload_config(config: web::Data<dyn LiveConfig>) -> Result<HttpResponse, ApiFailure> {
    let reloaded = config.reload().await?;
    Ok(HttpResponse::Ok().json(reloaded))
}
