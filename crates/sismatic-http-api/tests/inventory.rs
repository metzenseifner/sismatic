//! tests/inventory.rs — the device and group index, and the guard it puts on
//! the write path.
//!
//! Two things are worth pinning from out here. The first is that the index
//! answers from the *catalog* and not the store, which is what makes a `404`
//! here mean "not in the devices file" rather than "nothing polled yet" — the
//! distinction the readings routes deliberately cannot draw. The second is that
//! a write to an id the catalog does not hold is refused at submission, which
//! is the whole reason the port exists: without it the outbox admits the
//! command and answers `202` for a device that will never receive it.

use std::sync::Arc;

use sismatic_api_types::{
    ConnectionStatus, DeviceSummary, GroupSummary, Reading, ReadingValue, Timestamp,
};
use sismatic_store::{DynReadStore, WriteStore};
use sismatic_store_memory::{MemoryCatalog, MemoryStore};

mod harness;

const DEVICE: &str = harness::DEVICE;
const GROUP: &str = harness::GROUP;

fn summary(id: &str, host: &str, eager: bool) -> DeviceSummary {
    DeviceSummary {
        id: id.to_owned(),
        host: host.to_owned(),
        port: 22023,
        eager,
        status: ConnectionStatus::Unknown,
    }
}

/// A store holding one reading of `FIRMWARE` on [`DEVICE`], so the detail route
/// has something to join against.
async fn seeded_store() -> DynReadStore {
    let store = MemoryStore::default();
    store
        .upsert_latest(Reading {
            device: DEVICE.into(),
            field: "FIRMWARE".into(),
            value: ReadingValue::Version("2.11".into()),
            at: Timestamp("2026-07-23T14:03:11Z".into()),
        })
        .await
        .expect("seeding the store");
    Arc::new(store)
}

async fn get(url: String) -> (u16, serde_json::Value) {
    let response = reqwest::get(url).await.expect("issuing the request");
    let status = response.status().as_u16();
    let body = response.json().await.expect("parsing the response body");
    (status, body)
}

// ---- the index ---------------------------------------------------------

#[tokio::test]
async fn the_device_index_lists_the_configured_set_ordered_by_id() {
    let catalog = MemoryCatalog::new(
        // Built out of order, so a passing assertion is the adapter's sort and
        // not an accident of construction.
        vec![
            summary("zulu", "10.0.0.9", false),
            summary("alpha", "10.0.0.1", true),
        ],
        vec![],
    );
    let (address, ..) = harness::spawn_with(Arc::new(MemoryStore::default()), catalog);

    let (status, body) = get(format!("{address}/v1/devices")).await;

    assert_eq!(status, 200);
    assert_eq!(
        body,
        serde_json::json!({"devices": [
            {"id": "alpha", "host": "10.0.0.1", "port": 22023, "eager": true, "status": "unknown"},
            {"id": "zulu", "host": "10.0.0.9", "port": 22023, "eager": false, "status": "unknown"},
        ]})
    );
}

/// The whole point of answering from the catalog: an empty index means "none
/// are configured", where an empty readings list means "none have answered".
#[tokio::test]
async fn an_empty_catalog_is_an_empty_index_not_an_error() {
    let (address, ..) =
        harness::spawn_with(Arc::new(MemoryStore::default()), MemoryCatalog::default());

    assert_eq!(
        get(format!("{address}/v1/devices")).await,
        (200, serde_json::json!({"devices": []}))
    );
    assert_eq!(
        get(format!("{address}/v1/groups")).await,
        (200, serde_json::json!({"groups": []}))
    );
}

/// No credential can reach the wire, because `DeviceSummary` has no field to
/// carry one — a stronger guarantee than redacting at serialization time.
#[tokio::test]
async fn the_index_carries_no_credentials() {
    let (address, ..) = harness::spawn(Arc::new(MemoryStore::default()));

    let (_, body) = get(format!("{address}/v1/devices")).await;

    let rendered = body.to_string();
    for secret in ["username", "password", "admin", "extron"] {
        assert!(
            !rendered.contains(secret),
            "'{secret}' reached the wire: {rendered}"
        );
    }
}

#[tokio::test]
async fn the_group_index_lists_members_in_configured_order() {
    let catalog = MemoryCatalog::new(
        vec![
            summary("atrium", "10.0.0.1", false),
            summary("annex", "10.0.0.2", false),
        ],
        vec![GroupSummary {
            id: "room".to_owned(),
            // Not alphabetical: the operator wrote this sequence and a fan-out
            // has to address the room the way it reads.
            members: vec!["atrium".to_owned(), "annex".to_owned()],
        }],
    );
    let (address, ..) = harness::spawn_with(Arc::new(MemoryStore::default()), catalog);

    let (status, body) = get(format!("{address}/v1/groups")).await;

    assert_eq!(status, 200);
    assert_eq!(
        body,
        serde_json::json!({"groups": [{"id": "room", "members": ["atrium", "annex"]}]})
    );
}

// ---- one device, one group --------------------------------------------

