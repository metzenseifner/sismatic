//! tests/openapi.rs — the API description, and whether it describes *this* API.
//!
//! Black-box like the scope suites: the real server on a real socket, and
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
//! So [`every_documented_operation_is_one_the_server_serves`] closes the loop from
//! the outside: it reads the paths the *server* published, fills their
//! parameters with data the store has been seeded with, and requests each one.
//! A documented-but-unrouted path is the only way that test fails, which is
//! precisely the drift the literals allow.
//!
//! It closes one direction only. A route added to `startup` and never given a
//! `#[utoipa::path]` is invisible to a test that starts from the document — it
//! is missing, not wrong, and nothing here will say so.
//!
//! # The other place a path is written by hand: the prose
//!
//! A route attribute is not the only literal naming a URL. utoipa lifts each
//! handler's doc comment into the operation — first paragraph to `summary`, the
//! rest to `description` — and the response and tag descriptions are prose too,
//! so a path named in any of them is rendered beside the real one in Scalar with
//! nothing pairing the two.
//!
//! That is exactly where this drifted. Each handler opens with the route it
//! serves, and the handlers are written *inside* a scope module, so the natural
//! thing to write is the path as the attribute spells it — `GET
//! /devices/{id}/commands`. But the attribute's `path` is only half a URL; the
//! `context_path` above it carries the rest. Scalar renders the whole one, so
//! the summary said `/devices/{id}/commands` while the request example beside it
//! said `/v1/commands/devices/{id}/commands`, and a reader had two paths and no
//! way to tell which was the API.
//!
//! Two tests hold the prose to the document it is rendered in:
//! [`every_operation_summary_opens_with_the_route_it_documents`] requires each
//! summary to open with the operation's own method and full path, so the
//! scope-relative form cannot survive; and
//! [`every_path_in_the_prose_is_one_this_document_declares`] checks every other
//! URL mentioned anywhere in the document — cross-references between routes,
//! mostly, which are the ones that rot silently when a scope moves.

use std::net::TcpListener;
use std::sync::Arc;

use sismatic_api_types::{Intent, Reading, ReadingValue, RecordingState, Timestamp};
use sismatic_store::outbox::{CommandSubmit, Submission};
use sismatic_store::{DynReadStore, WriteStore};
use sismatic_store_memory::MemoryStore;

mod harness;

/// The device and field every seeded reading uses, and the values substituted
/// into `{id}` and `{field}` when a documented path is requested. The device is
/// the harness catalog's, so the write routes recognise it — an id the catalog
/// does not hold now answers 404, which this suite reads as a missing route.
const DEVICE: &str = harness::DEVICE;
const FIELD: &str = "RUNNING_STATE";
/// The command id `{id}` becomes on the `/v1/commands` scope root. The first id the
/// harness's counting stamp issues, which is the one the seeded submission
/// below receives.
const COMMAND: &str = "cmd-1";

