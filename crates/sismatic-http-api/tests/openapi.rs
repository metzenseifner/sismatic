//! tests/openapi.rs — the API description, and whether it describes *this* API.
//!
//! Black-box like the other two suites: the real server on a real socket, and
//! the document read back over HTTP rather than built in-process. What is worth
//! pinning is what a browser pointed at a running server receives.
//!
//! # The one thing that can drift, and the test that stops it
//!
//! Almost everything in the document is derived — schemas come off the DTOs'
//! `ToSchema`, query parameters off `ReadingQuery`'s `IntoParams` — so almost
//! nothing here *can* disagree with the server. The exception is the path
//! strings: `#[utoipa::path(path = "...")]` on a handler and
//! `web::resource("...")` in `startup` are two literals, written twice, that
//! nothing pairs. Move a route in one and not the other and the document
//! confidently advertises a URL that answers 404.
//!
//! So [`every_documented_path_is_a_path_the_server_serves`] closes the loop from
//! the outside: it reads the paths the *server* published, fills their
//! parameters with data the store has been seeded with, and requests each one.
//! A documented-but-unrouted path is the only way that test fails, which is
//! precisely the drift the literals allow.
//!
//! It closes one direction only. A route added to `startup` and never given a
//! `#[utoipa::path]` is invisible to a test that starts from the document — it
//! is missing, not wrong, and nothing here will say so.

use std::net::TcpListener;
use std::sync::Arc;

use sismatic_api_types::{Reading, ReadingValue, RecordingState, Timestamp};
use sismatic_store::{DynReadStore, WriteStore};
use sismatic_store_memory::MemoryStore;

/// The device and field every seeded reading uses, and the values substituted
/// into `{id}` and `{field}` when a documented path is requested.
const DEVICE: &str = "atrium-101";
const FIELD: &str = "RUNNING_STATE";

/// Start the application over a store holding one reading of [`FIELD`] on
/// [`DEVICE`]; return its base URL.
///
/// Seeded rather than empty so that *every* documented path can answer `200`.
/// That makes the drift test's expectation exactly "200", with no per-path
/// allowance for a legitimate 404 that a route-miss could then hide behind.
async fn spawn_app() -> String {
    let store = MemoryStore::default();
    store
        .upsert_latest(Reading {
            device: DEVICE.into(),
            field: FIELD.into(),
            value: ReadingValue::State(RecordingState::Started),
            at: Timestamp("2026-07-23T14:03:11Z".into()),
        })
        .await
        .expect("seeding the store");
    let store: DynReadStore = Arc::new(store);

    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();

    let server = sismatic_http_api::run(listener, store).expect("building the server");
    drop(tokio::spawn(server));

    format!("http://127.0.0.1:{port}")
}

/// Fetch and parse the served OpenAPI document.
async fn document(address: &str) -> serde_json::Value {
    let response = reqwest::get(format!("{address}/api-docs/openapi.json"))
        .await
        .expect("requesting the openapi document");
    assert_eq!(response.status().as_u16(), 200);
    response.json().await.expect("parsing the document as JSON")
}

