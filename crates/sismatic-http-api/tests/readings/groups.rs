//! `/v1/readings/groups/{id}/…` — a device group's readings, as a client meets
//! them.
//!
//! The real [`MemoryOutbox`] sits behind both the write routes and the
//! group-state read, and it is not a double for a reason specific to these
//! tests. The rule they depend on — *an expectation exists exactly when a group
//! request was admitted* — lives inside the outbox's admission critical section.
//! A stub that recorded expectations on demand would let a test assert drift
//! detection over a state the server can never actually be in, which is the
//! failure mode a double is worst at.
//!
//! So every expectation below is created the way a client creates one: by
//! POSTing to a group write route, over in the `/v1/commands` scope.
//!
//! [`MemoryOutbox`]: sismatic_store_memory::MemoryOutbox

use std::net::TcpListener;
use std::sync::Arc;

use sismatic_api_types::{Reading, ReadingValue, RecordingState, Timestamp};
use sismatic_store::WriteStore;
use sismatic_store_memory::MemoryStore;

use crate::{ANNEX, ATRIUM, GROUP, SCOPE, get_json, harness, reading_at, spawn_over};

/// The instant every seeded reading carries. Fixed, because no assertion here is
/// about time passing.
const AT: &str = "2026-08-17T00:00:00.000Z";

/// A `RUNNING_STATE` reading for `device`.
fn state(device: &str, value: RecordingState) -> Reading {
    reading(device, "RUNNING_STATE", ReadingValue::State(value))
}

fn reading(device: &str, field: &str, value: ReadingValue) -> Reading {
    reading_at(device, field, value, AT)
}

/// Start the application over a two-member device group and a store pre-loaded
/// with `readings`; return its base URL.
async fn spawn(readings: impl IntoIterator<Item = Reading>) -> String {
    spawn_over(readings, &[ATRIUM, ANNEX]).await
}

async fn get(url: &str) -> reqwest::Response {
    reqwest::get(url).await.expect("requesting the server")
}

/// Ask the device group to start recording, the way a client does — through the
/// write scope, which is where the expectation these routes read is filed.
async fn start_the_device_group(address: &str) {
    let status = reqwest::Client::new()
        .post(format!(
            "{address}/v1/commands/groups/{GROUP}/recording/start"
        ))
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

    let body = get_json(&address, &format!("/groups/{GROUP}/fields/RUNNING_STATE")).await;

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

    let body = get_json(&address, &format!("/groups/{GROUP}/fields/RUNNING_STATE")).await;

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

    let body = get_json(&address, &format!("/groups/{GROUP}/fields/TIMEZONE")).await;

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
        let body = get_json(&address, &format!("/groups/{GROUP}/fields/{spelling}")).await;
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

    let body = get_json(&address, &format!("/groups/{GROUP}/fields/RUNNING_STATE")).await;

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

    let body = get_json(&address, &format!("/groups/{GROUP}/fields/RUNNING_STATE")).await;

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

    let body = get_json(&address, &format!("/groups/{GROUP}/fields/RUNNING_STATE")).await;

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

    let body = get_json(&address, &format!("/groups/{GROUP}/fields/FIRMWARE")).await;

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

    let body = get_json(&address, &format!("/groups/{GROUP}/fields/RUNNING_STATE")).await;

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
        .put(format!(
            "{address}/v1/commands/groups/{GROUP}/metadata/title"
        ))
        .json(&serde_json::json!({"value": "Week 4"}))
        .send()
        .await
        .expect("writing the title")
        .status()
        .as_u16();
    assert_eq!(status, 202);

    let body = get_json(&address, &format!("/groups/{GROUP}/fields/TITLE")).await;

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
        .post(format!(
            "{address}/v1/commands/devices/{ATRIUM}/recording/start"
        ))
        .send()
        .await
        .expect("starting one device")
        .status()
        .as_u16();
    assert_eq!(status, 202);

    let body = get_json(&address, &format!("/groups/{GROUP}/fields/RUNNING_STATE")).await;

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

    let body = get_json(&address, &format!("/groups/{GROUP}/fields")).await;

    assert_eq!(body["group"], GROUP);
    assert_eq!(
        field_names(&body),
        ["FIRMWARE", "RUNNING_STATE", "TIMEZONE"]
    );

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

    let body = get_json(&address, &format!("/groups/{GROUP}/fields")).await;

    assert_eq!(field_names(&body), ["RUNNING_STATE"]);
    assert_eq!(body["fields"][0]["expected"]["value"]["value"], "started");
    assert_eq!(body["fields"][0]["sync"], "unknown");
}

#[tokio::test]
async fn an_index_for_a_room_that_has_done_nothing_is_empty_not_an_error() {
    let address = spawn([]).await;

    let body = get_json(&address, &format!("/groups/{GROUP}/fields")).await;

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
    let address = serve(store);

    let body = get_json(
        &address,
        &format!("/groups/{GROUP}/fields/RUNNING_STATE/history"),
    )
    .await;

    assert_eq!(body["group"], GROUP);
    assert_eq!(body["field"], "RUNNING_STATE");
    assert_eq!(body["members"][0]["device"], ATRIUM);
    assert_eq!(series(&body, 0).len(), 2);
    // Oldest first, so the series plots forwards.
    assert_eq!(series(&body, 0)[0]["value"]["value"], "stopped");
    assert_eq!(series(&body, 0)[1]["value"]["value"], "started");
    // A member with one row is listed with one row, not merged into the first.
    assert_eq!(body["members"][1]["device"], ANNEX);
    assert_eq!(series(&body, 1).len(), 1);
}

