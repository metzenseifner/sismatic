//! `POST`, `PUT` and `DELETE` under `/v1/inventory/devices` — the routes that
//! change which devices exist.
//!
//! Over a [`StatedInventory`] double rather than the real adapter, for the
//! reason the whole suite uses doubles: the real one is the composition root,
//! which owns a registry, an outbox and a store that this crate may not name.
//! What these routes are responsible for is the URL space, the status codes and
//! the bodies — whether the *removal sequence* is correctly ordered is
//! `sismatic-server`'s test to write, and it is written there.
//!
//! [`StatedInventory`]: harness::StatedInventory

use crate::{delete, harness, minimal, post, put};

// ---- adding ------------------------------------------------------------

#[tokio::test]
async fn adding_a_device_answers_201_with_its_detail_route() {
    let (address, inventory) = harness::spawn_with_inventory(harness::StatedInventory::default());

    let (status, location, body) = post(&address, "/devices", minimal("new-wing")).await;

    assert_eq!(status, 201);
    assert_eq!(
        location.as_deref(),
        Some("/v1/inventory/devices/new-wing"),
        "a 201 names where the thing it created can be read"
    );
    assert_eq!(body["id"], "new-wing");
    assert_eq!(
        inventory.calls(),
        ["add new-wing"],
        "the route must reach the port rather than answering by itself"
    );
}

/// The body is the only place an added device's id can come from — the URL does
/// not name one — so a body without it is the caller's mistake.
#[tokio::test]
async fn adding_a_device_without_an_id_is_a_400() {
    let (address, _) = harness::spawn_with_inventory(harness::StatedInventory::default());

    let (status, _, body) = post(
        &address,
        "/devices",
        serde_json::json!({"host": "10.0.0.9"}),
    )
    .await;

    assert_eq!(status, 400);
    assert_eq!(body["code"], "bad_request");
}

/// An id that already exists is a conflict rather than a bad request: the body
/// is well-formed and is exactly what a caller would send to *replace* the
/// device, so the message names that route.
#[tokio::test]
async fn adding_a_device_that_already_exists_is_a_409_naming_the_put_route() {
    let (address, _) = harness::spawn_with_inventory(harness::StatedInventory::with(&["taken"]));

    let (status, _, body) = post(&address, "/devices", minimal("taken")).await;

    assert_eq!(status, 409);
    assert_eq!(body["code"], "conflict");
}

/// A key this server does not know is refused rather than ignored. `DeviceWrite`
/// is `deny_unknown_fields`, so a typo in a key name cannot be silently dropped
/// and leave the device configured differently than the caller believes.
#[tokio::test]
async fn an_unknown_key_in_the_body_is_refused() {
    let (address, _) = harness::spawn_with_inventory(harness::StatedInventory::default());

    let (status, _, body) = post(
        &address,
        "/devices",
        serde_json::json!({"id": "typo", "host": "10.0.0.9", "connect_seconds": 5}),
    )
    .await;

    assert_eq!(status, 400, "`connect_seconds` is not `connect_secs`");
    // The error envelope, not actix's plain text: a body that will not
    // deserialize is the one failure a client is least likely to have tested,
    // so it must not be the one that answers something unparseable.
    assert_eq!(body["code"], "bad_request");
    assert!(
        body["error"]
            .as_str()
            .expect("error")
            .contains("connect_seconds"),
        "the message should name the key that was not recognised: {body}"
    );
}

// ---- replacing ---------------------------------------------------------

#[tokio::test]
async fn replacing_a_device_answers_200_with_the_new_device() {
    let (address, inventory) =
        harness::spawn_with_inventory(harness::StatedInventory::with(&["edited"]));

    let (status, _, body) = put(
        &address,
        "/devices/edited",
        serde_json::json!({"host": "10.0.0.8", "eager": true}),
    )
    .await;

    assert_eq!(status, 200);
    assert_eq!(body["id"], "edited");
    assert_eq!(inventory.calls(), ["replace edited"]);
}

#[tokio::test]
async fn replacing_a_device_that_does_not_exist_is_a_404() {
    let (address, _) = harness::spawn_with_inventory(harness::StatedInventory::default());

    let (status, _, body) = put(&address, "/devices/ghost", minimal("ghost")).await;

    assert_eq!(status, 404);
    assert_eq!(body["code"], "unknown_device");
}

/// The id is stated once. A body that disagrees with the path is refused rather
/// than resolved in favour of either, because there is no reading of that
/// request where the caller got what it meant.
#[tokio::test]
async fn a_body_whose_id_disagrees_with_the_path_is_a_400() {
    let (address, _) = harness::spawn_with_inventory(harness::StatedInventory::with(&["edited"]));

    let (status, _, body) = put(&address, "/devices/edited", minimal("somebody-else")).await;

    assert_eq!(status, 400);
    assert!(
        body["error"].as_str().expect("error").contains("edited"),
        "the message should name both ids: {body}"
    );
}

/// Stating the id and agreeing with the path is fine — the round trip
/// `GET`-edit-`PUT` sends back what it read, and that includes the id.
#[tokio::test]
async fn a_body_whose_id_matches_the_path_is_accepted() {
    let (address, _) = harness::spawn_with_inventory(harness::StatedInventory::with(&["edited"]));

    let (status, _, _) = put(&address, "/devices/edited", minimal("edited")).await;

    assert_eq!(status, 200);
}

// ---- removing ----------------------------------------------------------

