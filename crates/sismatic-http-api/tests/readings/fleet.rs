//! `/v1/readings/devices` — the fleet index, as a client meets it.
//!
//! The one route in the scope whose answer is a *set* of devices rather than one
//! addressable thing, so what these tests are mostly about is which devices come
//! back, in what order, and how a caller walks past the end of a page.
//!
//! The catalogs below are built here rather than taken from the harness, because
//! that helper puts every device it is given into the one group — and the two
//! filters this route composes (`?devices=` and `?group=`) can only be told apart
//! by a fleet that is *wider* than the group in it.

use std::net::TcpListener;
use std::sync::Arc;

use sismatic_api_types::{
    Barrier, ConnectionStatus, DeviceSummary, GroupSummary, Reading, ReadingValue, RecordingState,
};
use sismatic_store::WriteStore;
use sismatic_store_memory::{MemoryCatalog, MemoryStore};

use crate::{ANNEX, ATRIUM, GROUP, SCOPE, get, reading_at};

/// A third device, outside the group the other two are in.
///
/// The fleet is therefore strictly wider than the group, which is what makes
/// `?group=` observable as a filter at all — with every device in the group,
/// filtering by it and not filtering by it give the same page.
const BEACON: &str = "beacon";

/// A catalog of `devices`, with [`GROUP`] over `members`.
///
/// Member order is the caller's, deliberately: `MemoryCatalog` sorts devices by
/// id and leaves a group's members alone, so writing the members out of order is
/// what lets a test see whether the *page* came back in id order or in the
/// group's configured order.
fn catalog(devices: &[&str], members: &[&str]) -> MemoryCatalog {
    MemoryCatalog::new(
        devices
            .iter()
            .map(|id| DeviceSummary {
                id: (*id).to_owned(),
                host: "10.0.0.7".to_owned(),
                port: 22023,
                eager: false,
                status: ConnectionStatus::Unknown,
            })
            .collect(),
        vec![GroupSummary {
            id: GROUP.to_owned(),
            members: members.iter().map(|id| (*id).to_owned()).collect(),
            barrier_timeout_secs: 15,
            barrier: Barrier::FailBatch,
        }],
    )
}