/// Start the application over a store holding one reading of [`FIELD`] on
/// [`DEVICE`], and an outbox holding one command; return its base URL.
///
/// Seeded rather than empty so that a handler *reached* can never answer 404 of
/// its own accord — which is what lets the drift test read a 404 as "no such
/// route" and nothing else.
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

    let outbox = harness::serve(listener, store);
    // Submitted straight through the port rather than over HTTP: this suite is
    // about the document, and a seeding step that went through a route would
    // make it depend on the very routing it is checking.
    outbox
        .submit(Submission {
            ids: vec![COMMAND.to_owned()],
            targets: vec![DEVICE.to_owned()],
            // Filed against the group as well, so `/v1/readings/groups/{id}/fields`
            // has an expectation to return and cannot 404 or 500 of its own
            // accord — the same reason the store below is seeded.
            group: Some(harness::GROUP.to_owned()),
            batch: None,
            barrier: None,
            intent: Intent::SetSetting {
                field: "TIMEZONE".to_owned(),
                value: "UTC".to_owned(),
            },
            at: Timestamp(harness::AT.to_owned()),
            idempotency_key: None,
        })
        .await
        .expect("seeding the outbox");

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
    assert_eq!(doc["info"]["title"], "Sismatic API");
    // Read from the crate's metadata rather than written out, so a release bump
    // cannot leave a stale number in the document.
    assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn every_documented_operation_is_one_the_server_serves() {
    let address = spawn_app().await;
    let doc = document(&address).await;

    let paths = doc["paths"].as_object().expect("paths is an object");
    // A document that described nothing would pass the loop below vacuously.
    assert_eq!(
        paths.len(),
        28,
        "expected every documented route, got {:?}",
        paths.keys().collect::<Vec<_>>()
    );

    let client = reqwest::Client::new();
    let mut checked = 0;

    for (template, item) in paths {
        let operations = item.as_object().expect("a path item is an object");
        for (method, _) in operations {
            let url = format!("{address}{}", fill(template));
            let request = match method.as_str() {
                "get" => client.get(&url),
                "post" => client.post(&url),
                // The two write bodies the document declares. A `PUT` with no
                // body is a 400 from the extractor, which would be
                // indistinguishable from a route that does not exist.
                "put" => client.put(&url).json(&serde_json::json!({"value": "x"})),
                other => panic!("{template} documents an unhandled method: {other}"),
            };
            let status = request
                .send()
                .await
                .unwrap_or_else(|e| panic!("requesting {method} {url}: {e}"))
                .status()
                .as_u16();

            // Not "is it 200": several of these routes answer `202`, and a
            // `pause` against an idle device answers a perfectly correct `409`.
            // What drift produces is a *routing* failure — 404 for a path the
            // server does not have, 405 for a method it does not accept on that
            // path — and those are what this rules out. The store and the
            // outbox are seeded so that no handler reached at all can 404 of
            // its own accord. What each route answers on its own terms is
            // `tests/commands/`.
            assert!(
                status != 404 && status != 405,
                "the document advertises {method} {template}, but {method} {url} \
                 answered {status}; the `#[utoipa::path]` attribute and the route \
                 in `startup` disagree"
            );
            checked += 1;
        }
    }

    // One operation per path here, but asserted rather than assumed: a path
    // that gained a second method and lost it in `startup` would otherwise slip
    // through as "28 paths, still fine".
    assert_eq!(checked, 28, "expected one operation per documented path");
}

/// Substitute a documented path template's parameters with data the fixtures
/// hold.
///
/// `{id}` means three different things depending on where it sits: a group id
/// in the `/groups/` half of any scope, a device id in the `/devices/` half, and
/// a command id on the one route that sits on a scope root rather than in either
/// half. They share a spelling and nothing else, and filling one route with
/// another's id produces a handler's own honest 404 that looks exactly like a
/// missing route.
///
/// The command route is matched whole rather than by prefix: every write route
/// is *also* under `/v1/commands/`, and a prefix test would hand them all a
/// command id.
fn fill(template: &str) -> String {
    let id = if template == "/v1/commands/{id}" {
        COMMAND
    } else if template.contains("/groups/") {
        harness::GROUP
    } else {
        DEVICE
    };
    template.replace("{id}", id).replace("{field}", FIELD)
}

#[tokio::test]
async fn every_operation_summary_opens_with_the_route_it_documents() {
    // The convention every handler follows — open the doc comment with the route
    // — turned into an invariant, because the convention on its own is what
    // drifted: written next to `path = "/devices/{id}/commands"`, the obvious
    // thing to write is that same string, and it is not the URL. Requiring the
    // *document's* own path here means the only spelling that passes is the one
    // a reader can paste into a client.
    //
    // Only the opening span is pinned, and only as far as the path: what follows
    // it is free prose, and a query string inside it is the history routes
    // spelling out their filters. What is held is that a summary exists at all
    // and that the route it opens with is this operation's own.
    let address = spawn_app().await;
    let doc = document(&address).await;

    for (template, item) in doc["paths"].as_object().expect("paths is an object") {
        for (method, operation) in item.as_object().expect("a path item is an object") {
            let summary = operation["summary"].as_str().unwrap_or_else(|| {
                panic!(
                    "{method} {template} carries no summary; Scalar renders the \
                     operation with no title at all"
                )
            });
            // The source wraps a long summary across `///` lines, which reaches
            // the document as a newline that may fall inside the backticks.
            // Whitespace is not the subject here, so it is flattened first.
            let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
            let opening = summary
                .strip_prefix('`')
                .and_then(|rest| rest.split('`').next())
                .map(|span| span.split('?').next().unwrap_or(span).to_owned());
            let expected = format!("{} {template}", method.to_uppercase());
            assert_eq!(
                opening.as_deref(),
                Some(expected.as_str()),
                "{method} {template} opens its summary with {summary:?}, which does \
                 not name the route it documents"
            );
        }
    }
}

