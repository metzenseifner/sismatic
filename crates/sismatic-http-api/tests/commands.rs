//! tests/commands.rs — the write routes, black-box over a real socket.
//!
//! Black-box like the other suites, and over the real [`MemoryOutbox`] rather
//! than a double, for the reason `tests/readings.rs` gives for using the real
//! `MemoryStore`: a double would have to restate the admission table and the
//! epoch rules, and a handler tested against a drifted double passes while the
//! server is wrong.
//!
//! # What is asserted here, and what is not
//!
//! This file is about the HTTP surface: status codes, headers, bodies, and the
//! translation from a URL to an [`Intent`]. Whether the freeze rule is *right*
//! is `sismatic-store`'s admission-table test, and whether the outbox enforces
//! it atomically is `sismatic-store-memory`'s. What is left over — and only
//! testable from out here — is that a `PUT` on the metadata path builds a
//! `SetMetadata` and not a `SetSetting`, that a refusal becomes a `409` and not
//! a `500`, and that the `202` names a command a caller can then fetch.
//!
//! Nothing here reaches a device: there is no relay in this process, so every
//! submitted command stays `pending` forever. That is the point — the `202`
//! means recorded, and this suite is what pins that it means only that.

use std::sync::Arc;

use sismatic_api_types::{Intent, Phase};
use sismatic_store::DynReadStore;
use sismatic_store_memory::{MemoryOutbox, MemoryStore};

mod harness;

const DEVICE: &str = "atrium-101";

/// Start the application over an empty read store; return the base URL and the
/// outbox behind the write routes.
fn spawn_app() -> (String, MemoryOutbox) {
    let store: DynReadStore = Arc::new(MemoryStore::default());
    harness::spawn(store)
}

fn url(path: &str) -> String {
    format!("/v1/devices/{DEVICE}{path}")
}

/// `POST base+path`, returning the status, the `Location` header and the body.
async fn post(base: &str, path: &str) -> (u16, Option<String>, serde_json::Value) {
    send(reqwest::Client::new().post(format!("{base}{path}"))).await
}

/// `PUT base+path` with a `{"value": ...}` body.
async fn put(base: &str, path: &str, value: &str) -> (u16, Option<String>, serde_json::Value) {
    send(
        reqwest::Client::new()
            .put(format!("{base}{path}"))
            .json(&serde_json::json!({ "value": value })),
    )
    .await
}

async fn get(base: &str, path: &str) -> (u16, serde_json::Value) {
    let (status, _, body) = send(reqwest::Client::new().get(format!("{base}{path}"))).await;
    (status, body)
}

async fn send(request: reqwest::RequestBuilder) -> (u16, Option<String>, serde_json::Value) {
    let response = request.send().await.expect("issuing the request");
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // Every route here answers JSON, including the failures — an empty body
    // would itself be the bug.
    let body = response.json().await.expect("parsing the response body");
    (status, location, body)
}

// ---- accepting a write -------------------------------------------------

#[tokio::test]
async fn a_start_is_accepted_and_names_the_command_it_recorded() {
    let (address, _outbox) = spawn_app();

    let (status, location, body) = post(&address, &url("/recording/start")).await;

    assert_eq!(status, 202, "got {body}");
    assert_eq!(body["id"], "cmd-1");
    // The first recording, so epoch 1. Returned in the body so a caller writing
    // several metadata fields can check they all landed on one take.
    assert_eq!(body["epoch"], 1);
    // The header and the body must name the same command; with a UUID a test
    // could only check the header was *shaped* like one.
    assert_eq!(location.as_deref(), Some("/v1/commands/cmd-1"));
}

#[tokio::test]
async fn an_accepted_command_is_pending_and_no_device_was_contacted() {
    let (address, _outbox) = spawn_app();

    post(&address, &url("/recording/start")).await;
    let (status, body) = get(&address, "/v1/commands/cmd-1").await;

    assert_eq!(status, 200);
    // `pending`, not `succeeded`: there is no relay in this process. The 202
    // said the request was recorded, and that is all it said.
    assert_eq!(body["status"]["state"], "pending");
    assert_eq!(body["intent"]["kind"], "start_recording");
    assert_eq!(body["device"], DEVICE);
    assert_eq!(body["attempts"], 0);
}

/// The URL decides which intent is built, which is the whole reason the write
/// surface is five routes and not one polymorphic endpoint.
#[tokio::test]
async fn the_path_decides_the_intent() {
    let (address, outbox) = spawn_app();

    put(&address, &url("/metadata/title"), "Week 4").await;
    put(&address, &url("/settings/timezone"), "UTC").await;
    post(&address, &url("/recording/start")).await;
    post(&address, &url("/recording/pause")).await;
    post(&address, &url("/recording/stop")).await;

    let intents = recorded_intents(&outbox).await;
    assert_eq!(
        intents,
        vec![
            // Normalized to the canonical spelling the catalogs use, so a
            // lower-case URL and an upper-case one name one register.
            Intent::SetMetadata {
                field: "TITLE".to_owned(),
                value: "Week 4".to_owned(),
            },
            Intent::SetSetting {
                field: "TIMEZONE".to_owned(),
                value: "UTC".to_owned(),
            },
            Intent::StartRecording,
            Intent::PauseRecording,
            Intent::StopRecording,
        ]
    );
}

