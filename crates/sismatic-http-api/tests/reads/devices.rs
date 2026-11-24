//! `/v1/reads/devices/{id}/…` — one device's stored reads.
//!
//! The half of the scope that answers from the store alone. That is what makes
//! an unknown device an empty list here rather than a `404`: the store cannot
//! tell "no such device" from "this one has not answered yet", and only the
//! catalog is entitled to the stronger claim — see `tests/inventory.rs`.

use std::sync::Arc;

use sismatic_api_types::{Read, ReadValue, RecordingEvent, ScheduleState};

use crate::{FailingStore, SCOPE, get, read_at, spawn_app, spawn_over};

const DEVICE: &str = "atrium-101";

/// A `Read` for `DEVICE`/`field` with a `Number` value stamped at `at`.
fn read(device: &str, field: &str, value: u32, at: &str) -> Read {
    read_at(device, field, ReadValue::Number(value), at)
}

/// Start the application over a store pre-loaded with `reads`, and a catalog
/// holding [`DEVICE`] — so nothing here trips the configured-set check.
async fn spawn_with(reads: impl IntoIterator<Item = Read>) -> String {
    spawn_over(reads, &[DEVICE]).await
}

#[tokio::test]
async fn one_field_comes_back_as_a_read() {
    let address = spawn_with([read(DEVICE, "SSH_PORT", 22023, "2026-07-23T14:03:11Z")]).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices/{DEVICE}/fields/SSH_PORT")).await;

    assert_eq!(status, 200);
    // The whole body, not field-by-field: this is the shape a client compiles
    // against, so a stray extra key or a renamed one is a change worth failing.
    assert_eq!(
        body,
        serde_json::json!({
            "device": DEVICE,
            "field": "SSH_PORT",
            "value": { "type": "number", "value": 22023 },
            "at": "2026-07-23T14:03:11Z",
        })
    );
}

#[tokio::test]
async fn every_field_of_a_device_is_listed_sorted_by_name() {
    // The regression this whole change exists for: before the store was keyed by
    // `(device, field)`, these three writes left one read and the other two
    // were unreachable through any route.
    let address = spawn_with([
        read(DEVICE, "SSH_PORT", 22023, "2026-07-23T14:00:00Z"),
        read(DEVICE, "FIRMWARE", 211, "2026-07-23T14:00:01Z"),
        read(DEVICE, "TIMEZONE", 5, "2026-07-23T14:00:02Z"),
    ])
    .await;

    let (status, body) = get(format!("{address}{SCOPE}/devices/{DEVICE}/fields")).await;

    assert_eq!(status, 200);
    let fields: Vec<&str> = body["reads"]
        .as_array()
        .expect("reads is an array")
        .iter()
        .map(|r| r["field"].as_str().expect("field is a string"))
        .collect();
    assert_eq!(fields, ["FIRMWARE", "SSH_PORT", "TIMEZONE"]);
}

#[tokio::test]
async fn a_field_is_addressable_however_it_is_spelled_in_the_url() {
    let address = spawn_with([read(DEVICE, "RUNNING_STATE", 1, "2026-07-23T14:00:00Z")]).await;

    for spelling in [
        "RUNNING_STATE",
        "running_state",
        "running-state",
        "Running-State",
    ] {
        let (status, body) = get(format!(
            "{address}{SCOPE}/devices/{DEVICE}/fields/{spelling}"
        ))
        .await;

        assert_eq!(status, 200, "spelling {spelling} should resolve");
        // However it was asked for, the answer names the field canonically —
        // the client never has to echo its own spelling back.
        assert_eq!(body["field"], "RUNNING_STATE");
    }
}

#[tokio::test]
async fn a_field_with_no_read_is_404_with_the_shared_error_envelope() {
    let address = spawn_with([read(DEVICE, "FIRMWARE", 211, "2026-07-23T14:00:00Z")]).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices/{DEVICE}/fields/SSH_PORT")).await;

    assert_eq!(status, 404);
    // The machine-readable half is what a client branches on; the message is for
    // the human reading the log.
    assert_eq!(body["code"], "not_found");
    assert!(
        body["error"]
            .as_str()
            .expect("error is a string")
            .contains("SSH_PORT"),
        "the message should name the field that was missing, got {body}"
    );
}

#[tokio::test]
async fn an_unknown_device_lists_no_fields_rather_than_404() {
    // The store cannot tell an unconfigured device from one that has answered
    // nothing yet, so it must not claim the former. See `list_fields`.
    let address = spawn_with([]).await;

    let (status, body) = get(format!("{address}{SCOPE}/devices/nobody/fields")).await;

    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({ "reads": [] }));
}