/// Every URL named in the document's prose, as `(where it was written, the URL)`.
///
/// Only inside backticks: that is how this codebase writes a path, and it keeps
/// the scan off prose that merely contains a slash. Anything `/`-initial within
/// a code span is a claim about this API's URL space, and the caller checks it.
fn quoted_paths(doc: &serde_json::Value) -> Vec<(String, String)> {
    fn scan(found: &mut Vec<(String, String)>, where_: &str, prose: &serde_json::Value) {
        let Some(prose) = prose.as_str() else { return };
        // Odd indices are the spans between backticks; an unterminated final
        // span cannot exist, since `split` yields the tail at an even index.
        for span in prose.split('`').skip(1).step_by(2) {
            for token in span.split_whitespace() {
                let token = token.trim_end_matches(['.', ',', ';', ':']);
                // A query string is not part of the path, and the history routes
                // document theirs in the summary.
                let token = token.split('?').next().unwrap_or(token);
                if token.starts_with('/') && token.len() > 1 {
                    found.push((where_.to_owned(), token.to_owned()));
                }
            }
        }
    }

    let mut found = Vec::new();
    scan(&mut found, "info", &doc["info"]["description"]);
    for tag in doc["tags"].as_array().expect("the document declares tags") {
        let where_ = format!("the '{}' tag", tag["name"].as_str().expect("a tag name"));
        scan(&mut found, &where_, &tag["description"]);
    }
    for (template, item) in doc["paths"].as_object().expect("paths is an object") {
        for (method, operation) in item.as_object().expect("a path item is an object") {
            let where_ = format!("{method} {template}");
            scan(&mut found, &where_, &operation["summary"]);
            scan(&mut found, &where_, &operation["description"]);
            for parameter in operation["parameters"].as_array().unwrap_or(&Vec::new()) {
                scan(&mut found, &where_, &parameter["description"]);
            }
            let responses = operation["responses"].as_object();
            for (status, response) in responses.into_iter().flatten() {
                scan(
                    &mut found,
                    &format!("{where_} [{status}]"),
                    &response["description"],
                );
            }
        }
    }
    found
}

/// Whether `quoted` could be a real URL of a server serving `declared`.
///
/// True when it is a segment-wise prefix of some declared path, comparing a
/// `{parameter}` on either side as a wildcard. Three shapes have to pass, and
/// they are all things the prose legitimately writes:
///
/// * the path itself — `/v1/commands/groups/{id}/commands`;
/// * a *prefix* of one, which is how a whole half of a scope is named in a
///   sentence — `/v1/commands/groups`, meaning "the group routes";
/// * a template with a parameter filled in, which is how a concrete example is
///   given — `/v1/readings/devices/{id}/fields/RUNNING_STATE`.
///
/// What it rejects is the drift: `/devices/{id}/commands` is a prefix of nothing
/// this server serves, because every path starts with a scope.
fn is_documented(quoted: &str, declared: &[&String]) -> bool {
    fn segments(path: &str) -> Vec<&str> {
        path.trim_matches('/').split('/').collect()
    }

    let quoted = segments(quoted);
    declared.iter().any(|path| {
        let path = segments(path);
        quoted.len() <= path.len()
            && quoted
                .iter()
                .zip(&path)
                .all(|(q, p)| q == p || q.starts_with('{') || p.starts_with('{'))
    })
}