#[tokio::test]
async fn a_dashed_field_names_the_same_register_as_an_underscored_one() {
    let (address, outbox) = spawn_app();

    put(&address, &url("/metadata/system-name"), "atrium").await;

    assert_eq!(
        recorded_intents(&outbox).await,
        vec![Intent::SetMetadata {
            field: "SYSTEM_NAME".to_owned(),
            value: "atrium".to_owned(),
        }]
    );
}

// ---- refusing a write --------------------------------------------------

/// The requirement, through the HTTP surface: metadata is writable exactly
/// while nothing is recording.
#[tokio::test]
async fn a_metadata_write_during_a_recording_is_a_409() {
    let (address, _outbox) = spawn_app();

    let (before, ..) = put(&address, &url("/metadata/title"), "Week 4").await;
    assert_eq!(before, 202, "metadata is writable while idle");

    post(&address, &url("/recording/start")).await;

    let (status, location, body) = put(&address, &url("/metadata/title"), "oops").await;
    assert_eq!(status, 409);
    // A conflict is not a command, so there is nothing to poll for.
    assert_eq!(location, None);
    assert_eq!(body["code"], "conflict");
    // The message carries which rejection applied and the phase that produced
    // it — `ApiError` has one `code` slot, so this is where a caller learns
    // `metadata_frozen` rather than, say, `already_recording`.
    let message = body["error"].as_str().expect("an error message");
    assert!(
        message.contains("metadata_frozen") && message.contains("recording"),
        "got: {message}"
    );
}

#[tokio::test]
async fn a_setting_is_writable_during_a_recording() {
    let (address, _outbox) = spawn_app();
    post(&address, &url("/recording/start")).await;

    let (status, _, body) = put(&address, &url("/settings/timezone"), "UTC").await;

    // The freeze is metadata's alone: a device's configuration is not part of
    // the take in progress.
    assert_eq!(status, 202, "got {body}");
}

#[tokio::test]
async fn each_lifecycle_verb_is_refused_by_the_state_that_contradicts_it() {
    let (address, _outbox) = spawn_app();

    // Nothing is running.
    for path in ["/recording/stop", "/recording/pause"] {
        let (status, _, body) = post(&address, &url(path)).await;
        assert_eq!(status, 409, "{path} against an idle device: {body}");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|m| m.contains("not_recording")),
            "{path}: {}",
            body["error"]
        );
    }

    post(&address, &url("/recording/start")).await;
    let (status, _, body) = post(&address, &url("/recording/start")).await;
    assert_eq!(status, 409);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|m| m.contains("already_recording")),
        "{}",
        body["error"]
    );

    post(&address, &url("/recording/pause")).await;
    let (status, _, body) = post(&address, &url("/recording/pause")).await;
    assert_eq!(status, 409);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|m| m.contains("already_paused")),
        "{}",
        body["error"]
    );
}

/// A refused submission must leave no trace. A 409 that still queued the
/// command would dispatch the very write it just told the caller it refused.
#[tokio::test]
async fn a_refused_write_records_nothing() {
    let (address, outbox) = spawn_app();
    post(&address, &url("/recording/start")).await;

    put(&address, &url("/metadata/title"), "oops").await;

    assert_eq!(
        recorded_intents(&outbox).await,
        vec![Intent::StartRecording],
        "the refused write must not be in the log"
    );
}

// ---- idempotency -------------------------------------------------------

#[tokio::test]
async fn a_retry_under_one_key_returns_the_original_command() {
    let (address, outbox) = spawn_app();
    let client = reqwest::Client::new();
    let start = || {
        client
            .post(format!("{address}{}", url("/recording/start")))
            .header("Idempotency-Key", "take-4")
            .send()
    };

    let first: serde_json::Value = start().await.expect("first").json().await.expect("body");
    // Without deduplication this is a 409 `already_recording` for a command the
    // client believes never landed — the failure the header exists to prevent.
    let response = start().await.expect("retry");
    assert_eq!(response.status().as_u16(), 202);
    let second: serde_json::Value = response.json().await.expect("body");

    assert_eq!(first, second);
    assert_eq!(
        recorded_intents(&outbox).await,
        vec![Intent::StartRecording]
    );
}