/// The route that joins the two sides: the catalog says what exists, the store
/// says what it last reported.
#[tokio::test]
async fn the_detail_route_joins_the_catalog_and_the_store() {
    let (address, ..) = harness::spawn(seeded_store().await);

    let (status, body) = get(format!("{address}/v1/devices/{DEVICE}")).await;

    assert_eq!(status, 200);
    assert_eq!(body["device"]["id"], DEVICE);
    assert_eq!(body["latest"][0]["field"], "FIRMWARE");
    assert_eq!(body["latest"][0]["value"]["value"], "2.11");
}

/// A device that is configured but has never answered is a `200` with nothing
/// in `latest` — which is what lets a dashboard render its row at all.
#[tokio::test]
async fn a_device_that_has_never_answered_still_has_a_detail_page() {
    let (address, ..) = harness::spawn(Arc::new(MemoryStore::default()));

    let (status, body) = get(format!("{address}/v1/devices/{DEVICE}")).await;

    assert_eq!(status, 200);
    assert_eq!(body["latest"], serde_json::json!([]));
}

#[tokio::test]
async fn an_unconfigured_device_is_a_404_here_and_an_empty_list_on_the_readings_route() {
    let (address, ..) = harness::spawn(Arc::new(MemoryStore::default()));

    // The catalog knows the configured set, so it can say this id is not in it.
    let (status, body) = get(format!("{address}/v1/devices/nobody")).await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], "not_found");

    // The store cannot: "no such device" and "never polled" are one answer from
    // there, so it answers the honest, weaker one. The divergence is deliberate.
    assert_eq!(
        get(format!("{address}/v1/devices/nobody/fields")).await,
        (200, serde_json::json!({"readings": []}))
    );
}

#[tokio::test]
async fn one_group_resolves_by_id() {
    let (address, ..) = harness::spawn(Arc::new(MemoryStore::default()));

    let (status, body) = get(format!("{address}/v1/groups/{GROUP}")).await;

    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({"id": GROUP, "members": [DEVICE]}));
}

/// Devices and groups share one id namespace, so naming a device on the group
/// route is a different mistake from naming nothing — and the fix is a
/// different URL rather than a different id.
#[tokio::test]
async fn a_device_id_on_the_group_route_says_which_route_to_use() {
    let (address, ..) = harness::spawn(Arc::new(MemoryStore::default()));

    let (status, body) = get(format!("{address}/v1/groups/{DEVICE}")).await;

    assert_eq!(status, 404);
    let message = body["error"].as_str().expect("an error message");
    assert!(
        message.contains("is a device") && message.contains(&format!("/v1/devices/{DEVICE}")),
        "got: {message}"
    );
}

// ---- the guard on the write path --------------------------------------

/// The failure the catalog exists to prevent. Without it the outbox admits the
/// command against a fresh idle phase and answers `202`, and the caller learns
/// its recording never started by polling a command that fails at dispatch.
#[tokio::test]
async fn a_write_to_an_unconfigured_device_is_refused_at_submission() {
    let (address, outbox) = harness::spawn(Arc::new(MemoryStore::default()));

    let response = reqwest::Client::new()
        .post(format!("{address}/v1/devices/typo/recording/start"))
        .send()
        .await
        .expect("issuing the request");

    assert_eq!(response.status().as_u16(), 404);
    let body: serde_json::Value = response.json().await.expect("a body");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|m| m.contains("no device or group 'typo'")),
        "got: {}",
        body["error"]
    );

    // Nothing recorded: a refused submission must leave no command behind, or
    // the relay would dispatch the very write the caller was told was refused.
    use sismatic_store::outbox::CommandLog;
    assert!(
        outbox
            .commands_for("typo".to_owned())
            .await
            .expect("reading the log")
            .is_empty()
    );
}

/// Every write route is guarded, not just the one. They share a `submit`, but
/// a future refactor could give one its own path.
#[tokio::test]
async fn every_write_route_refuses_an_unconfigured_target() {
    let (address, _outbox) = harness::spawn(Arc::new(MemoryStore::default()));
    let client = reqwest::Client::new();
    let base = format!("{address}/v1/devices/typo");

    for path in ["/recording/start", "/recording/stop", "/recording/pause"] {
        let status = client
            .post(format!("{base}{path}"))
            .send()
            .await
            .expect("issuing the request")
            .status()
            .as_u16();
        assert_eq!(status, 404, "POST {path} was not guarded");
    }

    for path in ["/metadata/title", "/settings/timezone"] {
        let status = client
            .put(format!("{base}{path}"))
            .json(&serde_json::json!({"value": "x"}))
            .send()
            .await
            .expect("issuing the request")
            .status()
            .as_u16();
        assert_eq!(status, 404, "PUT {path} was not guarded");
    }
}

/// A group id is addressable too. Group fan-out is not implemented yet, so this
/// pins only that the guard does not reject one — the point being that when
/// fan-out lands, the catalog is already the thing that says a group exists.
#[tokio::test]
async fn a_group_id_passes_the_guard() {
    let (address, _outbox) = harness::spawn(Arc::new(MemoryStore::default()));

    let status = reqwest::Client::new()
        .post(format!("{address}/v1/devices/{GROUP}/recording/start"))
        .send()
        .await
        .expect("issuing the request")
        .status()
        .as_u16();

    assert_eq!(status, 202);
}
