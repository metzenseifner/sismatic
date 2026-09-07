//! tests/config.rs — the config scope, from outside.
//!
//! Black-box like the other suites: a real server on a real socket, addressed
//! with a real client. What is pinned here is what the *routes* are responsible
//! for — that a body arrives at the port parsed, that what the port answers is
//! what a caller receives, that each refusal becomes the status its case calls
//! for, and that the wrong verb on the right path is a 405 rather than a 404.
//!
//! What is deliberately *not* pinned here is what a patch does to a config.
//! Folding one onto the running settings is the composition root's, tested in
//! `sismatic-server`'s own suite over values — this crate may not name that
//! crate, which is the whole reason the port exists. So these run over
//! `harness::StatedConfig`, which answers with what it was told and records what
//! it was asked. See the harness for the argument.

use sismatic_api_types::{ApiError, ConfigDocument, ErrorCode};
use sismatic_http_api::config::ConfigRefusal;

mod harness;

use harness::{StatedConfig, settings, spawn_with_config};

/// A server whose settings port accepts everything and reports the harness's
/// stated document.
fn accepting() -> (String, std::sync::Arc<StatedConfig>) {
    spawn_with_config(StatedConfig::default())
}

/// A server whose settings port refuses every change with `refusal`.
fn refusing(refusal: ConfigRefusal) -> (String, std::sync::Arc<StatedConfig>) {
    spawn_with_config(StatedConfig::refusing(refusal))
}

#[tokio::test]
async fn the_scope_root_reports_every_setting() {
    let (address, _config) = accepting();

    let response = reqwest::get(format!("{address}/v1/config"))
        .await
        .expect("requesting the config");

    assert_eq!(response.status().as_u16(), 200);
    let body: ConfigDocument = response.json().await.expect("parsing the document");
    assert_eq!(body, settings());
}

#[tokio::test]
async fn a_patch_reaches_the_port_parsed_and_answers_with_the_settings() {
    // The two halves of what this route is for, and they are separate claims: a
    // body that never arrived would still produce a plausible-looking `200`,
    // since the response is the settings as they stand either way.
    let (address, config) = accepting();

    let response = reqwest::Client::new()
        .patch(format!("{address}/v1/config"))
        .json(&serde_json::json!({"sync": {"interval_secs": 60}}))
        .send()
        .await
        .expect("patching the config");

    assert_eq!(response.status().as_u16(), 200);
    let body: ConfigDocument = response.json().await.expect("parsing the document");
    assert_eq!(
        body,
        settings(),
        "the response is every setting, not a diff"
    );

    let applied = config.applied();
    assert_eq!(applied.len(), 1);
    let sync = applied[0].sync.clone().expect("the patch named sync");
    assert_eq!(sync.interval_secs, Some(60));
    // ...and named nothing else, which is what makes a patch a patch.
    assert_eq!(sync.fields, None);
    assert_eq!(applied[0].store, None);
    assert_eq!(applied[0].intent_relay, None);
}