#[tokio::test]
async fn a_member_with_nothing_in_the_span_is_an_empty_series_not_an_omission() {
    let address = spawn([state(ATRIUM, RecordingState::Started)]).await;

    let body = get_json(
        &address,
        &format!("/groups/{GROUP}/fields/RUNNING_STATE/history"),
    )
    .await;

    assert_eq!(body["members"].as_array().expect("members").len(), 2);
    assert_eq!(body["members"][1]["device"], ANNEX);
    assert_eq!(series(&body, 1).len(), 0);
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
    let address = serve(store);

    let body = get_json(
        &address,
        &format!("/groups/{GROUP}/fields/PORT_TIMEOUT/history?limit=1"),
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
            "/groups/{GROUP}/fields/RUNNING_STATE/history\
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
        "{address}{SCOPE}/groups/{GROUP}/fields/RUNNING_STATE/history?field=FIRMWARE"
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
        &format!("/groups/{GROUP}/fields/RUNNING_STATE/history?field=running-state"),
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

    for tail in [
        "/fields",
        "/fields/RUNNING_STATE",
        "/fields/RUNNING_STATE/history",
    ] {
        let url = format!("{address}{SCOPE}/groups/typo{tail}");
        let response = get(&url).await;
        assert_eq!(response.status().as_u16(), 404, "for {url}");
        let body: serde_json::Value = response.json().await.expect("parsing the error body");
        assert_eq!(body["code"], "not_found");
        assert!(
            body["error"].as_str().expect("error").contains("typo"),
            "the message should name the id, got {body}"
        );
    }
}

/// Devices and groups share one id namespace, so naming a device here is a
/// different mistake from naming nothing: the fix is a different URL, and saying
/// which saves a round trip. The refusal has to name a URL *in this scope* — a
/// hint pointing outside it would send the caller to a second 404.
#[tokio::test]
async fn a_device_id_on_a_group_route_says_which_route_to_use() {
    let address = spawn([]).await;

    let response = get(&format!("{address}{SCOPE}/groups/{ATRIUM}/fields")).await;

    assert_eq!(response.status().as_u16(), 404);
    let body: serde_json::Value = response.json().await.expect("parsing the error body");
    let message = body["error"].as_str().expect("error");
    assert!(
        message.contains("is a device") && message.contains(&format!("{SCOPE}/devices/{ATRIUM}")),
        "got {message}"
    );
}

/// The mirror of the test above, stated once over the whole `/devices` half of
/// this scope: a device group id is refused everywhere in it, and every refusal
/// names the `/groups` URL that answers the same question.
///
/// One test over the half rather than one per route, because the property is
/// about the half: a route that gained a group-id path back would be a hole in
/// it, and a per-route test would not notice the route it does not cover.
#[tokio::test]
async fn every_device_route_in_this_scope_refuses_a_device_group_id() {
    let address = spawn([]).await;

    for tail in [
        "/fields",
        "/fields/RUNNING_STATE",
        "/fields/RUNNING_STATE/history",
    ] {
        let url = format!("{address}{SCOPE}/devices/{GROUP}{tail}");
        let response = get(&url).await;
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.expect("parsing the error body");

        assert_eq!(status, 404, "GET {url} answered {status}: {body}");
        assert_eq!(body["code"], "not_found", "for {url}");
        let message = body["error"].as_str().expect("error");
        assert!(
            message.contains("is a device group"),
            "for {url}: {message}"
        );
        assert!(
            message.contains(&format!("{SCOPE}/groups/{GROUP}{tail}")),
            "for {url} the refusal should name {SCOPE}/groups/{GROUP}{tail}, got {message}"
        );
    }
}

/// ...and the refusal is a claim about *groups* only. An id that names nothing
/// keeps this route's own answer, so the device readings routes still say
/// "nothing stored" rather than inventing a claim about configuration.
#[tokio::test]
async fn an_unknown_id_is_unaffected_by_the_group_refusal() {
    let address = spawn([]).await;

    // Still an empty list, not a 404: the store cannot tell an unknown device
    // from one that has not answered yet.
    let body = get_json(&address, "/devices/nobody/fields").await;
    assert_eq!(body["readings"].as_array().expect("readings").len(), 0);
}

/// Serve `store` over the two-member device group on an ephemeral port.
///
/// The seeded-store spawns above go through [`spawn`]; this is for the two tests
/// that need several rows of one field, which `upsert_latest` can only produce
/// one at a time.
fn serve(store: MemoryStore) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener.local_addr().expect("address").port();
    harness::serve_with(
        listener,
        Arc::new(store),
        harness::device_group(&[ATRIUM, ANNEX]),
    );
    format!("http://127.0.0.1:{port}")
}

/// The field names an index response listed, in order.
fn field_names(body: &serde_json::Value) -> Vec<&str> {
    body["fields"]
        .as_array()
        .expect("fields is an array")
        .iter()
        .map(|f| f["field"].as_str().expect("field"))
        .collect()
}

/// One member's series out of a history response.
fn series(body: &serde_json::Value, member: usize) -> &Vec<serde_json::Value> {
    body["members"][member]["readings"]
        .as_array()
        .expect("readings")
}
