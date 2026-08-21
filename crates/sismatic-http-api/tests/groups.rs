//! tests/groups.rs — reading a device group as a client meets it.
//!
//! Black-box like the other suites: the real server on an ephemeral port, the
//! real [`MemoryStore`] under it, and the real [`MemoryOutbox`] behind both the
//! write routes and the group-state read. What is pinned is the JSON on the
//! wire.
//!
//! The outbox is not a double here for a reason specific to this suite. The
//! rule these routes depend on — *an expectation exists exactly when a group
//! request was admitted* — lives inside the outbox's admission critical
//! section. A stub that recorded expectations on demand would let a test assert
//! drift detection over a state the server can never actually be in, which is
//! the failure mode a double is worst at.
//!
//! So every expectation below is created the way a client creates one: by
//! POSTing to a group write route.

use std::net::TcpListener;
use std::sync::Arc;

use sismatic_api_types::{Reading, ReadingValue, RecordingState, Timestamp};
use sismatic_store::{DynReadStore, WriteStore};
use sismatic_store_memory::MemoryStore;

mod harness;

const ATRIUM: &str = "atrium";
const ANNEX: &str = "annex";
const GROUP: &str = harness::GROUP;
const AT: &str = "2026-08-17T00:00:00.000Z";

/// A `RUNNING_STATE` reading for `device`.
fn state(device: &str, value: RecordingState) -> Reading {
    reading(device, "RUNNING_STATE", ReadingValue::State(value))
}

fn reading(device: &str, field: &str, value: ReadingValue) -> Reading {
    Reading {
        device: device.into(),
        field: field.into(),
        value,
        at: Timestamp(AT.into()),
    }
}

/// Start the application over a device group of `members` and a store
/// pre-loaded with `readings`; return its base URL.
///
/// Members are written `[atrium, annex]` — deliberately *not* alphabetical, so
/// a response that came back sorted rather than in configured order fails
/// visibly instead of passing by coincidence.
async fn spawn(readings: impl IntoIterator<Item = Reading>) -> String {
    let store = MemoryStore::default();
    for r in readings {
        store.upsert_latest(r).await.expect("seeding the store");
    }
    let store: DynReadStore = Arc::new(store);

    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    harness::serve_with(listener, store, harness::device_group(&[ATRIUM, ANNEX]));

    format!("http://127.0.0.1:{port}")
}

async fn get(url: &str) -> reqwest::Response {
    reqwest::get(url).await.expect("requesting the server")
}

/// `GET` a path and parse its body, asserting the status first so a failure
/// reports the code rather than a confusing parse error.
async fn get_json(address: &str, path: &str) -> serde_json::Value {
    let response = get(&format!("{address}{path}")).await;
    let status = response.status().as_u16();
    let body: serde_json::Value = response.json().await.expect("parsing the body as JSON");
    assert_eq!(status, 200, "{path} answered {status}: {body}");
    body
}

/// Ask the device group to start recording, the way a client does.
async fn start_the_device_group(address: &str) {
    let status = reqwest::Client::new()
        .post(format!("{address}/v1/groups/{GROUP}/recording/start"))
        .send()
        .await
        .expect("starting the device group")
        .status()
        .as_u16();
    assert_eq!(
        status, 202,
        "the device group should have been asked to start"
    );
}

/// Each member's `sync` verdict, in the order the response listed them.
fn member_syncs(field: &serde_json::Value) -> Vec<(&str, &str)> {
    field["members"]
        .as_array()
        .expect("members is an array")
        .iter()
        .map(|m| {
            (
                m["device"].as_str().expect("device"),
                m["sync"].as_str().expect("sync"),
            )
        })
        .collect()
}

// ---- the shape of an answer ----------------------------------------------

#[tokio::test]
async fn a_field_route_lists_every_member_in_configured_order() {
    let address = spawn([
        state(ATRIUM, RecordingState::Started),
        state(ANNEX, RecordingState::Started),
    ])
    .await;

    let body = get_json(
        &address,
        &format!("/v1/groups/{GROUP}/fields/RUNNING_STATE"),
    )
    .await;

    assert_eq!(body["group"], GROUP);
    assert_eq!(body["field"], "RUNNING_STATE");
    // Configured order, not sorted: `annex` sorts first and is listed second.
    assert_eq!(
        member_syncs(&body)
            .iter()
            .map(|(device, _)| *device)
            .collect::<Vec<_>>(),
        [ATRIUM, ANNEX]
    );
    // Each member carries its own reading, whole — the same object the device
    // readings route would have served.
    assert_eq!(body["members"][0]["reading"]["device"], ATRIUM);
    assert_eq!(body["members"][0]["reading"]["value"]["type"], "state");
    assert_eq!(body["members"][0]["reading"]["value"]["value"], "started");
}

