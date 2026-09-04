//! The OpenAPI document, and the browsable UI served over it.
//!
//! ```text
//! GET /api-docs/openapi.json    the document itself
//! GET /scalar                   the Scalar API reference, reading that document
//! GET /scalar/scalar.js         the bundle that page loads
//! ```
//!
//! [`ApiDoc`] is a description of this crate's routes assembled at *compile
//! time*: [`utoipa::OpenApi`] collects the `#[utoipa::path]` attribute on each
//! handler, and each schema those attributes name is derived from the very DTO
//! the handler serializes. So the document is not a second description of the
//! API that has to be kept in step with the first — rename a field on
//! [`Read`] and the schema renames with it, because both come off one
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
//! `context_path` carries the two [`web::scope`]s a route is nested in —
//! `/v1/reads`, `/v1/writes` or `/v1/inventory` — which is a third literal
//! written twice, and the one a reader of the document depends on to build a
//! URL that works at all. The same test is what keeps it honest.
//!
//! # Why the UI is embedded rather than linked
//!
//! The whole reference ships in the binary: `scalar_api_reference` carries
//! `ui/scalar.js` inside its `.crate` file, and `build.rs` compresses it into
//! `OUT_DIR` for [`SCALAR_JS_GZ`] to include. That costs about a megabyte and
//! buys three things: the docs work on a network with no route to a CDN (which
//! is the normal case for the installations this serves), there is no
//! static-asset deployment step that can be skipped or version-skewed, and the
//! UI a server shows is the UI that server was built with. It also keeps the
//! build itself offline — see the dependency's comment in the workspace
//! manifest.
//!
//! Embedding the bundle is only half of it: a page can be served from the binary
//! and still reach for the network once it is running. So [`scalar_config`]
//! turns off the three things Scalar reaches for on its own — the webfonts, the
//! hosted request proxy and the hosted AI assistant — and a test asserts no
//! off-host name survives anywhere in the page. What renders offline is then the
//! whole page, not just its shell.
//!
//! # Why the bundle is stored compressed
//!
//! Minified JavaScript deflates well — 4.0 MB of it becomes 1.1 MB — and the
//! compressed form is the form a browser asks for anyway. Storing it that way
//! and handing it out under `Content-Encoding: gzip` therefore costs nothing at
//! all in the common case: no work at startup, no work per request, ~2.9 MB off
//! the binary and the same off every cold page load. [`scalar_js`] inflates only
//! for a client that explicitly refuses a compressed response, which is a case
//! that exists for correctness rather than because anything does it.
//!
//! The shell around it is [`SCALAR_HTML`] here rather than the template
//! `scalar_api_reference` ships, since that crate is build-only now — fifteen
//! lines, and it lets the page carry this API's name in the title bar instead of
//! the renderer's.
//!
//! # Rendered once, at startup
//!
//! [`Docs`] holds the three responses as [`Bytes`]: the document serialized, the
//! page with its configuration already substituted in, and the compressed bundle
//! borrowed straight from the binary's own image. The alternative is
//! re-serializing a document that cannot have changed on every request; `Bytes`
//! makes each response a refcount bump instead. It is built before the workers
//! are, so all of them share one copy — see [`crate::startup`].
//!
//! [`Read`]: sismatic_api_types::Read
//! [`web::scope`]: actix_web::web::scope

use actix_web::web::Bytes;
use actix_web::{HttpRequest, HttpResponse, web};
use serde_json::json;
use utoipa::OpenApi;
use utoipa::openapi::{Info, License};

/// Where the document is served, and the URL the UI is pointed at.
pub const OPENAPI_JSON_PATH: &str = "/api-docs/openapi.json";

/// Where the API reference is served. One page and one exact path — no tail
/// match, because the only asset it loads is the bundle below.
pub const SCALAR_UI_PATH: &str = "/api";

/// Where that page's bundle is served. Nested under [`SCALAR_UI_PATH`] so the
/// docs occupy one subtree of the URL space rather than two unrelated paths.
pub const SCALAR_JS_PATH: &str = "/scalar/scalar.js";