#[tokio::test]
async fn two_writes_without_a_key_are_two_commands() {
    let (address, _outbox) = spawn_app();

    let (_, _, first) = put(&address, &url("/metadata/title"), "one").await;
    let (_, _, second) = put(&address, &url("/metadata/title"), "two").await;

    assert_eq!(first["id"], "cmd-1");
    assert_eq!(second["id"], "cmd-2");
}

// ---- reading the write side -------------------------------------------

#[tokio::test]
async fn the_phase_route_reports_what_the_write_side_accepted() {
    let (address, _outbox) = spawn_app();

    let (status, body) = get(&address, &url("/recording")).await;
    assert_eq!(status, 200);
    // An unknown device is idle at epoch 0: this port holds what was submitted
    // and no catalog of what exists.
    assert_eq!(body, serde_json::json!({"phase": "idle", "epoch": 0}));

    post(&address, &url("/recording/start")).await;
    let (_, body) = get(&address, &url("/recording")).await;
    // Moved by the *acceptance*, before any device was contacted. This is the
    // write side's belief, not the device's last word — that is
    // `fields/RUNNING_STATE`.
    assert_eq!(body, serde_json::json!({"phase": "recording", "epoch": 1}));
}

#[tokio::test]
async fn the_command_list_is_newest_first_and_scoped_to_one_device() {
    let (address, _outbox) = spawn_app();

    put(&address, &url("/metadata/title"), "one").await;
    put(&address, &url("/metadata/presenter"), "two").await;
    reqwest::Client::new()
        .put(format!("{address}/v1/devices/elsewhere/metadata/title"))
        .json(&serde_json::json!({"value": "other"}))
        .send()
        .await
        .expect("a write to another device");

    let (status, body) = get(&address, &url("/commands")).await;
    assert_eq!(status, 200);

    let ids: Vec<&str> = body["commands"]
        .as_array()
        .expect("a commands array")
        .iter()
        .map(|c| c["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids, ["cmd-2", "cmd-1"]);
}

#[tokio::test]
async fn an_unknown_command_is_a_404() {
    let (address, _outbox) = spawn_app();

    let (status, body) = get(&address, "/v1/commands/never-minted").await;

    assert_eq!(status, 404);
    // Unlike the readings routes' 404, this one *is* a claim about existence:
    // an id is only ever minted by an accepted submission.
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn an_unknown_device_has_an_empty_command_list() {
    let (address, _outbox) = spawn_app();

    let (status, body) = get(&address, "/v1/devices/nobody/commands").await;

    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({"commands": []}));
}

// ---- method and body discipline ---------------------------------------

#[tokio::test]
async fn a_write_route_refuses_the_wrong_method() {
    let (address, _outbox) = spawn_app();
    let client = reqwest::Client::new();

    // 405 rather than 404: the path exists and the method does not, which is
    // the answer that tells a misconfigured client what to change. Same
    // reasoning as the readings routes' method guard.
    let response = client
        .get(format!("{address}{}", url("/recording/start")))
        .send()
        .await
        .expect("issuing the request");
    assert_eq!(response.status().as_u16(), 405);

    let response = client
        .post(format!("{address}{}", url("/metadata/title")))
        .json(&serde_json::json!({"value": "x"}))
        .send()
        .await
        .expect("issuing the request");
    assert_eq!(response.status().as_u16(), 405);
}

#[tokio::test]
async fn a_write_without_a_value_is_refused() {
    let (address, outbox) = spawn_app();

    let response = reqwest::Client::new()
        .put(format!("{address}{}", url("/metadata/title")))
        .json(&serde_json::json!({"note": "wrong field"}))
        .send()
        .await
        .expect("issuing the request");

    // The extractor refuses it, so no intent is built and nothing is recorded —
    // a malformed body must not become a command that fails at a device later.
    assert_eq!(response.status().as_u16(), 400);
    assert!(recorded_intents(&outbox).await.is_empty());
}

/// Every intent the outbox holds for [`DEVICE`], oldest first.
///
/// Reads the port directly rather than the `commands` route: this is the
/// assertion about *what was recorded*, and routing it through a second handler
/// would make a failure ambiguous between the two.
async fn recorded_intents(outbox: &MemoryOutbox) -> Vec<Intent> {
    use sismatic_store::outbox::CommandLog;
    let mut commands = outbox
        .commands_for(DEVICE.to_owned())
        .await
        .expect("reading the log");
    commands.reverse(); // the port promises newest-first
    commands.into_iter().map(|c| c.intent).collect()
}

/// Guards the phase the assertions above are written against — a suite that
/// silently started from `Recording` would invert half of them.
#[tokio::test]
async fn a_fresh_outbox_starts_idle() {
    let (_address, outbox) = spawn_app();
    use sismatic_store::outbox::CommandLog;
    assert_eq!(
        outbox.phase(DEVICE.to_owned()).await.expect("phase").phase,
        Phase::Idle
    );
}