/// A member that has never reported is listed with a `null` reading rather than
/// dropped: which member went quiet is the answer, and a five-member device
/// group with one silent member must not render as a four-member device group.
#[tokio::test]
async fn a_silent_member_is_listed_rather_than_omitted() {
    let address = spawn([state(ATRIUM, RecordingState::Started)]).await;

    let body = get_json(
        &address,
        &format!("/v1/groups/{GROUP}/fields/RUNNING_STATE"),
    )
    .await;

    assert_eq!(member_syncs(&body).len(), 2);
    assert!(body["members"][1]["reading"].is_null());
    assert_eq!(body["members"][1]["device"], ANNEX);
    assert_eq!(body["members"][1]["sync"], "unknown");
}

/// The group exists and its members are known, so "none of them has said
/// anything about this field" is a `200` that names them — the opposite of the
/// device route's `404`, and for the opposite reason: there the same response
/// would carry no information at all.
#[tokio::test]
async fn a_field_no_member_has_reported_is_an_answer_not_a_404() {
    let address = spawn([]).await;

    let body = get_json(&address, &format!("/v1/groups/{GROUP}/fields/TIMEZONE")).await;

    assert_eq!(body["sync"], "unknown");
    assert_eq!(
        member_syncs(&body),
        [(ATRIUM, "unknown"), (ANNEX, "unknown")]
    );
}

#[tokio::test]
async fn the_field_name_is_normalized_as_on_the_device_routes() {
    let address = spawn([state(ATRIUM, RecordingState::Started)]).await;

    for spelling in ["RUNNING_STATE", "running_state", "running-state"] {
        let body = get_json(&address, &format!("/v1/groups/{GROUP}/fields/{spelling}")).await;
        // The canonical spelling comes back whichever was asked for.
        assert_eq!(body["field"], "RUNNING_STATE", "for '{spelling}'");
        assert_eq!(body["members"][0]["reading"]["value"]["value"], "started");
    }
}

// ---- the two comparisons --------------------------------------------------

#[tokio::test]
async fn a_device_group_that_started_when_told_to_is_in_sync() {
    let address = spawn([
        state(ATRIUM, RecordingState::Started),
        state(ANNEX, RecordingState::Started),
    ])
    .await;
    start_the_device_group(&address).await;

    let body = get_json(
        &address,
        &format!("/v1/groups/{GROUP}/fields/RUNNING_STATE"),
    )
    .await;

    assert_eq!(body["expected"]["value"]["value"], "started");
    assert_eq!(body["expected"]["since"], harness::AT);
    assert_eq!(body["sync"], "in_sync");
    assert_eq!(body["uniform"], true);
}

/// The finding these routes exist for, in its most common shape: one recorder
/// in the device group did not start.
#[tokio::test]
async fn one_member_that_did_not_start_is_reported_as_drift_and_named() {
    let address = spawn([
        state(ATRIUM, RecordingState::Started),
        state(ANNEX, RecordingState::Stopped),
    ])
    .await;
    start_the_device_group(&address).await;

    let body = get_json(
        &address,
        &format!("/v1/groups/{GROUP}/fields/RUNNING_STATE"),
    )
    .await;

    assert_eq!(body["sync"], "drifted");
    assert_eq!(body["uniform"], false);
    assert_eq!(
        member_syncs(&body),
        [(ATRIUM, "in_sync"), (ANNEX, "drifted")],
        "the response must say which member drifted"
    );
}