/// The Scalar bundle, gzipped by `build.rs`.
///
/// A `&'static [u8]` pointing into the binary's own image, so nothing is read,
/// allocated or copied to have it — [`Docs`] wraps it in a [`Bytes`] that
/// borrows rather than owns.
const SCALAR_JS_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/scalar.js.gz"));

/// The page that loads it.
///
/// `{js}` and `{config}` are filled in by [`Docs::render`]. Both substitutions
/// land inside markup, which normally raises the question of escaping; neither
/// value here is data, though — one is [`SCALAR_JS_PATH`] and the other is
/// [`scalar_config`], both fixed at compile time and neither reachable from a
/// request. If that ever stops being true, this template is where it stops.
const SCALAR_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <title>Sismatic API</title>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
  </head>
  <body>
    <div id="app"></div>
    <script src="{js}"></script>
    <script>
      Scalar.createApiReference('#app', {config})
    </script>
  </body>
</html>
"#;

/// The read side's OpenAPI description.
///
/// `components(schemas(..))` lists only the types a response body names at top
/// level; utoipa walks into them for the rest, so [`ReadValue`] and its
/// payloads arrive by being reachable from [`Read`] rather than by being
/// enumerated here and drifting when one is added.
///
/// [`ReadValue`]: sismatic_api_types::ReadValue
/// [`Read`]: sismatic_api_types::Read
#[derive(OpenApi)]
#[openapi(
    paths(
        crate::handlers::health_check::health_check,
        crate::handlers::instructions::field_catalog,
        crate::handlers::instructions::writes_catalog,
        crate::handlers::reads::list_fields,
        crate::handlers::reads::read_field,
        crate::handlers::reads::field_history,
        crate::handlers::writes::start_recording,
        crate::handlers::writes::stop_recording,
        crate::handlers::writes::pause_recording,
        crate::handlers::writes::set_metadata,
        crate::handlers::writes::set_setting,
        crate::handlers::writes::read_desired_recording_state,
        crate::handlers::writes::list_writes,
        crate::handlers::writes::read_write,
        crate::handlers::devices::list_devices,
        crate::handlers::devices::read_device,
        crate::handlers::devices::list_groups,
        crate::handlers::devices::read_group,
        crate::handlers::group_reads::list_group_fields,
        crate::handlers::group_reads::read_group_field,
        crate::handlers::group_reads::group_field_history,
        crate::handlers::writes::start_group_recording,
        crate::handlers::writes::stop_group_recording,
        crate::handlers::writes::pause_group_recording,
        crate::handlers::writes::set_group_metadata,
        crate::handlers::writes::set_group_setting,
        crate::handlers::writes::read_group_desired_recording_state,
        crate::handlers::writes::list_group_writes,
    ),
    components(schemas(
        sismatic_api_types::Read,
        sismatic_api_types::ReadList,
        sismatic_api_types::ApiError,
        // The write side's top-level bodies. `Intent`, `WriteStatus`,
        // `DesiredRecordingState`, `Rejection` and `Accepted` are reachable
        // from these and so arrive by being walked into, for the same reason
        // `ReadValue` does.
        sismatic_api_types::Acceptance,
        sismatic_api_types::WriteRecord,
        sismatic_api_types::WriteList,
        sismatic_api_types::DeviceDesiredRecordingState,
        crate::handlers::writes::ValueWrite,
        // The inventory bodies. `DeviceSummary` and `ConnectionStatus` arrive
        // by being reachable from these.
        sismatic_api_types::DeviceList,
        sismatic_api_types::DeviceDetail,
        sismatic_api_types::GroupList,
        sismatic_api_types::GroupSummary,
        // The group read bodies. `MemberState`, `MemberHistory`,
        // `GroupExpectation` and `SyncState` arrive by being reachable from
        // these, for the same reason `ReadValue` does.
        sismatic_api_types::GroupFieldState,
        sismatic_api_types::GroupFieldStateList,
        sismatic_api_types::GroupHistory,
        // The group write-side bodies. `MemberDesiredRecordingState` and `MemberWrites`
        // arrive by being reachable from these.
        sismatic_api_types::GroupDesiredRecordingState,
        sismatic_api_types::GroupWriteList,
        // The two scope-root catalogs. `InstructionSummary` arrives by being
        // reachable from both.
        sismatic_api_types::FieldCatalog,
        sismatic_api_types::WritesCatalog,
    )),
    tags(
        (name = "reads", description =
            "Stored reads, of one device or of a whole device group. Every \
             queryable field of every device is reachable through these six \
             routes, because the field is a path parameter passed through to the \
             store rather than a symbol the server was compiled against — a field \
             added to the device catalog is served here with no code change. \
             `/v1/reads` lists every name those six accept, which is the one \
             thing a path parameter cannot tell you.\n\n\
             The `/v1/reads/devices` half answers from the store alone, so an \
             unknown id there is `nothing stored` rather than a `404`. The \
             `/v1/reads/groups` half \
             also consults the catalog, because a device group has no reads of \
             its own and its membership has to come from somewhere — so an unknown \
             *group* is a `404`, and each response additionally carries what the \
             device group was last told to be, which is what makes a device group \
             that ignored a request detectable when its members agree perfectly \
             with each other."),
        (name = "inventory", description =
            "What this server was configured with. Answered from the device catalog \
             rather than the store, so an unknown id here is a `404` — a real claim \
             about the devices file — where the reads routes can only answer \
             `nothing stored`."),
        (name = "writes", description =
            "Any operation on the write path — asking a device to do something, or \
             setting something on it. Every write is recorded and answered \
             `202 Accepted` before any device is contacted, so no response here is \
             ever waiting on one — follow the `Location` header to learn what \
             happened. Metadata is writable only while nothing is recording; \
             settings are writable always, and `/v1/writes` lists which names \
             are which."),
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
            .title("Sismatic API")
            .version(env!("CARGO_PKG_VERSION"))
            .description(Some(
                "Sismatic's HTTP surface. Reads are answered from what the sync \
                 driver polled and stored; writes are recorded as intents and \
                 performed later by the intent relay. Nothing here reaches a device \
                 during a request, so no response is ever waiting on one — a \
                 read's `at` says how fresh it is, and a write's `202` says it \
                 was accepted rather than done.",
            ))
            .license(Some(License::new(env!("CARGO_PKG_LICENSE"))))
            .build();
        doc
    }
}