#[tokio::test]
async fn the_document_is_served_as_openapi_json() {
    let address = spawn_app().await;

    let doc = document(&address).await;

    // The version field is what tells a generator which dialect it is reading,
    // so it is the one key whose absence makes the rest unusable.
    assert!(
        doc["openapi"].as_str().is_some_and(|v| v.starts_with("3.")),
        "expected an OpenAPI 3.x document, got {}",
        doc["openapi"]
    );
    assert_eq!(doc["info"]["title"], "Sismatic read API");
    // Read from the crate's metadata rather than written out, so a release bump
    // cannot leave a stale number in the document.
    assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn every_documented_path_is_a_path_the_server_serves() {
    let address = spawn_app().await;
    let doc = document(&address).await;

    let paths = doc["paths"].as_object().expect("paths is an object");
    // A document that described nothing would pass the loop below vacuously.
    assert_eq!(
        paths.len(),
        4,
        "expected the four documented routes, got {:?}",
        paths.keys().collect::<Vec<_>>()
    );

    for (template, item) in paths {
        assert!(
            item.get("get").is_some(),
            "{template} is documented but not as a GET"
        );

        let url = template.replace("{id}", DEVICE).replace("{field}", FIELD);
        let status = reqwest::get(format!("{address}{url}"))
            .await
            .unwrap_or_else(|e| panic!("requesting the documented path {url}: {e}"))
            .status()
            .as_u16();

        // 404 here means the server has no such route — the store was seeded so
        // that a handler reached at all has something to answer with.
        assert_eq!(
            status, 200,
            "the document advertises {template}, but {url} answered {status}; \
             the `#[utoipa::path]` attribute and the route in `startup` disagree"
        );
    }
}

#[tokio::test]
async fn the_versioned_routes_are_documented_under_their_scope() {
    // The `/v1` prefix comes from `web::scope` in `startup` and from
    // `context_path` in each path attribute — a third literal, and the one a
    // reader of the document depends on to build a working URL.
    let address = spawn_app().await;
    let doc = document(&address).await;

    let paths = doc["paths"].as_object().expect("paths is an object");
    let versioned: Vec<&String> = paths.keys().filter(|p| p.starts_with("/v1/")).collect();

    assert_eq!(
        versioned.len(),
        3,
        "expected the three readings routes under /v1, got {:?}",
        paths.keys().collect::<Vec<_>>()
    );
    // ...and the health check deliberately outside it: a liveness probe is not
    // part of the versioned contract.
    assert!(paths.contains_key("/health_check"));
}

#[tokio::test]
async fn the_history_route_documents_the_query_filters() {
    // These four come from `ReadingQuery`'s `IntoParams` rather than from a list
    // restated in the route attribute, so this test is really about that
    // derivation still reaching the document.
    let address = spawn_app().await;
    let doc = document(&address).await;

    let params = doc["paths"]["/v1/devices/{id}/fields/{field}/history"]["get"]["parameters"]
        .as_array()
        .expect("the history route documents parameters");

    let query: Vec<&str> = params
        .iter()
        .filter(|p| p["in"] == "query")
        .map(|p| p["name"].as_str().expect("a parameter name"))
        .collect();

    assert_eq!(query, ["field", "start", "end", "limit"]);
}

#[tokio::test]
async fn the_error_envelope_is_documented_where_it_can_be_returned() {
    // The failure shape is half the contract: a client branches on `code`, and
    // it can only do that if the document says which responses carry one.
    let address = spawn_app().await;
    let doc = document(&address).await;

    let not_found = &doc["paths"]["/v1/devices/{id}/fields/{field}"]["get"]["responses"]["404"];
    assert!(
        !not_found.is_null(),
        "the latest-value route should document its 404"
    );

    let schema = &doc["components"]["schemas"]["ApiError"];
    assert!(
        schema["properties"]["code"].is_object(),
        "ApiError should carry the machine-readable code, got {schema}"
    );
}

#[tokio::test]
async fn the_string_aliases_are_documented_as_strings() {
    // `DeviceId` and `FieldName` are `String` aliases, but a derive sees only
    // the name it was written with: left alone, utoipa refs a component it
    // invented called `String`, and every generated client grows a wrapper type
    // around a plain string. The `schema(value_type = String)` annotations in
    // `api-types` are what prevent that, and this is what notices if one is
    // dropped or a new alias field arrives without one.
    let address = spawn_app().await;
    let doc = document(&address).await;

    let schemas = doc["components"]["schemas"]
        .as_object()
        .expect("components.schemas is an object");
    assert!(
        !schemas.contains_key("String"),
        "a primitive leaked into components as a named schema: {:?}",
        schemas.keys().collect::<Vec<_>>()
    );

    let device = &doc["components"]["schemas"]["Reading"]["properties"]["device"];
    assert_eq!(device["type"], "string", "got {device}");
}

#[tokio::test]
async fn the_ui_is_reachable_without_the_trailing_slash() {
    // `/swagger-ui/{_:.*}` has the slash in its literal prefix, so the slashless
    // form matches no resource at all. This is the one route in the application
    // that a person reaches by typing it, so it redirects rather than 404s.
    let address = spawn_app().await;

    let response = reqwest::get(format!("{address}/swagger-ui"))
        .await
        .expect("requesting the ui without a trailing slash");

    // The client followed the redirect, which is what a browser does.
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        response.url().path().ends_with("/swagger-ui/"),
        "expected to land on the slashed path, got {}",
        response.url()
    );
}

#[tokio::test]
async fn the_slash_is_folded_for_the_ui_only() {
    // The counterpart of the test above, and the reason it is a redirect on one
    // route rather than `NormalizePath` across the application: the 404s and
    // 405s the other routes answer with are deliberate, and middleware that
    // folded trailing slashes everywhere would quietly turn them into aliases.
    let address = spawn_app().await;

    for path in ["/health_check/", "/v1/devices/atrium-101/fields/"] {
        let status = reqwest::get(format!("{address}{path}"))
            .await
            .expect("requesting a slashed path")
            .status()
            .as_u16();

        assert_eq!(status, 404, "{path} should not have been normalized");
    }
}

#[tokio::test]
async fn swagger_ui_is_served_and_points_at_the_document() {
    let address = spawn_app().await;

    let response = reqwest::get(format!("{address}/swagger-ui/"))
        .await
        .expect("requesting the swagger ui");

    assert_eq!(response.status().as_u16(), 200);
    // Served from the binary, not fetched from a CDN — so this is a real page
    // even on a host with no route off the LAN.
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("text/html"),
        "expected HTML, got {content_type}"
    );

    let body = response.text().await.expect("reading the page");
    assert!(
        body.contains("swagger-ui"),
        "the page should be the Swagger UI shell"
    );
}
