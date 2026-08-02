//! tests/readings.rs — the readings routes as a client meets them.
//!
//! Black-box, like `tests/health_check.rs`: each test starts the real server on
//! an ephemeral port and talks to it over HTTP, so what is pinned is the status
//! code and the JSON on the wire rather than a handler's return type. Nothing
//! below names a handler.
//!
//! The store under the server is the real [`MemoryStore`], not a double. These
//! routes are almost entirely *about* the store's semantics — which field wins,
//! what order a list comes back in, which rows fall inside a span — so a double
//! restating those semantics would be the thing under test rather than the thing
//! being tested against.

use std::net::TcpListener;
use std::sync::Arc;

use sismatic_api_types::{DeviceId, FieldName, Reading, ReadingValue, TimeSpan, Timestamp};
use sismatic_store::{DynReadStore, ReadError, ReadStore, WriteStore};
use sismatic_store_memory::MemoryStore;

/// A `Reading` for `device`/`field` with a `Number` value stamped at `at`.
fn reading(device: &str, field: &str, value: u32, at: &str) -> Reading {
    Reading {
        device: device.into(),
        field: field.into(),
        value: ReadingValue::Number(value),
        at: Timestamp(at.into()),
    }
}

/// A store whose every read fails, for the one test about a backend outage.
struct FailingStore;

#[async_trait::async_trait]
impl ReadStore for FailingStore {
    async fn latest(
        &self,
        _dev: DeviceId,
        _field: FieldName,
    ) -> Result<Option<Reading>, ReadError> {
        Err(ReadError::backend("the disk caught fire"))
    }

    async fn latest_all(&self, _dev: DeviceId) -> Result<Vec<Reading>, ReadError> {
        Err(ReadError::backend("the disk caught fire"))
    }

    async fn between(
        &self,
        _dev: DeviceId,
        _field: FieldName,
        _span: TimeSpan,
    ) -> Result<Vec<Reading>, ReadError> {
        Err(ReadError::backend("the disk caught fire"))
    }
}

/// Start the application over `store` on an ephemeral port; return its base URL.
fn spawn_app(store: DynReadStore) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();

    let server = sismatic_http_api::run(listener, store).expect("building the server");
    drop(tokio::spawn(server));

    format!("http://127.0.0.1:{port}")
}

/// Start the application over a [`MemoryStore`] pre-loaded with `readings`.
async fn spawn_with(readings: impl IntoIterator<Item = Reading>) -> String {
    let store = MemoryStore::default();
    for r in readings {
        store.upsert_latest(r).await.expect("seeding the store");
    }
    spawn_app(Arc::new(store))
}

/// `GET url`, returning the status and the parsed JSON body together — every
/// assertion below is about the pair.
async fn get(url: String) -> (u16, serde_json::Value) {
    let response = reqwest::get(url).await.expect("issuing the request");
    let status = response.status().as_u16();
    let body = response.json().await.expect("parsing the response body");
    (status, body)
}

#[tokio::test]
async fn one_field_comes_back_as_a_reading() {
    let address = spawn_with([reading(
        "atrium-101",
        "SSH_PORT",
        22023,
        "2026-07-23T14:03:11Z",
    )])
    .await;

    let (status, body) = get(format!("{address}/devices/atrium-101/fields/SSH_PORT")).await;

    assert_eq!(status, 200);
    // The whole body, not field-by-field: this is the shape a client compiles
    // against, so a stray extra key or a renamed one is a change worth failing.
    assert_eq!(
        body,
        serde_json::json!({
            "device": "atrium-101",
            "field": "SSH_PORT",
            "value": { "type": "number", "value": 22023 },
            "at": "2026-07-23T14:03:11Z",
        })
    );
}

#[tokio::test]
async fn every_field_of_a_device_is_listed_sorted_by_name() {
    // The regression this whole change exists for: before the store was keyed by
    // `(device, field)`, these three writes left one reading and the other two
    // were unreachable through any route.
    let address = spawn_with([
        reading("atrium-101", "SSH_PORT", 22023, "2026-07-23T14:00:00Z"),
        reading("atrium-101", "FIRMWARE", 211, "2026-07-23T14:00:01Z"),
        reading("atrium-101", "TIMEZONE", 5, "2026-07-23T14:00:02Z"),
    ])
    .await;

    let (status, body) = get(format!("{address}/devices/atrium-101/fields")).await;

    assert_eq!(status, 200);
    let fields: Vec<&str> = body["readings"]
        .as_array()
        .expect("readings is an array")
        .iter()
        .map(|r| r["field"].as_str().expect("field is a string"))
        .collect();
    assert_eq!(fields, ["FIRMWARE", "SSH_PORT", "TIMEZONE"]);
}

#[tokio::test]
async fn a_field_is_addressable_however_it_is_spelled_in_the_url() {
    let address = spawn_with([reading(
        "atrium-101",
        "RUNNING_STATE",
        1,
        "2026-07-23T14:00:00Z",
    )])
    .await;

    for spelling in [
        "RUNNING_STATE",
        "running_state",
        "running-state",
        "Running-State",
    ] {
        let (status, body) = get(format!("{address}/devices/atrium-101/fields/{spelling}")).await;

        assert_eq!(status, 200, "spelling {spelling} should resolve");
        // However it was asked for, the answer names the field canonically —
        // the client never has to echo its own spelling back.
        assert_eq!(body["field"], "RUNNING_STATE");
    }
}

#[tokio::test]
async fn a_field_with_no_reading_is_404_with_the_shared_error_envelope() {
    let address = spawn_with([reading(
        "atrium-101",
        "FIRMWARE",
        211,
        "2026-07-23T14:00:00Z",
    )])
    .await;

    let (status, body) = get(format!("{address}/devices/atrium-101/fields/SSH_PORT")).await;

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

    let (status, body) = get(format!("{address}/devices/nobody/fields")).await;

    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({ "readings": [] }));
}