/// How the API reference is configured, as the JSON its bootstrap call takes.
///
/// One key points the page at this server. The other three are the page's
/// defaults turned off, and they are the difference between a UI that is
/// *served* offline and one that *works* offline — embedding the bundle only
/// settles where the page comes from, not where it goes once it is running.
///
/// * `url` points at [`OPENAPI_JSON_PATH`] — a *relative* URL, so the page
///   fetches the document from whatever host it was itself served from.
///   Absolute would pin the docs to one deployment's hostname and break the
///   moment the server is reached through a different one.
/// * `withDefaultFonts` is on by default and means "load Inter and JetBrains
///   Mono from `fonts.scalar.com`". Left alone, an offline install renders a
///   page that spends its first seconds waiting on font requests that will
///   never resolve.
/// * `proxyUrl` is the one that would matter even *with* a route to the
///   internet. Undefined, the bundle falls back to `proxy.scalar.com` and sends
///   every "Try it" request through it — including whatever credentials the
///   reader typed into the auth panel, for a server that by construction is
///   reachable only from inside the network the reader is already on. An empty
///   string is how the bundle is told to call the API directly.
/// * `agent` is Scalar's hosted AI assistant, which is otherwise available
///   without configuration when the page is served from localhost.
///
/// The last three are all "off" written three different ways, because that is
/// what the upstream schema accepts for each. `tests/openapi.rs` holds them
/// down from the outside: no off-host name anywhere in the rendered page, and
/// the two settings whose host is named inside the *bundle* rather than here
/// asserted present by spelling, since a scan for hostnames cannot see a key
/// that has gone missing.
fn scalar_config() -> serde_json::Value {
    json!({
        "url": OPENAPI_JSON_PATH,
        "withDefaultFonts": false,
        "proxyUrl": "",
        "agent": { "disabled": true },
    })
}