/// The case member-versus-member comparison cannot see, and the whole reason
/// the expectation is stored: every recorder agrees with every other, and the
/// device group still is not doing what it was told.
#[tokio::test]
async fn a_device_group_that_uniformly_ignored_the_request_is_still_drift() {
    let address = spawn([
        state(ATRIUM, RecordingState::Stopped),
        state(ANNEX, RecordingState::Stopped),
    ])
    .await;
    start_the_device_group(&address).await;

    let body = get_json(
        &address,
        &format!("/v1/groups/{GROUP}/fields/RUNNING_STATE"),
    )
    .await;

    assert_eq!(
        body["uniform"], true,
        "the members do agree with each other"
    );
    assert_eq!(
        body["sync"], "drifted",
        "...and none of them agrees with what the device group was asked for"
    );
}

/// The case the expectation cannot see, and the reason `uniform` is reported
/// beside it: nobody ever commanded firmware, and the device group is still
/// wrong.
#[tokio::test]
async fn members_that_disagree_about_an_uncommanded_field_are_not_uniform() {
    let address = spawn([
        reading(ATRIUM, "FIRMWARE", ReadingValue::Version("2.11".into())),
        reading(ANNEX, "FIRMWARE", ReadingValue::Version("2.09".into())),
    ])
    .await;

    let body = get_json(&address, &format!("/v1/groups/{GROUP}/fields/FIRMWARE")).await;

    assert_eq!(body["uniform"], false);
    assert_eq!(
        body["sync"], "unknown",
        "nothing was asked, so there is nothing to agree with"
    );
    assert!(body["expected"].is_null());
}

/// A group that has never been commanded reports `unknown`, not `in_sync`:
/// agreement with nothing is not agreement.
#[tokio::test]
async fn an_uncommanded_group_is_unknown_rather_than_in_sync() {
    let address = spawn([
        state(ATRIUM, RecordingState::Started),
        state(ANNEX, RecordingState::Started),
    ])
    .await;

    let body = get_json(
        &address,
        &format!("/v1/groups/{GROUP}/fields/RUNNING_STATE"),
    )
    .await;

    assert!(body["expected"].is_null());
    assert_eq!(body["sync"], "unknown");
    assert_eq!(body["uniform"], true);
}

/// A metadata write carries the caller's text and the device echoes what it
/// stored, so the two agree — and the member that never took the write is the
/// one reported.
#[tokio::test]
async fn a_group_metadata_write_is_checked_against_what_the_members_echoed() {
    let address = spawn([
        reading(ATRIUM, "TITLE", ReadingValue::Text("Week 4".into())),
        reading(ANNEX, "TITLE", ReadingValue::Text("Week 3".into())),
    ])
    .await;
    let status = reqwest::Client::new()
        .put(format!("{address}/v1/groups/{GROUP}/metadata/title"))
        .json(&serde_json::json!({"value": "Week 4"}))
        .send()
        .await
        .expect("writing the title")
        .status()
        .as_u16();
    assert_eq!(status, 202);

    let body = get_json(&address, &format!("/v1/groups/{GROUP}/fields/TITLE")).await;

    assert_eq!(body["expected"]["value"]["value"], "Week 4");
    assert_eq!(
        member_syncs(&body),
        [(ATRIUM, "in_sync"), (ANNEX, "drifted")],
        "the recorder still on last week's title is the finding"
    );
}

/// A device-addressed write asks nothing of the device group, so it files no
/// expectation — otherwise starting one recorder would report every other
/// member of its group as drifted for doing nothing wrong.
#[tokio::test]
async fn a_device_addressed_write_does_not_speak_for_the_room() {
    let address = spawn([
        state(ATRIUM, RecordingState::Started),
        state(ANNEX, RecordingState::Stopped),
    ])
    .await;
    let status = reqwest::Client::new()
        .post(format!("{address}/v1/devices/{ATRIUM}/recording/start"))
        .send()
        .await
        .expect("starting one device")
        .status()
        .as_u16();
    assert_eq!(status, 202);

    let body = get_json(
        &address,
        &format!("/v1/groups/{GROUP}/fields/RUNNING_STATE"),
    )
    .await;

    assert!(body["expected"].is_null());
    assert_eq!(body["sync"], "unknown");
}

// ---- the index route ------------------------------------------------------