#[tokio::test]
async fn every_path_in_the_prose_is_one_this_document_declares() {
    // The summaries are held down route by route above. This is the rest of the
    // prose, where a path is named to point at a *different* route than the one
    // being described — "this id names a device group; `/v1/commands/groups/{id}/commands`
    // is the answer". Those are the most useful sentences in the document and
    // the ones nothing else checks: the route they name is not the route they
    // are attached to, so moving a scope leaves them pointing at a 404 while
    // every other test still passes.
    let address = spawn_app().await;
    let doc = document(&address).await;

    let declared: Vec<&String> = doc["paths"]
        .as_object()
        .expect("paths is an object")
        .keys()
        .collect();

    let quoted = quoted_paths(&doc);
    // A scan that found nothing would pass while saying nothing, and the
    // cross-references it exists for are the reason it is worth running.
    assert!(
        quoted.len() > 20,
        "expected the prose to reference paths, found {quoted:?}"
    );

    for (where_, path) in &quoted {
        assert!(
            is_documented(path, &declared),
            "{where_} names `{path}`, which is not a path this server serves — \
             a scope-relative path from a `#[utoipa::path]` attribute, or a \
             cross-reference left behind when a route moved"
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
        27,
        "expected every readings, group, commands and inventory route under /v1, \
         got {:?}",
        paths.keys().collect::<Vec<_>>()
    );
    // ...and the health check deliberately outside it: a liveness probe is not
    // part of the versioned contract.
    assert!(paths.contains_key("/health_check"));
}

/// Tags name the *question* a route answers, not the resource in its path.
///
/// `inventory` covers `/v1/devices` and `/v1/groups` alike; `commands` accepts
/// either kind of id, since the two share one namespace on the write side; and
/// `readings` covers the device field routes and the group field routes both.
/// The axis is worth pinning because the other one is the tempting mistake: a
/// tag named after a resource would file `/v1/groups/{id}` and
/// `/v1/readings/groups/{id}/fields` in different sections of one document while leaving
/// `/v1/groups` beside `/v1/devices`, and would hand a generated client a
/// `GroupsApi` that does not contain `listGroups`.
#[tokio::test]
async fn tags_name_the_question_a_route_answers_not_the_resource_it_names() {
    let address = spawn_app().await;
    let doc = document(&address).await;

    let declared: Vec<&str> = doc["tags"]
        .as_array()
        .expect("the document declares tags")
        .iter()
        .map(|t| t["name"].as_str().expect("a tag name"))
        .collect();
    assert_eq!(declared, ["readings", "inventory", "commands", "health"]);

    // Every operation carries exactly one tag, and it is one of those four. An
    // untagged operation lands in the renderer's catch-all bucket, which is how
    // a route goes missing from the rendered document without going missing from
    // the server.
    for (template, item) in doc["paths"].as_object().expect("paths is an object") {
        for (method, operation) in item.as_object().expect("a path item is an object") {
            let tags: Vec<&str> = operation["tags"]
                .as_array()
                .unwrap_or_else(|| panic!("{method} {template} carries no tags"))
                .iter()
                .map(|t| t.as_str().expect("a tag"))
                .collect();
            assert_eq!(tags.len(), 1, "{method} {template} carries {tags:?}");
            assert!(
                declared.contains(&tags[0]),
                "{method} {template} is tagged '{}', which the document does not declare",
                tags[0]
            );
        }
    }

    // The pairing the axis exists for: one device route and one group route,
    // asking the same question, filed together.
    for path in [
        "/v1/readings/devices/{id}/fields/{field}",
        "/v1/readings/groups/{id}/fields/{field}",
    ] {
        assert_eq!(
            doc["paths"][path]["get"]["tags"][0], "readings",
            "for {path}"
        );
    }
    // ...and the counterpart: two paths sharing the `/v1/groups` prefix that
    // answer different questions, filed apart.
    assert_eq!(
        doc["paths"]["/v1/inventory/groups/{id}"]["get"]["tags"][0],
        "inventory"
    );
}

#[tokio::test]
async fn the_history_route_documents_the_query_filters() {
    // These four come from `ReadingQuery`'s `IntoParams` rather than from a list
    // restated in the route attribute, so this test is really about that
    // derivation still reaching the document.
    let address = spawn_app().await;
    let doc = document(&address).await;

    let params =
        doc["paths"]["/v1/readings/devices/{id}/fields/{field}/history"]["get"]["parameters"]
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

    let not_found =
        &doc["paths"]["/v1/readings/devices/{id}/fields/{field}"]["get"]["responses"]["404"];
    assert!(
        !not_found.is_null(),
        "the latest-value route should document its 404"
    );

    let schema = &doc["components"]["schemas"]["ApiError"];
    assert!(
        schema["properties"]["code"].is_object(),
        "ApiError should carry the machine-readable code, got {schema}"
    );
    // The second machine-readable field, and the `Rejection` schema it refs.
    // A generated client gets a real union to switch on rather than four
    // values buried in `ErrorCode` with nothing saying which pair with a 409.
    assert!(
        schema["properties"]["rejection"].is_object(),
        "ApiError should carry the rejection, got {schema}"
    );
    let rejection = &doc["components"]["schemas"]["Rejection"];
    assert_eq!(
        rejection["enum"],
        serde_json::json!([
            "metadata_frozen",
            "already_recording",
            "already_paused",
            "not_recording"
        ]),
        "got {rejection}"
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
async fn the_ui_is_reachable_with_a_trailing_slash() {
    // The UI is the exact resource `/scalar`, so the slashed form matches no
    // resource at all. This is the one route in the application that a person
    // reaches by typing it, so it redirects rather than 404s.
    let address = spawn_app().await;

    let response = reqwest::get(format!("{address}/api/"))
        .await
        .expect("requesting the ui with a trailing slash");

    // The client followed the redirect, which is what a browser does.
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response.url().path(),
        "/api",
        "expected to land on the slashless path, got {}",
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

    for path in ["/health_check/", "/v1/readings/devices/atrium-101/fields/"] {
        let status = reqwest::get(format!("{address}{path}"))
            .await
            .expect("requesting a slashed path")
            .status()
            .as_u16();

        assert_eq!(status, 404, "{path} should not have been normalized");
    }
}

#[tokio::test]
async fn the_ui_is_served_and_points_at_the_document() {
    let address = spawn_app().await;

    let response = reqwest::get(format!("{address}/api"))
        .await
        .expect("requesting the api reference");

    assert_eq!(response.status().as_u16(), 200);
    let content_type = content_type(&response);
    assert!(
        content_type.starts_with("text/html"),
        "expected HTML, got {content_type}"
    );

    let body = response.text().await.expect("reading the page");
    assert!(
        body.contains("Scalar.createApiReference"),
        "the page should be the Scalar shell"
    );
    // Relative, so the page reads the document from whatever host served it.
    // An absolute URL here would pin the docs to one deployment's hostname.
    assert!(
        body.contains(r#""url":"/api-docs/openapi.json""#),
        "the page should point at this server's document, got: {body}"
    );
}

#[tokio::test]
async fn the_ui_reaches_for_nothing_off_this_server() {
    // The reason the bundle is embedded at all: these installations routinely
    // have no route off the LAN. A page served from the binary that then loads
    // its JavaScript — or its webfonts, or an AI assistant — from a CDN is
    // exactly as blank as one that was never embedded, so what is worth pinning
    // is the absence of an external host anywhere in the page rather than the
    // presence of the local one.
    //
    // One of these would matter even with a route off the LAN: left undefined,
    // `proxyUrl` sends every "Try it" request through `proxy.scalar.com`, auth
    // panel and all. See `openapi::scalar_config`.
    let address = spawn_app().await;

    let body = reqwest::get(format!("{address}/api"))
        .await
        .expect("requesting the api reference")
        .text()
        .await
        .expect("reading the page");

    // Three passes, because "off-host" hides in three different places. First
    // the hosts the upstream defaults are known to name — these turn up inside
    // the configuration object as readily as in an attribute, where no amount
    // of markup parsing would find them.
    for host in [
        "cdn.jsdelivr.net",
        "unpkg.com",
        "fonts.googleapis.com",
        "fonts.scalar.com",
        "proxy.scalar.com",
    ] {
        assert!(!body.contains(host), "the page reaches for {host}: {body}");
    }

    // Then the two settings whose host lives inside the bundle rather than
    // here, so there is no name for the pass above to scan for: the webfonts,
    // and the request proxy every "Try it" call is routed through when
    // `proxyUrl` is left undefined. Both are off by being spelled out, so a
    // regression looks like the key going missing — which a test can see and a
    // scan for hostnames cannot.
    for setting in [r#""withDefaultFonts":false"#, r#""proxyUrl":"""#] {
        assert!(
            body.contains(setting),
            "the page does not carry {setting}: {body}"
        );
    }

    // Then every URL the markup actually references, whatever host it names:
    // each must be rooted at `/` and must not be the `//host/path` form, which
    // is an off-host reference that spells no scheme and so passes any check
    // looking for `http`.
    for attribute in ["src=\"", "href=\""] {
        for (at, matched) in body.match_indices(attribute) {
            let url = body[at + matched.len()..]
                .split('"')
                .next()
                .expect("a quoted attribute value");
            assert!(
                url.starts_with('/') && !url.starts_with("//"),
                "the page references {url}, which this server does not serve"
            );
        }
    }

    // And the one thing it does load is served from here.
    let response = reqwest::get(format!("{address}/scalar/scalar.js"))
        .await
        .expect("requesting the bundle");

    assert_eq!(response.status().as_u16(), 200);
    let content_type = content_type(&response);
    assert!(
        content_type.contains("javascript"),
        "expected JavaScript, got {content_type}"
    );
    assert!(
        !response
            .bytes()
            .await
            .expect("reading the bundle")
            .is_empty(),
        "the bundle should not be empty"
    );
}

#[tokio::test]
async fn the_bundle_is_served_compressed_to_a_client_that_takes_it() {
    // The binary stores the bundle gzipped and hands those bytes straight out,
    // which is what makes it a megabyte in the binary rather than four. The
    // check that it is *actually* compressed on the wire is the gzip magic
    // number, because a `Content-Encoding` header is a claim and the two bytes
    // are the thing itself.
    let address = spawn_app().await;

    let response = reqwest::Client::new()
        .get(format!("{address}/scalar/scalar.js"))
        .header("accept-encoding", "gzip, deflate, br")
        .send()
        .await
        .expect("requesting the bundle");

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        header(&response, "content-encoding"),
        "gzip",
        "the bundle should have been served compressed"
    );
    // One URL, two possible bodies, so a cache between here and there has to
    // key on the request header that decides which.
    assert_eq!(header(&response, "vary"), "accept-encoding");

    let body = response.bytes().await.expect("reading the bundle");
    assert_eq!(
        &body[..2],
        &[0x1f, 0x8b],
        "expected a gzip stream, got {:?}",
        &body[..body.len().min(8)]
    );
    assert!(
        body.len() < 2_000_000,
        "the compressed bundle is {} bytes — is it still compressed?",
        body.len()
    );
}

#[tokio::test]
async fn the_bundle_is_inflated_for_a_client_that_refuses_it() {
    // The rare half, and the reason the handler keeps a decompressor at all: a
    // client that names an encoding set gzip is not in gets real JavaScript
    // rather than bytes it cannot read.
    let address = spawn_app().await;

    let response = reqwest::Client::new()
        .get(format!("{address}/scalar/scalar.js"))
        .header("accept-encoding", "identity")
        .send()
        .await
        .expect("requesting the bundle");

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        header(&response, "content-encoding"),
        "",
        "an identity-only client should not have been sent a coding"
    );

    let body = response.text().await.expect("reading the bundle");
    assert!(
        body.contains("createApiReference"),
        "expected the Scalar bundle as plain JavaScript"
    );
}

/// The `Content-Type` of a response, or the empty string if it carries none.
fn content_type(response: &reqwest::Response) -> String {
    header(response, "content-type")
}

/// One header of a response, or the empty string if it carries none — so an
/// absent header and an unreadable one assert the same way a client would see
/// them, rather than needing an `Option` at every call site.
fn header(response: &reqwest::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}