#[tokio::test]
async fn history_returns_one_field_oldest_first() {
    let address = spawn_with([
        reading("atrium-101", "SSH_PORT", 22, "2026-07-23T14:00:00Z"),
        // A second field polled in between must not appear in the series.
        reading("atrium-101", "FIRMWARE", 211, "2026-07-23T14:00:30Z"),
        reading("atrium-101", "SSH_PORT", 2222, "2026-07-23T14:01:00Z"),
        reading("atrium-101", "SSH_PORT", 22023, "2026-07-23T14:02:00Z"),
    ])
    .await;

    let (status, body) = get(format!(
        "{address}/devices/atrium-101/fields/SSH_PORT/history"
    ))
    .await;

    assert_eq!(status, 200);
    let values: Vec<u64> = body["readings"]
        .as_array()
        .expect("readings is an array")
        .iter()
        .map(|r| r["value"]["value"].as_u64().expect("a number"))
        .collect();
    assert_eq!(values, [22, 2222, 22023]);
}

#[tokio::test]
async fn history_is_scoped_to_the_requested_span() {
    let address = spawn_with([
        reading("atrium-101", "T", 1, "2026-07-23T13:59:59Z"),
        reading("atrium-101", "T", 2, "2026-07-23T14:00:00Z"),
        reading("atrium-101", "T", 3, "2026-07-23T14:30:00Z"),
        reading("atrium-101", "T", 4, "2026-07-23T15:00:00Z"),
        reading("atrium-101", "T", 5, "2026-07-23T15:00:01Z"),
    ])
    .await;

    let (status, body) = get(format!(
        "{address}/devices/atrium-101/fields/T/history\
         ?start=2026-07-23T14:00:00Z&end=2026-07-23T15:00:00Z"
    ))
    .await;

    assert_eq!(status, 200);
    let values: Vec<u64> = body["readings"]
        .as_array()
        .expect("readings is an array")
        .iter()
        .map(|r| r["value"]["value"].as_u64().expect("a number"))
        .collect();
    // Both bounds inclusive; the two straddling readings are out.
    assert_eq!(values, [2, 3, 4]);
}

#[tokio::test]
async fn a_limited_history_is_the_most_recent_rows_still_in_order() {
    let address = spawn_with(
        (0..5).map(|i| reading("atrium-101", "T", i, &format!("2026-07-23T14:0{i}:00Z"))),
    )
    .await;

    let (status, body) = get(format!(
        "{address}/devices/atrium-101/fields/T/history?limit=2"
    ))
    .await;

    assert_eq!(status, 200);
    let values: Vec<u64> = body["readings"]
        .as_array()
        .expect("readings is an array")
        .iter()
        .map(|r| r["value"]["value"].as_u64().expect("a number"))
        .collect();
    // The tail, not the head, and not reversed: a plot of a limited response is
    // a plot of the recent past.
    assert_eq!(values, [3, 4]);
}

#[tokio::test]
async fn an_empty_span_is_an_empty_list_not_a_404() {
    // "Nothing happened in that window" is an answer. A 404 would say the field
    // does not exist, and the same request over a wider span returns rows.
    let address = spawn_with([reading("atrium-101", "T", 1, "2026-07-23T14:00:00Z")]).await;

    let (status, body) = get(format!(
        "{address}/devices/atrium-101/fields/T/history\
         ?start=2020-01-01T00:00:00Z&end=2020-12-31T23:59:59Z"
    ))
    .await;

    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({ "readings": [] }));
}

#[tokio::test]
async fn a_field_query_parameter_contradicting_the_path_is_rejected() {
    let address = spawn_with([reading("atrium-101", "T", 1, "2026-07-23T14:00:00Z")]).await;

    let (status, body) = get(format!(
        "{address}/devices/atrium-101/fields/T/history?field=FIRMWARE"
    ))
    .await;

    // Rejected rather than ignored: serving `T` to a caller that spelled out
    // `FIRMWARE` is the failure mode this rules out.
    assert_eq!(status, 400);
    assert_eq!(body["code"], "bad_instruction");
}

#[tokio::test]
async fn a_field_query_parameter_agreeing_with_the_path_is_accepted() {
    let address = spawn_with([reading("atrium-101", "T", 1, "2026-07-23T14:00:00Z")]).await;

    // Redundant, but not a contradiction — and normalized the same way, so a
    // lowercase spelling of the same field still agrees.
    let (status, _) = get(format!(
        "{address}/devices/atrium-101/fields/T/history?field=t"
    ))
    .await;

    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_store_failure_is_a_500_and_not_a_404() {
    // The distinction a client needs: "there is no such reading" is about the
    // request, "the backend is down" is about us, and answering 404 for the
    // second would tell a dashboard to stop asking.
    let address = spawn_app(Arc::new(FailingStore));

    let (status, body) = get(format!("{address}/devices/atrium-101/fields/FIRMWARE")).await;

    assert_eq!(status, 500);
    assert_eq!(body["code"], "internal");
}

#[tokio::test]
async fn the_readings_routes_are_gets() {
    let address = spawn_with([]).await;

    let response = reqwest::Client::new()
        .post(format!("{address}/devices/atrium-101/fields/FIRMWARE"))
        .send()
        .await
        .expect("posting to a readings route");

    // 405 and an `Allow` header, for the same reason the health check gives one:
    // it names the method that would have worked. A read side that answered 404
    // to a POST would look like a missing route.
    assert_eq!(response.status().as_u16(), 405);
    assert_eq!(
        response.headers().get("allow").map(|v| v.as_bytes()),
        Some(&b"GET"[..])
    );
}