#[tokio::test]
async fn history_returns_one_field_oldest_first() {
    let address = spawn_with([
        read(DEVICE, "SSH_PORT", 22, "2026-07-23T14:00:00Z"),
        // A second field polled in between must not appear in the series.
        read(DEVICE, "FIRMWARE", 211, "2026-07-23T14:00:30Z"),
        read(DEVICE, "SSH_PORT", 2222, "2026-07-23T14:01:00Z"),
        read(DEVICE, "SSH_PORT", 22023, "2026-07-23T14:02:00Z"),
    ])
    .await;

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices/{DEVICE}/fields/SSH_PORT/history"
    ))
    .await;

    assert_eq!(status, 200);
    assert_eq!(values(&body), [22, 2222, 22023]);
}

#[tokio::test]
async fn history_is_scoped_to_the_requested_span() {
    let address = spawn_with([
        read(DEVICE, "T", 1, "2026-07-23T13:59:59Z"),
        read(DEVICE, "T", 2, "2026-07-23T14:00:00Z"),
        read(DEVICE, "T", 3, "2026-07-23T14:30:00Z"),
        read(DEVICE, "T", 4, "2026-07-23T15:00:00Z"),
        read(DEVICE, "T", 5, "2026-07-23T15:00:01Z"),
    ])
    .await;

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices/{DEVICE}/fields/T/history\
         ?start=2026-07-23T14:00:00Z&end=2026-07-23T15:00:00Z"
    ))
    .await;

    assert_eq!(status, 200);
    // Both bounds inclusive; the two straddling reads are out.
    assert_eq!(values(&body), [2, 3, 4]);
}

#[tokio::test]
async fn a_limited_history_is_the_most_recent_rows_still_in_order() {
    let address =
        spawn_with((0..5).map(|i| read(DEVICE, "T", i, &format!("2026-07-23T14:0{i}:00Z")))).await;

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices/{DEVICE}/fields/T/history?limit=2"
    ))
    .await;

    assert_eq!(status, 200);
    // The tail, not the head, and not reversed: a plot of a limited response is
    // a plot of the recent past.
    assert_eq!(values(&body), [3, 4]);
}

#[tokio::test]
async fn an_empty_span_is_an_empty_list_not_a_404() {
    // "Nothing happened in that window" is an answer. A 404 would say the field
    // does not exist, and the same request over a wider span returns rows.
    let address = spawn_with([read(DEVICE, "T", 1, "2026-07-23T14:00:00Z")]).await;

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices/{DEVICE}/fields/T/history\
         ?start=2020-01-01T00:00:00Z&end=2020-12-31T23:59:59Z"
    ))
    .await;

    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({ "reads": [] }));
}

#[tokio::test]
async fn a_field_query_parameter_contradicting_the_path_is_rejected() {
    let address = spawn_with([read(DEVICE, "T", 1, "2026-07-23T14:00:00Z")]).await;

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices/{DEVICE}/fields/T/history?field=FIRMWARE"
    ))
    .await;

    // Rejected rather than ignored: serving `T` to a caller that spelled out
    // `FIRMWARE` is the failure mode this rules out.
    assert_eq!(status, 400);
    assert_eq!(body["code"], "bad_instruction");
}

#[tokio::test]
async fn a_field_query_parameter_agreeing_with_the_path_is_accepted() {
    let address = spawn_with([read(DEVICE, "T", 1, "2026-07-23T14:00:00Z")]).await;

    // Redundant, but not a contradiction — and normalized the same way, so a
    // lowercase spelling of the same field still agrees.
    let (status, _) = get(format!(
        "{address}{SCOPE}/devices/{DEVICE}/fields/T/history?field=t"
    ))
    .await;

    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_store_failure_is_a_500_and_not_a_404() {
    // The distinction a client needs: "there is no such read" is about the
    // request, "the backend is down" is about us, and answering 404 for the
    // second would tell a dashboard to stop asking.
    let address = spawn_app(Arc::new(FailingStore));

    let (status, body) = get(format!("{address}{SCOPE}/devices/{DEVICE}/fields/FIRMWARE")).await;

    assert_eq!(status, 500);
    assert_eq!(body["code"], "internal");
}

#[tokio::test]
async fn the_reads_routes_are_gets() {
    let address = spawn_with([]).await;

    let response = reqwest::Client::new()
        .post(format!("{address}{SCOPE}/devices/{DEVICE}/fields/FIRMWARE"))
        .send()
        .await
        .expect("posting to a reads route");

    // 405 and an `Allow` header, for the same reason the health check gives one:
    // it names the method that would have worked. A read side that answered 404
    // to a POST would look like a missing route.
    assert_eq!(response.status().as_u16(), 405);
    assert_eq!(
        response.headers().get("allow").map(|v| v.as_bytes()),
        Some(&b"GET"[..])
    );
}

/// A `RECORDING_EVENTS` read for [`DEVICE`] whose events have `states`, in order.
fn events(states: &[ScheduleState], at: &str) -> Read {
    let events = states
        .iter()
        .enumerate()
        .map(|(i, state)| RecordingEvent {
            db_id: i as u64,
            start: None,
            identifier: String::new(),
            creator: String::new(),
            title: String::new(),
            state: Some(*state),
            media_present: None,
            package: format!("/var/internal/P_{i}"),
            extra: Default::default(),
        })
        .collect();
    read_at(
        DEVICE,
        "RECORDING_EVENTS",
        ReadValue::RecordingEvents(events),
        at,
    )
}

/// The `state` of every event in a read's list, as the wire spells it.
fn states(read: &serde_json::Value) -> Vec<String> {
    read["value"]["value"]
        .as_array()
        .expect("an event list")
        .iter()
        .map(|e| e["state"].as_str().expect("a state").to_owned())
        .collect()
}

#[tokio::test]
async fn where_on_a_history_picks_reads_before_limit_counts_them() {
    // Five readings, of which 0, 2 and 3 are over 1. `limit=2` counts matching
    // reads, so it returns the two most recent matches rather than whichever of
    // the last two rows happen to match.
    let readings = [5, 1, 7, 9, 0];
    let address = spawn_with(
        readings
            .iter()
            .enumerate()
            .map(|(i, v)| read(DEVICE, "T", *v, &format!("2026-07-23T14:0{i}:00Z"))),
    )
    .await;

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices/{DEVICE}/fields/T/history?where=T:gt:1&limit=2"
    ))
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(values(&body), [7, 9]);
}