/// Start the application over a store pre-loaded with `readings` and a stated
/// catalog; return its base URL.
async fn spawn(readings: impl IntoIterator<Item = Reading>, catalog: MemoryCatalog) -> String {
    let store = MemoryStore::default();
    for r in readings {
        store.upsert_latest(r).await.expect("seeding the store");
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    crate::harness::serve_with(listener, Arc::new(store), catalog);

    format!("http://127.0.0.1:{port}")
}

/// The three-device fleet every test below runs against unless it says
/// otherwise, with `atrium` and `annex` in the group and `beacon` outside it.
fn fleet() -> MemoryCatalog {
    catalog(&[ATRIUM, ANNEX, BEACON], &[ATRIUM, ANNEX])
}

/// A recording-state reading, the field most of these filters are written
/// against.
fn state(device: &str, value: RecordingState, at: &str) -> Reading {
    reading_at(device, "RUNNING_STATE", ReadingValue::State(value), at)
}

const AT: &str = "2026-07-23T14:00:00Z";

/// The device ids of a fleet page, in the order they were served.
fn ids(body: &serde_json::Value) -> Vec<String> {
    body["devices"]
        .as_array()
        .expect("devices is an array")
        .iter()
        .map(|d| d["device"].as_str().expect("device is a string").to_owned())
        .collect()
}

/// The field names one row carries, in the order they were served.
fn fields(row: &serde_json::Value) -> Vec<String> {
    row["latest"]
        .as_array()
        .expect("latest is an array")
        .iter()
        .map(|r| r["field"].as_str().expect("field is a string").to_owned())
        .collect()
}

#[tokio::test]
async fn the_whole_fleet_comes_back_one_row_per_device_ordered_by_id() {
    let address = spawn(
        [
            state(ATRIUM, RecordingState::Started, AT),
            state(ANNEX, RecordingState::Stopped, AT),
            state(BEACON, RecordingState::Paused, AT),
        ],
        fleet(),
    )
    .await;

    let (status, body) = get(format!("{address}{SCOPE}/devices")).await;

    assert_eq!(status, 200);
    // Id order, not the order the catalog was written in or the readings were
    // seeded in — the cursor indexes this order, so it has to be the sorted one.
    assert_eq!(ids(&body), [ANNEX, ATRIUM, BEACON]);
    // The last page of a walk carries no cursor.
    assert_eq!(body["next"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_configured_device_that_has_never_answered_is_a_row_with_no_readings() {
    // The row the route exists to show. Sourced from the store this device would
    // simply be missing, and "the recorder we installed last week has reported
    // nothing" is the finding a fleet page is read for.
    let address = spawn([state(ATRIUM, RecordingState::Started, AT)], fleet()).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices")).await;

    assert_eq!(status, 200);
    assert_eq!(ids(&body), [ANNEX, ATRIUM, BEACON]);
    assert_eq!(body["devices"][0]["device"], ANNEX);
    assert_eq!(body["devices"][0]["latest"], serde_json::json!([]));
}

#[tokio::test]
async fn fields_selects_which_columns_each_row_carries() {
    let address = spawn(
        [
            state(ATRIUM, RecordingState::Started, AT),
            reading_at(ATRIUM, "FIRMWARE", ReadingValue::Version("2.11".into()), AT),
            reading_at(ATRIUM, "TIMEZONE", ReadingValue::Text("UTC".into()), AT),
        ],
        fleet(),
    )
    .await;

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices?devices={ATRIUM}&fields=running-state,firmware"
    ))
    .await;

    assert_eq!(status, 200);
    // Normalized exactly as a path segment is, and the untouched third field is
    // gone. Field order is the store's, which is alphabetical.
    assert_eq!(fields(&body["devices"][0]), ["FIRMWARE", "RUNNING_STATE"]);
}

#[tokio::test]
async fn where_selects_devices_and_leaves_their_other_requested_fields_alone() {
    // The composition the two filters exist for: "the firmware of every stopped
    // recorder". The predicate reads a field `?fields=` does not ask for, so a
    // handler that projected before filtering would answer with nothing at all.
    let address = spawn(
        [
            state(ATRIUM, RecordingState::Started, AT),
            reading_at(ATRIUM, "FIRMWARE", ReadingValue::Version("2.11".into()), AT),
            state(ANNEX, RecordingState::Stopped, AT),
            reading_at(ANNEX, "FIRMWARE", ReadingValue::Version("2.09".into()), AT),
        ],
        fleet(),
    )
    .await;

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices?fields=FIRMWARE&where=RUNNING_STATE:stopped"
    ))
    .await;

    assert_eq!(status, 200);
    assert_eq!(ids(&body), [ANNEX]);
    // The row carries the column that was asked for, not the one that was
    // filtered on.
    assert_eq!(fields(&body["devices"][0]), ["FIRMWARE"]);
    assert_eq!(body["devices"][0]["latest"][0]["value"]["value"], "2.09");
}

#[tokio::test]
async fn a_predicate_is_matched_in_the_shape_the_device_answered_in() {
    // The caller writes text in a URL and the store holds a decoded value; the
    // comparison reconciles them, so a port filter does not have to be spelled
    // `{"type":"port","value":8080}`.
    let address = spawn(
        [
            reading_at(ATRIUM, "HTTP_PORT", ReadingValue::Port(8080), AT),
            reading_at(ANNEX, "HTTP_PORT", ReadingValue::Port(9090), AT),
        ],
        fleet(),
    )
    .await;

    let (status, body) = get(format!("{address}{SCOPE}/devices?where=HTTP_PORT:8080")).await;

    assert_eq!(status, 200);
    assert_eq!(ids(&body), [ATRIUM]);
}

