//! The OpenAPI document, and the browsable UI served over it.
//!
//! ```text
//! GET /api-docs/openapi.json    the document itself
//! GET /swagger-ui/              Swagger UI, reading that document
//! ```
//!
//! [`ApiDoc`] is a description of this crate's routes assembled at *compile
//! time*: [`utoipa::OpenApi`] collects the `#[utoipa::path]` attribute on each
//! handler, and each schema those attributes name is derived from the very DTO
//! the handler serializes. So the document is not a second description of the
//! API that has to be kept in step with the first — rename a field on
//! [`Reading`] and the schema renames with it, because both come off one
//! `#[derive]` (see `sismatic-api-types`' `openapi` feature).
//!
//! # What is still hand-written, and how it is held down
//!
//! One thing does not come from the code: the path strings. `#[utoipa::path(path
//! = "/devices/{id}/fields")]` is a literal, and so is the `web::resource(...)`
//! it is meant to describe over in [`crate::startup`] — the two are written
//! twice and nothing in the type system pairs them. A route moved in one place
//! and not the other is exactly the failure a generated document is supposed to
//! rule out, so it is ruled out by a test instead: `tests/openapi.rs` reads the
//! served document, requests every path it advertises, and fails if the server
//! does not route one. See that file for how a route-miss is told apart from a
//! handler's own 404.
//!
//! `context_path = "/v1"` carries the [`web::scope`] the readings routes are
//! mounted under; the same test is what keeps that honest too.
//!
//! # Why the UI is embedded rather than linked
//!
//! `utoipa-swagger-ui`'s `vendored` feature compiles the Swagger UI assets into
//! the binary. That costs a few hundred kilobytes and buys three things: the
//! docs work on a network with no route to a CDN (which is the normal case for
//! the installations this serves), there is no static-asset deployment step that
//! can be skipped or version-skewed, and the UI a server shows is the UI that
//! server was built with. It also keeps the build itself offline — see the
//! dependency's comment in the workspace manifest.
//!
//! [`Reading`]: sismatic_api_types::Reading
//! [`web::scope`]: actix_web::web::scope

use utoipa::OpenApi;
use utoipa::openapi::{Info, License};

/// Where the document is served, and the URL the UI is pointed at.
pub const OPENAPI_JSON_PATH: &str = "/api-docs/openapi.json";

/// Where Swagger UI is served. The `{_:.*}` tail is actix's spelling for "and
/// everything under it": the UI is a directory of assets, not one page, so the
/// resource has to match `/swagger-ui/swagger-ui.css` as well as `/swagger-ui/`.
pub const SWAGGER_UI_PATH: &str = "/swagger-ui/{_:.*}";

/// The read side's OpenAPI description.
///
/// `components(schemas(..))` lists only the types a response body names at top
/// level; utoipa walks into them for the rest, so [`ReadingValue`] and its
/// payloads arrive by being reachable from [`Reading`] rather than by being
/// enumerated here and drifting when one is added.
///
/// [`ReadingValue`]: sismatic_api_types::ReadingValue
/// [`Reading`]: sismatic_api_types::Reading
#[derive(OpenApi)]
#[openapi(
    paths(
        crate::routes::health_check::health_check,
        crate::routes::readings::list_fields,
        crate::routes::readings::read_field,
        crate::routes::readings::field_history,
    ),
    components(schemas(
        sismatic_api_types::Reading,
        sismatic_api_types::ReadingList,
        sismatic_api_types::ApiError,
    )),
    tags(
        (name = "readings", description =
            "Stored readings. Every queryable field of every device is reachable \
             through these three routes, because the field is a path parameter \
             passed through to the store rather than a symbol the server was \
             compiled against — a field added to the device catalog is served here \
             with no code change."),
        (name = "health", description =
            "Liveness. Consults nothing, so it reports on this process and never on \
             its dependencies."),
    ),
)]
pub struct ApiDoc;

impl ApiDoc {
    /// The document, with the fields utoipa cannot infer filled in.
    ///
    /// Title, version and license are taken from the crate's own metadata rather
    /// than written out, so the version in the document is the version that was
    /// built and a release bump cannot leave a stale number here. The derive
    /// already defaults to `CARGO_PKG_*`; what this adds is the license and a
    /// description worth reading on the UI's landing page.
    pub fn document() -> utoipa::openapi::OpenApi {
        let mut doc = <Self as OpenApi>::openapi();
        doc.info = Info::builder()
            .title("Sismatic read API")
            .version(env!("CARGO_PKG_VERSION"))
            .description(Some(
                "The read (query) side of Sismatic. A device is polled by the sync \
                 driver, which writes what it reads to the store; these routes answer \
                 questions about what was written. Nothing here reaches a device, so \
                 no response is ever waiting on one — a value's `at` timestamp is how \
                 fresh it is.",
            ))
            .license(Some(License::new(env!("CARGO_PKG_LICENSE"))))
            .build();
        doc
    }
}