#[tokio::test]
async fn a_history_can_be_asked_when_an_upload_failed() {
    let address = spawn_with([
        events(&[ScheduleState::Archiving], "2026-07-23T14:00:00Z"),
        events(&[ScheduleState::ArchiveFailed], "2026-07-23T14:01:00Z"),
        events(&[ScheduleState::Archived], "2026-07-23T14:02:00Z"),
    ])
    .await;

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices/{DEVICE}/fields/RECORDING_EVENTS/history\
         ?filter=RECORDING_EVENTS.state:archive_failed&where=@filter"
    ))
    .await;

    assert_eq!(status, 200, "{body}");
    let reads = body["reads"].as_array().expect("reads is an array");
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0]["at"], "2026-07-23T14:01:00Z");
}

#[tokio::test]
async fn filter_trims_the_latest_routes() {
    let address = spawn_with([
        events(
            &[ScheduleState::Archived, ScheduleState::ArchiveFailed],
            "2026-07-23T14:00:00Z",
        ),
        read(DEVICE, "T", 1, "2026-07-23T14:00:00Z"),
    ])
    .await;
    let filter = "filter=RECORDING_EVENTS.state:archive_failed";

    let (status, body) = get(format!("{address}{SCOPE}/devices/{DEVICE}/fields?{filter}")).await;
    assert_eq!(status, 200, "{body}");
    // Both reads stay; only the list inside one of them is trimmed.
    assert_eq!(body["reads"].as_array().expect("reads").len(), 2);
    assert_eq!(states(&body["reads"][0]), ["archive_failed"]);

    let (status, body) = get(format!(
        "{address}{SCOPE}/devices/{DEVICE}/fields/RECORDING_EVENTS?{filter}"
    ))
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(states(&body), ["archive_failed"]);
}

#[tokio::test]
async fn where_on_a_route_for_one_device_points_at_the_index() {
    let address = spawn_with([read(DEVICE, "T", 1, "2026-07-23T14:00:00Z")]).await;

    for route in ["fields", "fields/T"] {
        let (status, body) = get(format!(
            "{address}{SCOPE}/devices/{DEVICE}/{route}?where=T:1"
        ))
        .await;
        assert_eq!(status, 400, "{route}");
        let message = body["error"].as_str().expect("error is a string");
        assert!(
            message.contains(&format!("/v1/reads/devices?devices={DEVICE}")),
            "{route}: the message should name the index route, got {message}"
        );
    }
}

#[tokio::test]
async fn a_clause_on_another_field_than_the_path_names_is_refused() {
    let address = spawn_with([read(DEVICE, "T", 1, "2026-07-23T14:00:00Z")]).await;

    for query in ["where=FIRMWARE:2.11", "filter=ACTIVE_ALARMS.level:critical"] {
        let (status, _) = get(format!(
            "{address}{SCOPE}/devices/{DEVICE}/fields/T/history?{query}"
        ))
        .await;
        assert_eq!(status, 400, "{query}");
    }
}

/// The numeric values of a history response, in the order they were served.
fn values(body: &serde_json::Value) -> Vec<u64> {
    body["reads"]
        .as_array()
        .expect("reads is an array")
        .iter()
        .map(|r| r["value"]["value"].as_u64().expect("a number"))
        .collect()
}