#[tokio::test]
async fn an_empty_patch_is_a_patch() {
    // What a caller sends to change nothing, and what every partial body is
    // built out of. It has to reach the port rather than being refused as an
    // empty request, because "apply nothing" is a legitimate no-op — and is what
    // `tests/openapi.rs` sends when it walks every documented route.
    let (address, config) = accepting();

    let status = reqwest::Client::new()
        .patch(format!("{address}/v1/config"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("patching the config")
        .status()
        .as_u16();

    assert_eq!(status, 200);
    assert_eq!(config.applied().len(), 1);
}

#[tokio::test]
async fn a_misspelled_key_is_refused_before_it_reaches_the_port() {
    // `deny_unknown_fields` on the wire contract, from outside. A silently
    // ignored key is the one mistake a caller cannot notice on its own: the
    // response is the settings as they stand, which looks exactly like a change
    // that did not take.
    let (address, config) = accepting();

    let response = reqwest::Client::new()
        .patch(format!("{address}/v1/config"))
        .json(&serde_json::json!({"sync": {"interval_sec": 60}}))
        .send()
        .await
        .expect("patching the config");

    assert_eq!(response.status().as_u16(), 400);
    assert!(
        config.applied().is_empty(),
        "a body that does not deserialize must not reach the port"
    );
}

#[tokio::test]
async fn a_value_the_port_cannot_read_is_the_callers_fault() {
    let (address, _config) = refusing(ConfigRefusal::Malformed(
        "store.retain: '5 fortnights' is not a duration".to_owned(),
    ));

    let response = reqwest::Client::new()
        .patch(format!("{address}/v1/config"))
        .json(&serde_json::json!({"store": {"retain": "5 fortnights"}}))
        .send()
        .await
        .expect("patching the config");

    assert_eq!(response.status().as_u16(), 400);
    let body: ApiError = response.json().await.expect("parsing the error");
    assert_eq!(body.code, Some(ErrorCode::BadRequest));
    // The message reaches the caller intact: it is the only thing that says
    // which of half a dozen values in one body was the problem.
    assert!(body.error.contains("fortnights"), "got: {}", body.error);
}

#[tokio::test]
async fn a_setting_that_needs_a_restart_is_a_conflict() {
    // The status a script branches on. A deployment automating a ConfigMap roll
    // reads a 409 as "this one needs a rolling restart" and a 400 as "the file
    // is wrong", and they call for opposite responses.
    let (address, _config) = refusing(ConfigRefusal::Fixed(
        "http is fixed until this process restarts: it is bound to 127.0.0.1:8080".to_owned(),
    ));

    let response = reqwest::Client::new()
        .patch(format!("{address}/v1/config"))
        .json(&serde_json::json!({"http": {"host": "0.0.0.0", "port": 9090}}))
        .send()
        .await
        .expect("patching the config");

    assert_eq!(response.status().as_u16(), 409);
    let body: ApiError = response.json().await.expect("parsing the error");
    assert_eq!(body.code, Some(ErrorCode::Conflict));
    // Not a write rejection. The two 409s in this API are told apart by this
    // field, so a client that switches on it must not find one here.
    assert_eq!(body.rejection, None);
    assert!(body.error.contains("restart"), "got: {}", body.error);
}

#[tokio::test]
async fn a_reload_asks_the_port_and_answers_with_the_settings() {
    let (address, config) = accepting();

    let response = reqwest::Client::new()
        .post(format!("{address}/v1/config/reload"))
        .send()
        .await
        .expect("reloading the config");

    assert_eq!(response.status().as_u16(), 200);
    let body: ConfigDocument = response.json().await.expect("parsing the document");
    assert_eq!(body, settings());
    assert_eq!(config.reloads(), 1);
}

#[tokio::test]
async fn a_reload_takes_no_body_and_is_idempotent() {
    // Both properties a watcher depends on: it has nothing to say beyond "the
    // file changed", and it may say so on every write to a mounted volume —
    // which for a ConfigMap is more than once per actual change.
    let (address, config) = accepting();
    let client = reqwest::Client::new();

    for _ in 0..3 {
        let status = client
            .post(format!("{address}/v1/config/reload"))
            .send()
            .await
            .expect("reloading the config")
            .status()
            .as_u16();
        assert_eq!(status, 200);
    }

    assert_eq!(config.reloads(), 3);
}

#[tokio::test]
async fn a_config_file_that_will_not_load_is_ours() {
    // The one refusal the caller could not have avoided: the request was fine
    // and the file on disk is not. A 500 rather than a 4xx, and the server keeps
    // running under the settings it had.
    let (address, _config) = refusing(ConfigRefusal::Source(
        "reading /etc/sismatic/configuration.yaml: unknown field `sink`".to_owned(),
    ));

    let response = reqwest::Client::new()
        .post(format!("{address}/v1/config/reload"))
        .send()
        .await
        .expect("reloading the config");

    assert_eq!(response.status().as_u16(), 500);
    let body: ApiError = response.json().await.expect("parsing the error");
    assert_eq!(body.code, Some(ErrorCode::Internal));
    // The path and the parse error both survive: between them they are the whole
    // of what an operator needs to fix it.
    assert!(body.error.contains("configuration.yaml") && body.error.contains("sink"));
}

#[tokio::test]
async fn the_wrong_verb_on_the_right_path_says_what_is_allowed() {
    // Two methods on one resource, which is what makes this a 405 with an
    // `Allow` header rather than a 404. A caller reaching for `PUT` — the
    // obvious guess for a settings endpoint — is told what to send instead.
    let (address, config) = accepting();

    let response = reqwest::Client::new()
        .put(format!("{address}/v1/config"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("putting the config");

    assert_eq!(response.status().as_u16(), 405);
    let allow = response
        .headers()
        .get("allow")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        allow.contains("GET") && allow.contains("PATCH"),
        "expected both verbs in the Allow header, got: {allow:?}"
    );
    assert!(config.applied().is_empty());
}

#[tokio::test]
async fn the_document_a_get_returns_is_a_patch_a_patch_accepts() {
    // The round trip, end to end over HTTP. The types promise it and
    // `sismatic-server` pins that applying it changes nothing; what this adds is
    // that the *bytes* survive the journey — a caller can fetch, edit one
    // number, and send the whole thing back.
    let (address, config) = accepting();
    let client = reqwest::Client::new();

    let document: serde_json::Value = client
        .get(format!("{address}/v1/config"))
        .send()
        .await
        .expect("requesting the config")
        .json()
        .await
        .expect("parsing the document");

    let status = client
        .patch(format!("{address}/v1/config"))
        .json(&document)
        .send()
        .await
        .expect("patching the config")
        .status()
        .as_u16();

    assert_eq!(status, 200, "a document should be an acceptable patch");
    let applied = config.applied();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0], settings().as_patch());
}
