//! `/v1/inventory/devices…` — the device index and one device's detail page.

use sismatic_api_types::ConnectionStatus;
use sismatic_store_memory::MemoryCatalog;

use crate::{
    DEVICE, GROUP, SCOPE, get, harness, seeded_store, spawn_with, spawn_with_status, summary,
};

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
    let address = spawn_with(catalog);

    let (status, body) = get(&address, "/devices").await;

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
/// are configured", where an empty reads list means "none have answered".
#[tokio::test]
async fn an_empty_catalog_is_an_empty_index_not_an_error() {
    let address = spawn_with(MemoryCatalog::default());

    assert_eq!(
        get(&address, "/devices").await,
        (200, serde_json::json!({"devices": []}))
    );
    assert_eq!(
        get(&address, "/groups").await,
        (200, serde_json::json!({"groups": []}))
    );
}

/// No credential can reach the wire, because `DeviceSummary` has no field to
/// carry one — a stronger guarantee than redacting at serialization time.
#[tokio::test]
async fn the_index_carries_no_credentials() {
    let address = spawn_with(harness::catalog());

    let (_, body) = get(&address, "/devices").await;

    let rendered = body.to_string();
    for secret in ["username", "password", "admin", "extron"] {
        assert!(
            !rendered.contains(secret),
            "'{secret}' reached the wire: {rendered}"
        );
    }
}

// ---- one device --------------------------------------------------------

/// The route that joins the two sides: the catalog says what exists, the store
/// says what it last reported.
#[tokio::test]
async fn the_detail_route_joins_the_catalog_and_the_store() {
    let (address, ..) = harness::spawn(seeded_store().await);

    let (status, body) = get(&address, &format!("/devices/{DEVICE}")).await;

    assert_eq!(status, 200);
    assert_eq!(body["device"]["id"], DEVICE);
    assert_eq!(body["latest"][0]["field"], "FIRMWARE");
    assert_eq!(body["latest"][0]["value"]["value"], "2.11");
}

/// A device that is configured but has never answered is a `200` with nothing
/// in `latest` — which is what lets a dashboard render its row at all.
#[tokio::test]
async fn a_device_that_has_never_answered_still_has_a_detail_page() {
    let address = spawn_with(harness::catalog());

    let (status, body) = get(&address, &format!("/devices/{DEVICE}")).await;

    assert_eq!(status, 200);
    assert_eq!(body["latest"], serde_json::json!([]));
}

/// The divergence between this scope and the reads scope, stated in one
/// test because it is the reason both exist.
#[tokio::test]
async fn an_unconfigured_device_is_a_404_here_and_an_empty_list_on_the_reads_route() {
    let address = spawn_with(harness::catalog());

    // The catalog knows the configured set, so it can say this id is not in it.
    let (status, body) = get(&address, "/devices/nobody").await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], "not_found");

    // The store cannot: "no such device" and "never polled" are one answer from
    // there, so it answers the honest, weaker one. The divergence is deliberate.
    let reads: serde_json::Value =
        reqwest::get(format!("{address}/v1/reads/devices/nobody/fields"))
            .await
            .expect("issuing the request")
            .json()
            .await
            .expect("parsing the response body");
    assert_eq!(reads, serde_json::json!({"reads": []}));
}

/// A device group id is refused on the device half of this scope, with the
/// `/groups` URL that answers instead — the same pairing every other scope
/// enforces, and the refusal has to name a URL *inside this scope* or it sends
/// the caller to a second 404.
#[tokio::test]
async fn a_group_id_on_the_device_route_says_which_route_to_use() {
    let address = spawn_with(harness::catalog());

    let (status, body) = get(&address, &format!("/devices/{GROUP}")).await;

    assert_eq!(status, 404);
    let message = body["error"].as_str().expect("an error message");
    assert!(
        message.contains("is a device group")
            && message.contains(&format!("{SCOPE}/groups/{GROUP}")),
        "got: {message}"
    );
}

// ---- live connection status --------------------------------------------

/// The gap the status port closes. Before it, every device reported `unknown`
/// because the catalog is a snapshot of configuration taken before the process
/// connects to anything.
#[tokio::test]
async fn the_index_reports_each_devices_live_connection_state() {
    let catalog = MemoryCatalog::new(
        vec![
            summary("warm-one", "10.0.0.1", true),
            summary("down-one", "10.0.0.2", false),
            summary("idle-one", "10.0.0.3", false),
        ],
        vec![],
    );
    let status = harness::StatedStatus::of(&[
        ("warm-one", ConnectionStatus::Warm),
        ("down-one", ConnectionStatus::Gated),
        ("idle-one", ConnectionStatus::Cold),
    ]);
    let address = spawn_with_status(catalog, status);

    let (code, body) = get(&address, "/devices").await;

    assert_eq!(code, 200);
    let states: Vec<(&str, &str)> = body["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .map(|d| {
            (
                d["id"].as_str().expect("id"),
                d["status"].as_str().expect("status"),
            )
        })
        .collect();
    // `gated` is the one that matters operationally: it says the device is
    // *down*, where `cold` says only that nothing has connected to it yet.
    assert_eq!(
        states,
        [
            ("down-one", "gated"),
            ("idle-one", "cold"),
            ("warm-one", "warm"),
        ]
    );
}

#[tokio::test]
async fn the_detail_route_reports_the_live_state_too() {
    let catalog = MemoryCatalog::new(vec![summary("busy-one", "10.0.0.1", false)], vec![]);
    let status = harness::StatedStatus::of(&[("busy-one", ConnectionStatus::Busy)]);
    let address = spawn_with_status(catalog, status);

    let (code, body) = get(&address, "/devices/busy-one").await;

    assert_eq!(code, 200);
    assert_eq!(body["device"]["status"], "busy");
}

/// A device the status port does not know about keeps `unknown` rather than
/// being invented as `cold`. The two sources are built from one config so it
/// cannot happen in the server — but the route must not paper over it if it
/// ever does, because `cold` is a claim and `unknown` is an admission.
#[tokio::test]
async fn a_device_the_status_port_does_not_know_stays_unknown() {
    let catalog = MemoryCatalog::new(vec![summary("ghost", "10.0.0.1", false)], vec![]);
    let address = spawn_with_status(catalog, harness::StatedStatus::default());

    let (_, body) = get(&address, "/devices").await;
    assert_eq!(body["devices"][0]["status"], "unknown");

    let (_, body) = get(&address, "/devices/ghost").await;
    assert_eq!(body["device"]["status"], "unknown");
}