/// The three docs responses, rendered once and shared by every worker.
///
/// Registered with [`actix_web::App::app_data`] and taken by the handlers below
/// as [`web::Data<Docs>`], which is the same handle-sharing the store ports use:
/// one value, an `Arc` per worker, no per-request work.
pub struct Docs {
    /// The OpenAPI document, serialized.
    json: Bytes,
    /// The page, with [`scalar_config`] already substituted into its bootstrap
    /// call and its `<script src>` pointed at [`SCALAR_JS_PATH`].
    html: Bytes,
    /// The bundle that page loads, still gzipped.
    js_gz: Bytes,
}

impl Docs {
    /// Render all three from [`ApiDoc`].
    ///
    /// # Panics
    ///
    /// If the document cannot be serialized — a `ToSchema` derive that cannot
    /// produce JSON, which is a build-time fact wearing a runtime type. Failing
    /// here makes it a crash on the way up rather than a 500 the first time
    /// someone opens the docs, which is the difference between noticing at
    /// deploy time and noticing when a reader complains.
    ///
    /// The bundle cannot fail the same way: it is [`include_bytes!`]d, so a
    /// binary that lacks it is a binary that did not link.
    pub fn render() -> Self {
        let json = ApiDoc::document()
            .to_json()
            .expect("serializing the OpenAPI document");
        let html = SCALAR_HTML
            .replace("{js}", SCALAR_JS_PATH)
            .replace("{config}", &scalar_config().to_string());

        Self {
            json: Bytes::from(json),
            html: Bytes::from(html),
            // `from_static`, so this is a pointer into the binary's own image
            // rather than a megabyte on the heap — and every clone of it below
            // is a refcount bump on that same pointer.
            js_gz: Bytes::from_static(SCALAR_JS_GZ),
        }
    }
}

/// The document, as `application/json`.
pub async fn openapi_json(docs: web::Data<Docs>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(docs.json.clone())
}

/// The reference page, as `text/html`.
pub async fn scalar_ui(docs: web::Data<Docs>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(docs.html.clone())
}

