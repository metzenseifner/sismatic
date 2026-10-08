//! tests/ui.rs — the embedded frontend as a browser meets it.
//!
//! Black-box, like the rest of the suite: every test starts the real server on
//! a real socket and asks for a URL. What is worth pinning here is not that a
//! handler returned bytes but *which* URLs answer with the application, which
//! answer with a file, and — the test this file exists for — which still answer
//! with the 404 they answered before a frontend was embedded at all. See
//! `sismatic_http_api::ui` for why the application owns a subtree rather than
//! the root; `the_api_keeps_the_404s_a_root_mount_would_have_swallowed` is that
//! argument written as an assertion.
//!
//! Two things about the client are deliberate. It asks for `identity` wherever
//! the body is read, because the assets are *stored* gzipped and this
//! workspace's `reqwest` is built without the compression features — it would
//! neither negotiate an encoding nor inflate one. And it is built with
//! redirects disabled where a redirect is the thing under test, since the
//! default policy would follow it and hide it.

#![cfg(feature = "ui")]

use std::sync::Arc;

use sismatic_http_api::ui::UI_PATH;
use sismatic_store::DynReadStore;
use sismatic_store_memory::MemoryStore;

mod harness;

/// Start the application and return its base URL.
///
/// No test here reads data, so the store is empty; what is under test is served
/// out of the binary's own image and consults nothing.
fn spawn() -> String {
    let store: DynReadStore = Arc::new(MemoryStore::default());
    let (address, _) = harness::spawn(store);
    address
}

/// `GET path`, asking for an uncompressed body. Returns status, headers, body.
async fn get(address: &str, path: &str) -> (u16, reqwest::header::HeaderMap, String) {
    let response = reqwest::Client::new()
        .get(format!("{address}{path}"))
        .header("accept-encoding", "identity")
        .send()
        .await
        .unwrap_or_else(|error| panic!("requesting {path}: {error}"));

    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.text().await.expect("reading the body");
    (status, headers, body)
}

/// The header value as a string, or `""`.
fn header(headers: &reqwest::header::HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn the_application_is_served_under_its_own_subtree() {
    let address = spawn();

    let (status, headers, body) = get(&address, &format!("{UI_PATH}/")).await;

    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), "text/html; charset=utf-8");
    assert!(
        body.to_lowercase().starts_with("<!doctype html"),
        "not an HTML document: {body}"
    );
    // The one URL whose contents change while its name does not, so a browser
    // must not keep it: a cached copy would pin it to an older build's asset
    // names.
    assert_eq!(header(&headers, "cache-control"), "no-cache");
}

#[tokio::test]
async fn the_slashless_subtree_serves_the_application_too() {
    // `/ui` is what a person types. A tail pattern needs the leading `/` to
    // match, so without the scope-root resource this would be a 404 — and
    // unlike `/api` this answers directly rather than redirecting, because
    // every asset URL in the page is absolute and so does not depend on which
    // of the two URLs loaded it.
    let address = spawn();

    let (slashless, _, slashless_body) = get(&address, UI_PATH).await;
    let (slashed, _, slashed_body) = get(&address, &format!("{UI_PATH}/")).await;

    assert_eq!(slashless, 200);
    assert_eq!(slashed, 200);
    assert_eq!(slashless_body, slashed_body);
}

#[tokio::test]
async fn a_deep_link_is_answered_with_the_application() {
    // The property that makes a single-page app's router work: a URL the server
    // has never heard of still has to arrive at the page, which then reads it.
    let address = spawn();

    let (status, headers, body) = get(&address, &format!("{UI_PATH}/devices/atrium-101")).await;
    let (_, _, index) = get(&address, &format!("{UI_PATH}/")).await;

    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), "text/html; charset=utf-8");
    assert_eq!(body, index);
}

#[tokio::test]
async fn a_missing_file_is_a_404_rather_than_the_application() {
    // The other half of the rule above, and the half that is easy to get
    // wrong. Answering the page to *everything* turns a mistyped script URL
    // into `200 text/html`, and what the browser then reports is a MIME error
    // on a file it believes exists — instead of the 404 that would have said
    // the asset is simply not there.
    let address = spawn();

    for path in [
        // Vite's own output directory: its contents are known exhaustively at
        // build time, so a miss here is always a miss.
        "/assets/index-deadbeef.js",
        "/assets/nope.css",
        // Anything that names a file, wherever it is.
        "/nope.css",
        "/favicon.png",
        "/nested/thing.js",
    ] {
        let (status, _, _) = get(&address, &format!("{UI_PATH}{path}")).await;
        assert_eq!(status, 404, "{UI_PATH}{path} should not have been the page");
    }
}

#[tokio::test]
async fn the_api_keeps_the_404s_a_root_mount_would_have_swallowed() {
    // This is the test the subtree exists for. Served from `/`, the frontend's
    // fallback would answer the page — `200 text/html` — to every path below,
    // and each one is a deliberate 404 that something depends on:
    //
    // * `/health` is the complement that makes a 200 from `/health_check` mean
    //   something (tests/health_check.rs).
    // * the slashed forms are `the_slash_is_folded_for_the_ui_only`
    //   (tests/openapi.rs) and `the_roots_are_exact_paths`
    //   (tests/instructions.rs): this application folds a trailing slash on one
    //   route and nowhere else.
    // * `/v2/reads` is a version that does not exist, which a client must be
    //   able to tell from one that does.
    let address = spawn();

    for path in [
        "/health",
        "/health_check/",
        "/v1/reads/",
        "/v1/writes/",
        "/v2/reads",
        "/nope",
        "/api-docs",
    ] {
        let (status, _, _) = get(&address, path).await;
        assert_eq!(status, 404, "{path} should still have been a 404");
    }
}

#[tokio::test]
async fn the_root_redirects_to_the_application() {
    // The bare host is what an operator types, and nothing else serves `/`.
    let address = spawn();

    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("building a client that does not follow redirects")
        .get(format!("{address}/"))
        .send()
        .await
        .expect("requesting the root");

    assert!(
        response.status().is_redirection(),
        "got {}",
        response.status()
    );
    assert_eq!(
        header(response.headers(), "location"),
        format!("{UI_PATH}/")
    );
}

#[tokio::test]
async fn an_asset_is_served_compressed_unless_the_client_refuses() {
    // The stored form *is* the compressed form, so the common path — every
    // browser there is — copies nothing and compresses nothing. The identity
    // path exists for correctness rather than because anything takes it.
    let address = spawn();
    let url = format!("{address}{UI_PATH}/");

    let compressed = reqwest::Client::new()
        .get(&url)
        .header("accept-encoding", "gzip")
        .send()
        .await
        .expect("requesting the page as gzip");

    assert_eq!(compressed.status().as_u16(), 200);
    assert_eq!(header(compressed.headers(), "content-encoding"), "gzip");
    // Two clients that disagree about `Accept-Encoding` get different bytes
    // from this one URL, so anything caching in between has to key on it.
    assert_eq!(header(compressed.headers(), "vary"), "accept-encoding");

    let plain = reqwest::Client::new()
        .get(&url)
        .header("accept-encoding", "identity")
        .send()
        .await
        .expect("requesting the page as identity");

    assert_eq!(plain.status().as_u16(), 200);
    assert_eq!(header(plain.headers(), "content-encoding"), "");
    assert!(
        plain
            .text()
            .await
            .expect("reading the page")
            .to_lowercase()
            .starts_with("<!doctype html")
    );
}