#[tokio::test]
async fn the_index_covers_every_field_any_member_reported_ordered_by_name() {
    let address = spawn([
        reading(ATRIUM, "FIRMWARE", ReadingValue::Version("2.11".into())),
        state(ATRIUM, RecordingState::Started),
        // Only `annex` has answered on this one; it still appears, with `atrium`
        // carrying a null reading.
        reading(ANNEX, "TIMEZONE", ReadingValue::Text("UTC".into())),
    ])
    .await;

    let body = get_json(&address, &format!("/v1/groups/{GROUP}/fields")).await;

    assert_eq!(body["group"], GROUP);
    let names: Vec<&str> = body["fields"]
        .as_array()
        .expect("fields is an array")
        .iter()
        .map(|f| f["field"].as_str().expect("field"))
        .collect();
    assert_eq!(names, ["FIRMWARE", "RUNNING_STATE", "TIMEZONE"]);

    let timezone = &body["fields"][2];
    assert_eq!(
        member_syncs(timezone)
            .iter()
            .map(|(device, _)| *device)
            .collect::<Vec<_>>(),
        [ATRIUM, ANNEX],
        "every member is listed on every field, reported or not"
    );
    assert!(timezone["members"][0]["reading"].is_null());
}

/// A field the device group was told to set but no member has answered on yet
/// is what a write that reached nobody looks like — so it has to appear in the
/// index even though the store holds nothing for it.
#[tokio::test]
async fn the_index_covers_a_commanded_field_no_member_has_reported() {
    let address = spawn([]).await;
    start_the_device_group(&address).await;

    let body = get_json(&address, &format!("/v1/groups/{GROUP}/fields")).await;

    let names: Vec<&str> = body["fields"]
        .as_array()
        .expect("fields is an array")
        .iter()
        .map(|f| f["field"].as_str().expect("field"))
        .collect();
    assert_eq!(names, ["RUNNING_STATE"]);
    assert_eq!(body["fields"][0]["expected"]["value"]["value"], "started");
    assert_eq!(body["fields"][0]["sync"], "unknown");
}

#[tokio::test]
async fn an_index_for_a_room_that_has_done_nothing_is_empty_not_an_error() {
    let address = spawn([]).await;

    let body = get_json(&address, &format!("/v1/groups/{GROUP}/fields")).await;

    assert_eq!(body["group"], GROUP);
    assert_eq!(body["fields"].as_array().expect("fields").len(), 0);
}

// ---- history --------------------------------------------------------------