/// The bundle the page loads, as `text/javascript` — gzipped if the client will
/// take it that way, which every browser will.
///
/// The stored form *is* the compressed form (see the module docs), so the common
/// path copies nothing and compresses nothing. Inflating is the exceptional
/// path, and it is done per request rather than once at startup on purpose: a
/// client that refuses `gzip` is rare enough that keeping a second four-megabyte
/// copy resident for its benefit would be paying, forever, for something that
/// may never be asked for.
pub async fn scalar_js(request: HttpRequest, docs: web::Data<Docs>) -> HttpResponse {
    if accepts_gzip(&request) {
        return HttpResponse::Ok()
            .content_type("text/javascript; charset=utf-8")
            .insert_header(("content-encoding", "gzip"))
            // Two clients that disagree about `Accept-Encoding` get different
            // bytes from this one URL, so anything caching in between has to
            // key on that header rather than on the URL alone.
            .insert_header(("vary", "accept-encoding"))
            .body(docs.js_gz.clone());
    }

    let mut js = Vec::new();
    match std::io::Read::read_to_end(
        &mut flate2::read::GzDecoder::new(docs.js_gz.as_ref()),
        &mut js,
    ) {
        Ok(_) => HttpResponse::Ok()
            .content_type("text/javascript; charset=utf-8")
            .insert_header(("vary", "accept-encoding"))
            .body(js),
        // Unreachable short of memory corruption — the input is a constant the
        // build script produced with the very library reading it back. Answered
        // rather than panicked because a docs asset is not worth taking a worker
        // thread down over.
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

/// Whether `request` will accept a gzipped response body.
///
/// RFC 9110 §12.5.3 is the whole of the rule, and the case that matters most is
/// the one that reads backwards: *no* `Accept-Encoding` header means any coding
/// is acceptable, not none. That is what a bare `curl` sends, so treating a
/// missing header as "identity only" would hand the least capable client the
/// most expensive answer.
///
/// A quality of `0` is a refusal — `gzip;q=0` and `*;q=0` both say no — and an
/// explicit mention of `gzip` outranks a `*` that would otherwise cover it.
fn accepts_gzip(request: &HttpRequest) -> bool {
    let Some(header) = request.headers().get("accept-encoding") else {
        return true;
    };
    let Ok(header) = header.to_str() else {
        return false;
    };

    let mut wildcard = None;
    for entry in header.split(',') {
        let mut parts = entry.split(';');
        let coding = parts.next().unwrap_or_default().trim();
        // `q=0`, `q=0.0`, `q=0.000` — anything that parses as zero is a
        // refusal, and anything unparseable is treated as the default of 1
        // rather than as a refusal, so a malformed parameter cannot silently
        // cost a client the compressed body.
        let acceptable = !parts.any(|parameter| {
            let parameter = parameter.trim();
            parameter
                .strip_prefix("q=")
                .or_else(|| parameter.strip_prefix("Q="))
                .and_then(|q| q.trim().parse::<f32>().ok())
                .is_some_and(|q| q == 0.0)
        });

        if coding.eq_ignore_ascii_case("gzip") {
            return acceptable;
        }
        if coding == "*" {
            wildcard = Some(acceptable);
        }
    }

    wildcard.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test::TestRequest;

    /// The header spellings a real client sends, and the two that refuse.
    #[test]
    fn gzip_is_accepted_unless_the_client_says_otherwise() {
        // The reading that is easy to get backwards: absent means "anything".
        assert!(accepts_gzip(&TestRequest::default().to_http_request()));

        for header in [
            "gzip",
            "gzip, deflate",
            "gzip, deflate, br, zstd",
            "GZIP",
            "br;q=1.0, gzip;q=0.8, *;q=0.1",
            "deflate, *",
            // The wildcard covers gzip when nothing names it.
            "*",
        ] {
            let request = TestRequest::default()
                .insert_header(("accept-encoding", header))
                .to_http_request();
            assert!(accepts_gzip(&request), "{header} should have been accepted");
        }

        for header in [
            "identity",
            "deflate",
            "gzip;q=0",
            "gzip;q=0.0",
            // Named explicitly at zero, so the wildcard beside it does not
            // rescue it.
            "gzip;q=0, *",
            "*;q=0",
            "",
        ] {
            let request = TestRequest::default()
                .insert_header(("accept-encoding", header))
                .to_http_request();
            assert!(!accepts_gzip(&request), "{header} should have been refused");
        }
    }

    /// What the page is, checked without a socket: the two substitutions
    /// happened, and neither left its placeholder behind.
    #[test]
    fn the_page_carries_the_bundle_path_and_the_configuration() {
        let docs = Docs::render();
        let html = std::str::from_utf8(&docs.html).expect("the page is UTF-8");

        assert!(
            html.contains(&format!(r#"<script src="{SCALAR_JS_PATH}">"#)),
            "{html}"
        );
        assert!(html.contains(r#""url":"/api-docs/openapi.json""#), "{html}");
        assert!(
            !html.contains("{js}") && !html.contains("{config}"),
            "{html}"
        );
    }

    /// The stored bundle is the compressed form of real JavaScript, and the
    /// build script's output survived the round trip into the binary.
    #[test]
    fn the_stored_bundle_inflates_to_the_scalar_bundle() {
        let docs = Docs::render();
        let mut js = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(docs.js_gz.as_ref()),
            &mut js,
        )
        .expect("inflating the stored bundle");

        let js = String::from_utf8(js).expect("the bundle is UTF-8");
        assert!(js.contains("createApiReference"), "not the Scalar bundle");
        // The whole point of storing it compressed: it is worth compressing.
        assert!(
            docs.js_gz.len() * 2 < js.len(),
            "{} compressed vs {} raw — is the stored form still gzip?",
            docs.js_gz.len(),
            js.len()
        );
    }
}