#[tokio::test]
async fn every_predicate_has_to_hold_for_a_device_to_appear() {
    let address = spawn(
        [
            state(ATRIUM, RecordingState::Stopped, AT),
            reading_at(ATRIUM, "FIRMWARE", ReadingValue::Version("2.11".into()), AT),
            state(ANNEX, RecordingState::Stopped, AT),
            reading_at(ANNEX, "FIRMWARE", ReadingValue::Version("2.09".into()), AT),
        ],
        fleet(),
    )
    .await;

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices?where=RUNNING_STATE:stopped,FIRMWARE:2.11"
    ))
    .await;

    assert_eq!(status, 200);
    assert_eq!(ids(&body), [ATRIUM]);
}

#[tokio::test]
async fn a_malformed_predicate_is_refused_rather_than_ignored() {
    // Dropping it would answer a wider question than the one asked — every
    // device rather than the stopped ones — which is the one thing a filter
    // must never do.
    let address = spawn([], fleet()).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices?where=RUNNING_STATE")).await;

    assert_eq!(status, 400);
    assert_eq!(body["code"], "bad_instruction");
    assert!(
        body["error"]
            .as_str()
            .expect("error is a string")
            .contains("RUNNING_STATE"),
        "the message should quote what could not be read, got {body}"
    );
}

#[tokio::test]
async fn devices_narrows_the_page_by_id() {
    let address = spawn([], fleet()).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices?devices={BEACON},{ANNEX}")).await;

    assert_eq!(status, 200);
    // Still id order, not the order the filter named them in.
    assert_eq!(ids(&body), [ANNEX, BEACON]);
}

#[tokio::test]
async fn an_unknown_device_in_the_filter_is_a_404_that_names_it() {
    // The claim the catalog is entitled to make, and the readings routes below
    // this one are not. A silently smaller page would report a typo as a healthy
    // fleet.
    let address = spawn([], fleet()).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices?devices={ATRIUM},nobody")).await;

    assert_eq!(status, 404);
    assert_eq!(body["code"], "not_found");
    assert!(
        body["error"]
            .as_str()
            .expect("error is a string")
            .contains("nobody"),
        "the message should name the id that is not configured, got {body}"
    );
}

#[tokio::test]
async fn a_group_id_in_the_device_filter_points_at_the_group_parameter() {
    // A different mistake from a typo, and the fix is the other *parameter*
    // rather than another URL — which is why this route answers it itself
    // instead of redirecting to the group routes the way a path would.
    let address = spawn([], fleet()).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices?devices={GROUP}")).await;

    assert_eq!(status, 404);
    let message = body["error"].as_str().expect("error is a string");
    assert!(
        message.contains(&format!("?group={GROUP}")),
        "the message should name the parameter that answers, got {message}"
    );
}

#[tokio::test]
async fn group_narrows_to_its_members_and_still_pages_in_id_order() {
    let address = spawn([], fleet()).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices?group={GROUP}")).await;

    assert_eq!(status, 200);
    // The group is configured `[atrium, annex]`; the page is `[annex, atrium]`.
    // That is the point — the cursor indexes id order, so a page served in a
    // group's configured order would skip and repeat devices across a walk.
    assert_eq!(ids(&body), [ANNEX, ATRIUM]);
}

#[tokio::test]
async fn an_unknown_group_is_a_404() {
    let address = spawn([], fleet()).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices?group=nowhere")).await;

    assert_eq!(status, 404);
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn the_two_device_filters_intersect() {
    // Two filters on one request narrow. Neither contradicts the other — "these
    // two devices, if they are in that group" has an answer — so neither is
    // refused the way a `?field=` disagreeing with a path segment is.
    let address = spawn([], fleet()).await;

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices?group={GROUP}&devices={ATRIUM},{BEACON}"
    ))
    .await;

    assert_eq!(status, 200);
    // `beacon` is named but outside the group; `annex` is in the group but not
    // named. Only `atrium` is both.
    assert_eq!(ids(&body), [ATRIUM]);
}