/// Removal reports what went with the device, because a caller cannot otherwise
/// discover it: writes it submitted are now terminal.
#[tokio::test]
async fn removing_a_device_reports_what_went_with_it() {
    let (address, inventory) =
        harness::spawn_with_inventory(harness::StatedInventory::with(&["goner"]));

    let (status, _, body) = delete(&address, "/devices/goner").await;

    assert_eq!(status, 200);
    assert_eq!(body["device"], "goner");
    assert_eq!(body["writes_canceled"], 3);
    assert!(
        body["reads_dropped"].is_null(),
        "null is `cleanup_on_remove` off — distinct from a count of zero"
    );
    assert_eq!(inventory.calls(), ["remove goner"]);
}

#[tokio::test]
async fn removing_a_device_that_does_not_exist_is_a_404() {
    let (address, _) = harness::spawn_with_inventory(harness::StatedInventory::default());

    let (status, _, body) = delete(&address, "/devices/ghost").await;

    assert_eq!(status, 404);
    assert_eq!(body["code"], "unknown_device");
}

/// A device a group still names is refused, not cascaded. Cascading would
/// silently change what the group means, and the message says what to do
/// instead.
#[tokio::test]
async fn removing_a_device_a_group_still_names_is_a_409() {
    let (address, _) =
        harness::spawn_with_inventory(harness::StatedInventory::holding(&["member"], &["member"]));

    let (status, _, body) = delete(&address, "/devices/member").await;

    assert_eq!(status, 409);
    assert_eq!(body["code"], "conflict");
    let message = body["error"].as_str().expect("error");
    assert!(
        message.contains("group") && message.contains("first"),
        "the refusal should say to edit the group first: {message}"
    );
}

/// The read routes still work on the same paths. A `GET` is not a `DELETE`, and
/// registering three verbs on one resource must not have shadowed the first.
#[tokio::test]
async fn the_read_routes_still_answer_on_the_same_paths() {
    let (address, _) = harness::spawn_with_inventory(harness::StatedInventory::default());

    let (status, _) = crate::get(&address, "/devices").await;
    assert_eq!(status, 200);
}

// ---- export and reset --------------------------------------------------

/// The route serves the port's bytes verbatim, with the content type and a
/// filename whose extension the devices-file loader dispatches on — which is
/// what makes an export savable and loadable rather than merely readable.
#[tokio::test]
async fn exporting_serves_the_document_with_a_loadable_filename() {
    let (address, inventory) = harness::spawn_with_inventory(harness::StatedInventory::default());

    let response = reqwest::get(format!("{address}/v1/inventory/devices/export"))
        .await
        .expect("issuing the request");

    assert_eq!(response.status().as_u16(), 200);
    assert!(
        response
            .headers()
            .get("content-disposition")
            .expect("a filename")
            .to_str()
            .expect("text")
            .contains("devices.toml"),
        "the default format is TOML, and the name has to carry the extension"
    );
    assert!(response.text().await.expect("body").contains("Toml"));
    assert_eq!(
        inventory.calls(),
        ["export Toml promote=false secrets=false"],
        "both switches default to off"
    );
}

/// The two switches reach the port. `include_secrets` defaulting off is the one
/// that matters: an export lands in shell history and CI logs.
#[tokio::test]
async fn the_export_switches_reach_the_port() {
    let (address, inventory) = harness::spawn_with_inventory(harness::StatedInventory::default());

    let response = reqwest::get(format!(
        "{address}/v1/inventory/devices/export\
         ?format=json&promote_auto_disabled_fields_to_disabled_fields=true&include_secrets=true"
    ))
    .await
    .expect("issuing the request");

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .expect("a content type")
            .to_str()
            .expect("text"),
        "application/json"
    );
    assert_eq!(inventory.calls(), ["export Json promote=true secrets=true"]);
}

/// `export` is a literal path segment, and it is registered before
/// `/devices/{id}` so the parameterized route cannot swallow it. Without that
/// ordering this request reads a device named "export".
#[tokio::test]
async fn the_export_path_is_not_captured_as_a_device_id() {
    let (address, inventory) = harness::spawn_with_inventory(harness::StatedInventory::default());

    let response = reqwest::get(format!("{address}/v1/inventory/devices/export"))
        .await
        .expect("issuing the request");

    assert_eq!(response.status().as_u16(), 200);
    assert!(
        inventory
            .calls()
            .iter()
            .all(|call| call.starts_with("export")),
        "the detail route must not have been reached: {:?}",
        inventory.calls()
    );
}

/// An unreadable query is the caller's mistake, and answers the error envelope
/// rather than actix's plain text — the same treatment a malformed body gets.
#[tokio::test]
async fn an_unknown_export_format_is_a_400() {
    let (address, _) = harness::spawn_with_inventory(harness::StatedInventory::default());

    let response = reqwest::get(format!("{address}/v1/inventory/devices/export?format=xml"))
        .await
        .expect("issuing the request");

    assert_eq!(response.status().as_u16(), 400);
}

#[tokio::test]
async fn resetting_reports_the_fleet_it_ended_with() {
    let (address, inventory) =
        harness::spawn_with_inventory(harness::StatedInventory::with(&["a", "b"]));

    let (status, _, body) = crate::send(&address, reqwest::Method::POST, "/reset", None).await;

    assert_eq!(status, 200);
    let ids: Vec<&str> = body["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .map(|d| d["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, ["a", "b"], "the fleet as the index now reports it");
    assert_eq!(inventory.calls(), ["reset"]);
}
