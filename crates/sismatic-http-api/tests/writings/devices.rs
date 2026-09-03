//! `/v1/writings/devices/{id}/…` — asking one device to do something, and
//! reading back what it was asked.
//!
//! Also the scope-root `/v1/writings/{id}`, which belongs to neither id-space:
//! a writing id is globally unique, so the route that fetches one needs no
//! device to address it. It is tested here because every id it is given is
//! minted by a device-addressed submission above.

use sismatic_api_types::{Intent, Phase};

use crate::{DEVICE, GROUP, SCOPE, get, post, put, recorded_intents, spawn_app};

/// A device-addressed path, relative to the scope.
fn url(path: &str) -> String {
    format!("/devices/{DEVICE}{path}")
}

// ---- accepting a write -------------------------------------------------

#[tokio::test]
async fn a_start_is_accepted_and_names_the_writing_it_recorded() {
    let (address, _outbox) = spawn_app();

    let (status, location, body) = post(&address, &url("/recording/start")).await;

    assert_eq!(status, 202, "got {body}");
    // Always a list, even for a device that can only ever produce one writing:
    // one response shape rather than one per kind of id. See `Acceptance`.
    assert_eq!(body["batch"], serde_json::Value::Null);
    assert_eq!(body["writings"][0]["id"], "cmd-1");
    assert_eq!(body["writings"][0]["device"], DEVICE);
    // The first recording, so epoch 1. Returned in the body so a caller writing
    // several metadata fields can check they all landed on one take.
    assert_eq!(body["writings"][0]["epoch"], 1);
    // The header and the body must name the same writing; with a UUID a test
    // could only check the header was *shaped* like one. It names the scope
    // root, which is where `read_writing` is mounted.
    assert_eq!(location.as_deref(), Some("/v1/writings/cmd-1"));
}

#[tokio::test]
async fn an_accepted_writing_is_pending_and_no_device_was_contacted() {
    let (address, _outbox) = spawn_app();

    post(&address, &url("/recording/start")).await;
    let (status, body) = get(&address, "/cmd-1").await;

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

    let intents = recorded_intents(&outbox, DEVICE).await;
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
        recorded_intents(&outbox, DEVICE).await,
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
    // A conflict is not a writing, so there is nothing to poll for.
    assert_eq!(location, None);
    assert_eq!(body["code"], "conflict");
    // The typed field is how a caller tells "your edit was discarded" from "the
    // device is already doing what you asked" — the two call for very different
    // handling, and before this field existed telling them apart meant matching
    // prose.
    assert_eq!(body["rejection"], "metadata_frozen");
    // The prose still says it too, because that is what lands in a log.
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
        assert_eq!(body["rejection"], "not_recording", "{path}: {body}");
    }

    post(&address, &url("/recording/start")).await;
    let (status, _, body) = post(&address, &url("/recording/start")).await;
    assert_eq!(status, 409);
    assert_eq!(body["rejection"], "already_recording", "{body}");

    post(&address, &url("/recording/pause")).await;
    let (status, _, body) = post(&address, &url("/recording/pause")).await;
    assert_eq!(status, 409);
    assert_eq!(body["rejection"], "already_paused", "{body}");
}

/// A refused submission must leave no trace. A 409 that still queued the
/// writing would dispatch the very write it just told the caller it refused.
#[tokio::test]
async fn a_refused_write_records_nothing() {
    let (address, outbox) = spawn_app();
    post(&address, &url("/recording/start")).await;

    put(&address, &url("/metadata/title"), "oops").await;

    assert_eq!(
        recorded_intents(&outbox, DEVICE).await,
        vec![Intent::StartRecording],
        "the refused write must not be in the log"
    );
}

// ---- an id the catalog does not hold -----------------------------------

/// The failure the catalog exists to prevent. Without it the outbox admits the
/// writing against a fresh idle phase and answers `202`, and the caller learns
/// its recording never started by polling a writing that fails at dispatch.
#[tokio::test]
async fn a_write_to_an_unconfigured_device_is_refused_at_submission() {
    let (address, outbox) = spawn_app();

    let (status, _, body) = post(&address, "/devices/typo/recording/start").await;

    assert_eq!(status, 404);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|m| m.contains("no device 'typo'")),
        "got: {}",
        body["error"]
    );

    // Nothing recorded: a refused submission must leave no writing behind, or
    // the relay would dispatch the very write the caller was told was refused.
    assert!(recorded_intents(&outbox, "typo").await.is_empty());
}

/// Every write route is guarded, not just the one. They share a `submit`, but
/// a future refactor could give one its own path.
#[tokio::test]
async fn every_write_route_refuses_an_unconfigured_target() {
    let (address, _outbox) = spawn_app();

    for path in ["/recording/start", "/recording/stop", "/recording/pause"] {
        let (status, ..) = post(&address, &format!("/devices/typo{path}")).await;
        assert_eq!(status, 404, "POST {path} was not guarded");
    }

    for path in ["/metadata/title", "/settings/timezone"] {
        let (status, ..) = put(&address, &format!("/devices/typo{path}"), "x").await;
        assert_eq!(status, 404, "PUT {path} was not guarded");
    }
}

