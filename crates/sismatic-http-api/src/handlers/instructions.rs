//! The two exploratory routes: which names the rest of the API accepts.
//!
//! ```text
//! GET /v1/readings      every field a reading can be asked for
//! GET /v1/writings      every command, metadata register and setting a write
//!                       can name
//! ```
//!
//! One route on each scope root, answering the question a caller has *before* it
//! can build any other URL in that scope. Both are static: the lists are fixed
//! when the binary is built, so neither consults a store, a catalog or a device.
//!
//! # Why these exist at all
//!
//! Every other route in these two scopes takes the name as a path parameter and
//! passes it through — `{field}` is data here, not a symbol this crate was
//! compiled against, which is what lets a name added to `sismatic-core`'s catalog
//! be served with no code change in any crate (see [`crate::handlers::readings`]
//! for the whole argument). The price is stated there too, and it is what these
//! routes pay off: a URL naming a field that does not exist is indistinguishable
//! from one naming a field nothing has polled, so the API could not previously
//! tell a caller it had made a typo. It still cannot, per-request — but it can
//! now hand over the list, which is the same information delivered before the
//! mistake instead of after it.
//!
//! That also removes the last reason to read core's source to use the HTTP API.
//! `STREAM_NAME_1` is an accepted spelling of `STREAM_1_NAME` and no
//! normalization rule derives it; before this it was discoverable only in
//! `query.rs`.
//!
//! # Why the lists arrive as data
//!
//! This crate cannot see `sismatic-core` — the load-bearing rule of the workspace
//! layout — so it cannot read `Query::ALL` any more than it can read a
//! `Registry`. The composition root projects both into DTOs once at startup and
//! passes them in, exactly as it does for the [`DeviceCatalog`], and for the same
//! reason: the list crosses the seam as a value, not as a dependency edge.
//!
//! It is a plain value rather than a port because there is nothing to ask it.
//! The device catalog is a trait since an installation could one day resolve its
//! inventory from somewhere other than a file; an instruction catalog cannot, as
//! it is a table the compiler wrote. See [`crate::startup::Ports`].
//!
//! [`DeviceCatalog`]: sismatic_store::catalog::DeviceCatalog

use actix_web::{HttpResponse, web};
use sismatic_api_types::{FieldCatalog, WritingsCatalog};

/// `GET /v1/readings` — every field a reading can be asked for.
///
/// A field listed here is one this server knows how to *ask* a device for. It
/// is not a promise that anything has: whether a field is polled, and how often,
/// is the sync schedule's business, so a name can appear here and never appear
/// under `GET /v1/readings/devices/{id}/fields`.
#[utoipa::path(
    get,
    path = "",
    context_path = "/v1/readings",
    tag = "readings",
    responses(
        (status = 200, description = "Every queryable field, in catalog order, each \
             with the other spellings accepted for it and a line saying what it is. \
             The `name` is the canonical form, and the form a stored reading carries \
             whichever spelling was requested.", body = FieldCatalog),
    ),
)]
pub async fn field_catalog(fields: web::Data<FieldCatalog>) -> HttpResponse {
    // Serialized from the shared value rather than cloned into a `web::Json`,
    // which is the same choice `openapi::Docs` makes for the same reason: the
    // body cannot have changed since startup, so a per-request copy of a
    // hundred-odd strings would buy nothing.
    HttpResponse::Ok().json(&**fields)
}

/// `GET /v1/writings` — every command, metadata register, and setting a write can
/// name.
///
/// Three lists, because the three are written through three different routes:
/// `metadata` and `settings` are the `{field}` of their respective `PUT`s, while
/// `commands` are what the `recording/{verb}` routes send and are never spelled
/// in a URL. See [`WritingCatalog`] for the rules that separate them.
#[utoipa::path(
    get,
    path = "",
    context_path = "/v1/writings",
    tag = "writings",
    responses(
        (status = 200, description = "What a write can name, in three lists. \
             `metadata` names go in the `{field}` of \
             `PUT /v1/writings/devices/{id}/metadata/{field}` and are writable only \
             while nothing is recording; `settings` names go in the `{field}` of \
             `PUT /v1/writings/devices/{id}/settings/{field}` and are writable \
             always. `commands` are the recording instructions behind \
             `POST /v1/writings/devices/{id}/recording/start` and the two beside \
             it — reported for completeness, not to be put in a URL.",
         body = WritingsCatalog),
    ),
)]
pub async fn writings_catalog(writings: web::Data<WritingsCatalog>) -> HttpResponse {
    HttpResponse::Ok().json(&**writings)
}