#[tokio::test]
async fn a_walk_covers_the_fleet_without_repeating_or_skipping_a_device() {
    let address = spawn([], fleet()).await;

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    // Bounded so a cursor that fails to advance fails the test rather than
    // hanging it.
    for _ in 0..5 {
        let url = match &cursor {
            Some(after) => format!("{address}{SCOPE}/devices?limit=1&after={after}"),
            None => format!("{address}{SCOPE}/devices?limit=1"),
        };
        let (status, body) = get(url).await;
        assert_eq!(status, 200);

        seen.extend(ids(&body));
        match body["next"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }

    assert_eq!(seen, [ANNEX, ATRIUM, BEACON]);
}

#[tokio::test]
async fn a_full_page_with_nothing_after_it_carries_no_cursor() {
    // The boundary a `limit + 1` scan exists to get right: three devices asked
    // for in a page of three is the *last* page, not a full one with more behind
    // it. Answering a cursor here would send a client round again for nothing.
    let address = spawn([], fleet()).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices?limit=3")).await;

    assert_eq!(status, 200);
    assert_eq!(ids(&body), [ANNEX, ATRIUM, BEACON]);
    assert_eq!(body["next"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_cursor_walks_the_filtered_set_rather_than_the_whole_fleet() {
    // Pagination composes with the filters: the cursor advances over what
    // survived them, so a page boundary cannot resurrect a device `?where=`
    // excluded.
    let address = spawn(
        [
            state(ATRIUM, RecordingState::Stopped, AT),
            state(ANNEX, RecordingState::Started, AT),
            state(BEACON, RecordingState::Stopped, AT),
        ],
        fleet(),
    )
    .await;

    let (status, first) = get(format!(
        "{address}{SCOPE}/devices?where=RUNNING_STATE:stopped&limit=1"
    ))
    .await;
    assert_eq!(status, 200);
    assert_eq!(ids(&first), [ATRIUM]);
    assert_eq!(first["next"], ATRIUM);

    let (status, second) = get(format!(
        "{address}{SCOPE}/devices?where=RUNNING_STATE:stopped&limit=1&after={ATRIUM}"
    ))
    .await;
    assert_eq!(status, 200);
    // `annex` is started and sits between them in id order; the walk steps over
    // it rather than spending a page on it.
    assert_eq!(ids(&second), [BEACON]);
    assert_eq!(second["next"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_page_of_no_devices_is_refused() {
    // It could carry no cursor, so a client that accepted one would loop forever
    // on a `next` that is always null while the fleet it never saw sits behind
    // it.
    let address = spawn([], fleet()).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices?limit=0")).await;

    assert_eq!(status, 400);
    assert_eq!(body["code"], "bad_instruction");
}

#[tokio::test]
async fn an_over_large_limit_is_capped_rather_than_refused() {
    // The other half of the asymmetry: clamping still yields a usable page and a
    // cursor that advances, so it costs a round trip and nothing else.
    let address = spawn([], fleet()).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices?limit=4294967295")).await;

    assert_eq!(status, 200);
    assert_eq!(ids(&body), [ANNEX, ATRIUM, BEACON]);
}

#[tokio::test]
async fn a_store_failure_is_a_500_and_not_an_empty_fleet() {
    // The distinction that matters most on this route: every row comes from a
    // store read, so a backend outage could plausibly render as a page of
    // devices with empty `latest` lists — a fleet that looks configured and
    // silent rather than one nobody can read.
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    crate::harness::serve_with(listener, Arc::new(crate::FailingStore), fleet());
    let address = format!("http://127.0.0.1:{port}");

    let (status, body) = get(format!("{address}{SCOPE}/devices")).await;

    assert_eq!(status, 500);
    assert_eq!(body["code"], "internal");
}

#[tokio::test]
async fn the_fleet_route_is_a_get() {
    let address = spawn([], fleet()).await;

    let response = reqwest::Client::new()
        .post(format!("{address}{SCOPE}/devices"))
        .send()
        .await
        .expect("posting to the fleet route");

    assert_eq!(response.status().as_u16(), 405);
    assert_eq!(
        response.headers().get("allow").map(|v| v.as_bytes()),
        Some(&b"GET"[..])
    );
}