/// The mirror of `groups::a_device_id_on_a_group_write_route_is_refused`, stated
/// once over the whole `/devices` half of this scope: a device group id is
/// refused everywhere in it, and every refusal names the `/groups` URL that
/// answers the same question.
///
/// One test over the half rather than one per route, because the property is
/// about the half: a route that gained a group-id path back would be a hole in
/// it, and a per-route test would not notice the route it does not cover.
#[tokio::test]
async fn every_device_route_in_this_scope_refuses_a_device_group_id() {
    let (address, _outbox) = spawn_app();

    for (method, tail) in [
        ("get", "/recording"),
        ("get", "/history"),
        ("post", "/recording/start"),
        ("post", "/recording/stop"),
        ("post", "/recording/pause"),
        ("put", "/metadata/TITLE"),
        ("put", "/settings/TIMEZONE"),
    ] {
        let path = format!("/devices/{GROUP}{tail}");
        let (status, body) = match method {
            "get" => get(&address, &path).await,
            "post" => {
                let (status, _, body) = post(&address, &path).await;
                (status, body)
            }
            _ => {
                let (status, _, body) = put(&address, &path, "x").await;
                (status, body)
            }
        };

        assert_eq!(status, 404, "{method} {path} answered {status}: {body}");
        assert_eq!(body["code"], "not_found", "for {method} {path}");
        let message = body["error"].as_str().expect("error");
        assert!(
            message.contains("is a device group"),
            "for {method} {path}: {message}"
        );
        assert!(
            message.contains(&format!("{SCOPE}/groups/{GROUP}{tail}")),
            "for {method} {path} the refusal should name \
             {SCOPE}/groups/{GROUP}{tail}, got {message}"
        );
    }
}

// ---- idempotency -------------------------------------------------------

#[tokio::test]
async fn a_retry_under_one_key_returns_the_original_writing() {
    let (address, outbox) = spawn_app();
    let client = reqwest::Client::new();
    let start = || {
        client
            .post(format!("{address}{SCOPE}{}", url("/recording/start")))
            .header("Idempotency-Key", "take-4")
            .send()
    };

    let first: serde_json::Value = start().await.expect("first").json().await.expect("body");
    // Without deduplication this is a 409 `already_recording` for a writing the
    // client believes never landed — the failure the header exists to prevent.
    let response = start().await.expect("retry");
    assert_eq!(response.status().as_u16(), 202);
    let second: serde_json::Value = response.json().await.expect("body");

    assert_eq!(first, second);
    assert_eq!(
        recorded_intents(&outbox, DEVICE).await,
        vec![Intent::StartRecording]
    );
}

#[tokio::test]
async fn two_writes_without_a_key_are_two_writings() {
    let (address, _outbox) = spawn_app();

    let (_, _, first) = put(&address, &url("/metadata/title"), "one").await;
    let (_, _, second) = put(&address, &url("/metadata/title"), "two").await;

    assert_eq!(first["writings"][0]["id"], "cmd-1");
    assert_eq!(second["writings"][0]["id"], "cmd-2");
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
    // `/v1/readings/devices/{id}/fields/RUNNING_STATE`.
    assert_eq!(body, serde_json::json!({"phase": "recording", "epoch": 1}));
}

#[tokio::test]
async fn the_writing_list_is_newest_first_and_scoped_to_one_device() {
    let (address, _outbox) = spawn_app();

    put(&address, &url("/metadata/title"), "one").await;
    put(&address, &url("/metadata/presenter"), "two").await;
    put(&address, "/devices/elsewhere/metadata/title", "other").await;

    let (status, body) = get(&address, &url("/history")).await;
    assert_eq!(status, 200);

    let ids: Vec<&str> = body["writings"]
        .as_array()
        .expect("a writings array")
        .iter()
        .map(|c| c["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids, ["cmd-2", "cmd-1"]);
}

#[tokio::test]
async fn an_unknown_writing_is_a_404() {
    let (address, _outbox) = spawn_app();

    let (status, body) = get(&address, "/never-minted").await;

    assert_eq!(status, 404);
    // Unlike the readings routes' 404, this one *is* a claim about existence:
    // an id is only ever minted by an accepted submission.
    assert_eq!(body["code"], "not_found");
    // The failure shape every other route returns is unchanged by the write
    // side's extra field: `rejection` is skipped when absent, so a body that
    // has no rejection serializes exactly as it did before the field existed.
    assert_eq!(
        body.as_object()
            .expect("an object")
            .keys()
            .collect::<Vec<_>>(),
        ["code", "error"],
        "an error with no rejection must not carry the key: {body}"
    );
}

#[tokio::test]
async fn an_unknown_device_has_an_empty_writing_list() {
    let (address, _outbox) = spawn_app();

    let (status, body) = get(&address, "/devices/nobody/history").await;

    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({"writings": []}));
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
        .get(format!("{address}{SCOPE}{}", url("/recording/start")))
        .send()
        .await
        .expect("issuing the request");
    assert_eq!(response.status().as_u16(), 405);

    let response = client
        .post(format!("{address}{SCOPE}{}", url("/metadata/title")))
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
        .put(format!("{address}{SCOPE}{}", url("/metadata/title")))
        .json(&serde_json::json!({"note": "wrong field"}))
        .send()
        .await
        .expect("issuing the request");

    // The extractor refuses it, so no intent is built and nothing is recorded —
    // a malformed body must not become a writing that fails at a device later.
    assert_eq!(response.status().as_u16(), 400);
    assert!(recorded_intents(&outbox, DEVICE).await.is_empty());
}

/// Guards the phase the assertions above are written against — a suite that
/// silently started from `Recording` would invert half of them.
#[tokio::test]
async fn a_fresh_outbox_starts_idle() {
    let (_address, outbox) = spawn_app();
    use sismatic_store::outbox::WritingLog;
    assert_eq!(
        outbox.phase(DEVICE.to_owned()).await.expect("phase").phase,
        Phase::Idle
    );
}