#[tokio::test]
async fn history_is_one_series_per_member_oldest_first() {
    let store = MemoryStore::default();
    for (device, at, value) in [
        (ATRIUM, "2026-08-17T00:00:00Z", RecordingState::Stopped),
        (ATRIUM, "2026-08-17T00:01:00Z", RecordingState::Started),
        (ANNEX, "2026-08-17T00:01:00Z", RecordingState::Started),
    ] {
        store
            .upsert_latest(Reading {
                device: device.into(),
                field: "RUNNING_STATE".into(),
                value: ReadingValue::State(value),
                at: Timestamp(at.into()),
            })
            .await
            .expect("seeding the store");
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener.local_addr().expect("address").port();
    harness::serve_with(
        listener,
        Arc::new(store),
        harness::device_group(&[ATRIUM, ANNEX]),
    );
    let address = format!("http://127.0.0.1:{port}");

    let body = get_json(
        &address,
        &format!("/v1/groups/{GROUP}/fields/RUNNING_STATE/history"),
    )
    .await;

    assert_eq!(body["group"], GROUP);
    assert_eq!(body["field"], "RUNNING_STATE");
    assert_eq!(body["members"][0]["device"], ATRIUM);
    assert_eq!(
        body["members"][0]["readings"]
            .as_array()
            .expect("readings")
            .len(),
        2
    );
    // Oldest first, so the series plots forwards.
    assert_eq!(
        body["members"][0]["readings"][0]["value"]["value"],
        "stopped"
    );
    assert_eq!(
        body["members"][0]["readings"][1]["value"]["value"],
        "started"
    );
    // A member with one row is listed with one row, not merged into the first.
    assert_eq!(body["members"][1]["device"], ANNEX);
    assert_eq!(
        body["members"][1]["readings"]
            .as_array()
            .expect("readings")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_member_with_nothing_in_the_span_is_an_empty_series_not_an_omission() {
    let address = spawn([state(ATRIUM, RecordingState::Started)]).await;

    let body = get_json(
        &address,
        &format!("/v1/groups/{GROUP}/fields/RUNNING_STATE/history"),
    )
    .await;

    assert_eq!(body["members"].as_array().expect("members").len(), 2);
    assert_eq!(body["members"][1]["device"], ANNEX);
    assert_eq!(
        body["members"][1]["readings"]
            .as_array()
            .expect("readings")
            .len(),
        0
    );
}

/// `limit` is per member, not per response: a two-member device group asked for
/// one row each answers with two rows, one from each series.
#[tokio::test]
async fn the_limit_bounds_each_members_series_rather_than_the_response() {
    let store = MemoryStore::default();
    for device in [ATRIUM, ANNEX] {
        for (n, at) in ["2026-08-17T00:00:00Z", "2026-08-17T00:01:00Z"]
            .into_iter()
            .enumerate()
        {
            store
                .upsert_latest(Reading {
                    device: device.into(),
                    field: "PORT_TIMEOUT".into(),
                    value: ReadingValue::Number(n as u32),
                    at: Timestamp(at.into()),
                })
                .await
                .expect("seeding the store");
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener.local_addr().expect("address").port();
    harness::serve_with(
        listener,
        Arc::new(store),
        harness::device_group(&[ATRIUM, ANNEX]),
    );
    let address = format!("http://127.0.0.1:{port}");

    let body = get_json(
        &address,
        &format!("/v1/groups/{GROUP}/fields/PORT_TIMEOUT/history?limit=1"),
    )
    .await;

    for member in body["members"].as_array().expect("members") {
        let readings = member["readings"].as_array().expect("readings");
        assert_eq!(readings.len(), 1, "each member is limited on its own");
        // The most recent row, kept from the tail — so a limited plot is a plot
        // of the tail rather than of a truncated head.
        assert_eq!(readings[0]["value"]["value"], 1);
    }
}

#[tokio::test]
async fn the_span_filters_each_members_series() {
    let address = spawn([
        state(ATRIUM, RecordingState::Started),
        state(ANNEX, RecordingState::Started),
    ])
    .await;

    let body = get_json(
        &address,
        &format!(
            "/v1/groups/{GROUP}/fields/RUNNING_STATE/history\
             ?start=2020-01-01T00:00:00Z&end=2020-12-31T23:59:59Z"
        ),
    )
    .await;

    for member in body["members"].as_array().expect("members") {
        assert_eq!(member["readings"].as_array().expect("readings").len(), 0);
    }
}

/// The same contradiction the device history route refuses, refused the same
/// way: a caller must never be served a different field than the one it spelled
/// out in the path.
#[tokio::test]
async fn a_query_field_that_contradicts_the_path_is_a_400() {
    let address = spawn([]).await;

    let response = get(&format!(
        "{address}/v1/groups/{GROUP}/fields/RUNNING_STATE/history?field=FIRMWARE"
    ))
    .await;

    assert_eq!(response.status().as_u16(), 400);
    let body: serde_json::Value = response.json().await.expect("parsing the error body");
    assert_eq!(body["code"], "bad_instruction");
}

/// ...and one that *agrees* is redundant, not wrong.
#[tokio::test]
async fn a_query_field_that_agrees_with_the_path_is_accepted() {
    let address = spawn([]).await;

    let body = get_json(
        &address,
        &format!("/v1/groups/{GROUP}/fields/RUNNING_STATE/history?field=running-state"),
    )
    .await;

    assert_eq!(body["field"], "RUNNING_STATE");
}

// ---- an id that names nothing, or the wrong thing -------------------------

/// Unlike the device readings routes, these cannot answer without the catalog —
/// so an unknown id is a real claim about configuration rather than "nothing
/// stored".
#[tokio::test]
async fn an_unconfigured_group_is_a_404_on_every_group_route() {
    let address = spawn([]).await;

    for path in [
        "/v1/groups/typo/fields",
        "/v1/groups/typo/fields/RUNNING_STATE",
        "/v1/groups/typo/fields/RUNNING_STATE/history",
    ] {
        let response = get(&format!("{address}{path}")).await;
        assert_eq!(response.status().as_u16(), 404, "for {path}");
        let body: serde_json::Value = response.json().await.expect("parsing the error body");
        assert_eq!(body["code"], "not_found");
        assert!(
            body["error"].as_str().expect("error").contains("typo"),
            "the message should name the id, got {body}"
        );
    }
}

/// Devices and groups share one id namespace, so naming a device here is a
/// different mistake from naming nothing: the fix is a different URL, and
/// saying which saves a round trip. The same courtesy `/v1/groups/{id}` already
/// extends.
#[tokio::test]
async fn a_device_id_on_a_group_route_says_which_route_to_use() {
    let address = spawn([]).await;

    let response = get(&format!("{address}/v1/groups/{ATRIUM}/fields")).await;

    assert_eq!(response.status().as_u16(), 404);
    let body: serde_json::Value = response.json().await.expect("parsing the error body");
    let message = body["error"].as_str().expect("error");
    assert!(
        message.contains("is a device") && message.contains(&format!("/v1/devices/{ATRIUM}")),
        "got {message}"
    );
}

// ---- the write side -------------------------------------------------------

/// Every write verb is reachable under `/v1/groups` and expands across the
/// members, which is what makes the space worth having rather than a synonym.
#[tokio::test]
async fn every_write_verb_is_addressable_under_the_group_space() {
    let client = reqwest::Client::new();
    for (method, path) in [
        ("post", format!("/v1/groups/{GROUP}/recording/start")),
        ("post", format!("/v1/groups/{GROUP}/recording/stop")),
        ("put", format!("/v1/groups/{GROUP}/metadata/title")),
        ("put", format!("/v1/groups/{GROUP}/settings/timezone")),
    ] {
        // A fresh server per verb: `stop` needs an idle group to refuse and
        // `start` needs one to accept, and this suite is about routing and
        // expansion rather than about the admission table, which
        // `tests/commands.rs` already covers.
        let address = spawn([]).await;
        let url = format!("{address}{path}");
        let request = match method {
            "post" => client.post(&url),
            _ => client
                .put(&url)
                .json(&serde_json::json!({"value": "Week 4"})),
        };
        let response = request.send().await.expect("submitting");
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.expect("parsing the body");

        assert!(
            status == 202 || status == 409,
            "{method} {path} answered {status}: {body}"
        );
        if status == 202 {
            // One command per member, in configured order — the expansion the
            // device space performs for a group id, reached through a URL that
            // says what it is doing.
            let devices: Vec<&str> = body["commands"]
                .as_array()
                .expect("commands")
                .iter()
                .map(|c| c["device"].as_str().expect("device"))
                .collect();
            assert_eq!(devices, [ATRIUM, ANNEX], "for {method} {path}");
        }
    }
}

/// The three lifecycle verbs need the members to act together, so a group start
/// is expanded under a rendezvous and every row carries the batch.
#[tokio::test]
async fn a_group_start_is_batched_and_a_metadata_write_is_not() {
    let address = spawn([]).await;
    let client = reqwest::Client::new();

    // The title first, while both members are idle and metadata is writable —
    // otherwise the write is refused and the body is an `ApiError`, whose
    // missing `batch` would read as `null` and prove nothing.
    let titled = client
        .put(format!("{address}/v1/groups/{GROUP}/metadata/title"))
        .json(&serde_json::json!({"value": "Week 4"}))
        .send()
        .await
        .expect("writing the title");
    assert_eq!(titled.status().as_u16(), 202);
    let titled: serde_json::Value = titled.json().await.expect("parsing");
    // A write gains nothing from unison, so it is expanded without a barrier it
    // could only time out against.
    assert!(
        titled["batch"].is_null(),
        "a write needs no rendezvous, got {titled}"
    );
    assert_eq!(titled["commands"].as_array().expect("commands").len(), 2);

    let started = client
        .post(format!("{address}/v1/groups/{GROUP}/recording/start"))
        .send()
        .await
        .expect("starting");
    assert_eq!(started.status().as_u16(), 202);
    let started: serde_json::Value = started.json().await.expect("parsing");
    assert!(
        started["batch"].is_string(),
        "a lifecycle verb needs a rendezvous, got {started}"
    );
}

/// The check that makes the two spaces mean what they say. Without it a device
/// id here would start one recorder and answer `202`.
#[tokio::test]
async fn a_device_id_on_a_group_write_route_is_refused_with_the_device_url() {
    let address = spawn([]).await;

    let response = reqwest::Client::new()
        .post(format!("{address}/v1/groups/{ATRIUM}/recording/start"))
        .send()
        .await
        .expect("submitting");

    assert_eq!(response.status().as_u16(), 404);
    let body: serde_json::Value = response.json().await.expect("parsing the error body");
    let message = body["error"].as_str().expect("error");
    assert!(
        message.contains("is a device")
            && message.contains(&format!("/v1/devices/{ATRIUM}/recording/start")),
        "the message should name the route that would have worked, got {message}"
    );
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn an_unconfigured_group_is_a_404_on_every_group_write_route() {
    let address = spawn([]).await;
    let client = reqwest::Client::new();

    for (method, path) in [
        ("post", "/v1/groups/typo/recording/start"),
        ("post", "/v1/groups/typo/recording/stop"),
        ("post", "/v1/groups/typo/recording/pause"),
        ("put", "/v1/groups/typo/metadata/title"),
        ("put", "/v1/groups/typo/settings/timezone"),
        ("get", "/v1/groups/typo/recording"),
        ("get", "/v1/groups/typo/commands"),
    ] {
        let url = format!("{address}{path}");
        let request = match method {
            "post" => client.post(&url),
            "put" => client.put(&url).json(&serde_json::json!({"value": "x"})),
            _ => client.get(&url),
        };
        let status = request.send().await.expect("requesting").status().as_u16();
        assert_eq!(status, 404, "for {method} {path}");
    }
}

// ---- the two status reads the device space answers wrongly ----------------

/// The bug this route exists for. The outbox keys its logs by device, so
/// `GET /v1/devices/{group-id}/recording` used to report an idle device that
/// does not exist. It now refuses the id and names this route instead.
#[tokio::test]
async fn the_group_phase_route_reports_members_and_the_device_route_refuses_the_id() {
    let address = spawn([]).await;
    start_the_device_group(&address).await;

    // The device space no longer answers for a group id at all.
    let refused = get(&format!("{address}/v1/devices/{GROUP}/recording")).await;
    assert_eq!(refused.status().as_u16(), 404);
    let refusal: serde_json::Value = refused.json().await.expect("parsing the error body");
    assert!(
        refusal["error"]
            .as_str()
            .expect("error")
            .contains(&format!("/v1/groups/{GROUP}/recording")),
        "the refusal must name this route, got {refusal}"
    );

    // What the group route says.
    let body = get_json(&address, &format!("/v1/groups/{GROUP}/recording")).await;
    assert_eq!(body["group"], GROUP);
    assert_eq!(
        body["phase"], "recording",
        "every member was admitted, so they agree"
    );
    let members = body["members"].as_array().expect("members");
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["device"], ATRIUM);
    assert_eq!(members[0]["phase"], "recording");
    assert_eq!(
        members[0]["epoch"], 1,
        "each member opened its own first take"
    );
    assert_eq!(members[1]["device"], ANNEX);
}

/// A device group has no phase of its own, so `null` when the members have
/// diverged — which is what a start that reached only some of them looks like.
#[tokio::test]
async fn a_divided_group_reports_no_shared_phase() {
    let address = spawn([]).await;
    // One member started on its own, through the device space.
    let status = reqwest::Client::new()
        .post(format!("{address}/v1/devices/{ATRIUM}/recording/start"))
        .send()
        .await
        .expect("starting one member")
        .status()
        .as_u16();
    assert_eq!(status, 202);

    let body = get_json(&address, &format!("/v1/groups/{GROUP}/recording")).await;

    assert!(
        body["phase"].is_null(),
        "the members disagree, so there is no group phase: {body}"
    );
    assert_eq!(body["members"][0]["phase"], "recording");
    assert_eq!(body["members"][1]["phase"], "idle");
}

#[tokio::test]
async fn an_uncommanded_group_is_idle_on_every_member() {
    let address = spawn([]).await;

    let body = get_json(&address, &format!("/v1/groups/{GROUP}/recording")).await;

    assert_eq!(body["phase"], "idle");
    for member in body["members"].as_array().expect("members") {
        assert_eq!(member["phase"], "idle");
        assert_eq!(member["epoch"], 0);
    }
}

/// The other route that used to answer wrongly: an empty list for a group whose
/// members each have a queue. Refused now, and answered here.
#[tokio::test]
async fn the_group_command_list_is_partitioned_by_member() {
    let address = spawn([]).await;
    start_the_device_group(&address).await;

    let refused = get(&format!("{address}/v1/devices/{GROUP}/commands")).await;
    assert_eq!(refused.status().as_u16(), 404);

    let body = get_json(&address, &format!("/v1/groups/{GROUP}/commands")).await;

    assert_eq!(body["group"], GROUP);
    let members = body["members"].as_array().expect("members");
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["device"], ATRIUM);
    assert_eq!(members[1]["device"], ANNEX);
    for member in members {
        let commands = member["commands"].as_array().expect("commands");
        assert_eq!(commands.len(), 1, "one row per member");
        assert_eq!(commands[0]["intent"]["kind"], "start_recording");
        // The batch is what ties one group-addressed request back together.
        assert!(commands[0]["batch"].is_string());
    }
    // Both rows share it.
    assert_eq!(
        members[0]["commands"][0]["batch"],
        members[1]["commands"][0]["batch"]
    );
}

#[tokio::test]
async fn a_member_that_has_been_asked_nothing_is_an_empty_list_not_an_omission() {
    let address = spawn([]).await;
    let status = reqwest::Client::new()
        .post(format!("{address}/v1/devices/{ATRIUM}/recording/start"))
        .send()
        .await
        .expect("starting one member")
        .status()
        .as_u16();
    assert_eq!(status, 202);

    let body = get_json(&address, &format!("/v1/groups/{GROUP}/commands")).await;

    assert_eq!(body["members"].as_array().expect("members").len(), 2);
    assert_eq!(body["members"][0]["commands"].as_array().unwrap().len(), 1);
    assert_eq!(body["members"][1]["device"], ANNEX);
    assert_eq!(body["members"][1]["commands"].as_array().unwrap().len(), 0);
}

/// The breaking change, stated once against the whole `/v1/devices` space: a
/// device group id is refused everywhere, and every refusal names the
/// `/v1/groups` URL that answers the same question.
///
/// One test over the whole space rather than one per route, because the
/// property is about the space: a route that gained a group-id path back would
/// be a hole in it, and a per-route test would not notice the route it does not
/// cover.
#[tokio::test]
async fn every_device_route_refuses_a_device_group_id_and_names_the_group_url() {
    let address = spawn([]).await;
    let client = reqwest::Client::new();

    for (method, tail) in [
        ("get", ""),
        ("get", "/fields"),
        ("get", "/fields/RUNNING_STATE"),
        ("get", "/fields/RUNNING_STATE/history"),
        ("get", "/recording"),
        ("get", "/commands"),
        ("post", "/recording/start"),
        ("post", "/recording/stop"),
        ("post", "/recording/pause"),
        ("put", "/metadata/TITLE"),
        ("put", "/settings/TIMEZONE"),
    ] {
        let url = format!("{address}/v1/devices/{GROUP}{tail}");
        let request = match method {
            "get" => client.get(&url),
            "post" => client.post(&url),
            _ => client.put(&url).json(&serde_json::json!({"value": "x"})),
        };
        let response = request.send().await.expect("requesting");
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.expect("parsing the error body");

        assert_eq!(status, 404, "{method} {url} answered {status}: {body}");
        assert_eq!(body["code"], "not_found", "for {method} {url}");
        let message = body["error"].as_str().expect("error");
        assert!(
            message.contains("is a device group"),
            "for {method} {url}: {message}"
        );
        assert!(
            message.contains(&format!("/v1/groups/{GROUP}{tail}")),
            "for {method} {url} the refusal should name /v1/groups/{GROUP}{tail}, got {message}"
        );
    }
}

/// ...and the refusal is a claim about *groups* only. An id that names nothing
/// keeps each route's own answer, so the readings routes still say "nothing
/// stored" rather than inventing a claim about configuration.
#[tokio::test]
async fn an_unknown_id_is_unaffected_by_the_group_refusal() {
    let address = spawn([]).await;

    // Still an empty list, not a 404: the store cannot tell an unknown device
    // from one that has not answered yet.
    let body = get_json(&address, "/v1/devices/nobody/fields").await;
    assert_eq!(body["readings"].as_array().expect("readings").len(), 0);

    // Still idle at epoch 0, for the same reason.
    let body = get_json(&address, "/v1/devices/nobody/recording").await;
    assert_eq!(body["phase"], "idle");
}
