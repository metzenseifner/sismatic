//! tests/inventory/ — the `/v1/inventory` scope: what this server was
//! configured with.
//!
//! One thing is worth pinning from out here above all others: the index answers
//! from the *catalog* and not the store, which is what makes a `404` here mean
//! "not in the devices file" rather than "nothing polled yet" — the distinction
//! the reads routes deliberately cannot draw.
//!
//! # What moved out of this suite
//!
//! The catalog also guards the write path: a submission to an id it does not
//! hold is refused before anything is recorded. That guard is exercised through
//! `/v1/writes/…` URLs, so it is tested in `tests/writes/` beside the other
//! things those routes do — a suite is organized by the scope it addresses, not
//! by which port happens to be load-bearing. What stays here is the `/groups`
//! route's `barrier_timeout_secs` and `barrier`, because those are configuration
//! this scope reports and nothing else does.
//!
//! # The two halves
//!
//! [`devices`] covers `/v1/inventory/devices…` and [`groups`] covers
//! `/v1/inventory/groups…`, both against `handlers::devices`.

use std::sync::Arc;

use sismatic_api_types::{ConnectionStatus, DeviceSummary, Read, ReadValue, Timestamp};
use sismatic_store::{DynReadStore, WriteStore};
use sismatic_store_memory::{MemoryCatalog, MemoryStore};

// See `tests/reads/main.rs` for why this is a `#[path]` and not a plain
// `mod harness;`.
#[path = "../harness/mod.rs"]
mod harness;

mod devices;
mod groups;
mod mutations;

/// The scope every path in this suite is built under.
const SCOPE: &str = "/v1/inventory";

const DEVICE: &str = harness::DEVICE;
const GROUP: &str = harness::GROUP;

fn summary(id: &str, host: &str, eager: bool) -> DeviceSummary {
    DeviceSummary {
        id: id.to_owned(),
        uuid: format!("00000000-0000-0000-0000-{:012x}", id.len()),
        host: host.to_owned(),
        port: 22023,
        eager,
        // Always `unknown` in a catalog: it is a snapshot of configuration taken
        // before the process connected to anything. The live value is overlaid
        // by the status port — see `devices::the_index_reports_each_devices_live_connection_state`.
        status: ConnectionStatus::Unknown,
        disabled_fields: Vec::new(),
        auto_disabled_fields: Vec::new(),
    }
}

/// A store holding one read of `FIRMWARE` on [`DEVICE`], so the detail route
/// has something to join against.
async fn seeded_store() -> DynReadStore {
    let store = MemoryStore::default();
    store
        .upsert_latest(Read {
            device: DEVICE.into(),
            field: "FIRMWARE".into(),
            value: ReadValue::Version("2.11".into()),
            at: Timestamp("2026-07-23T14:03:11Z".into()),
        })
        .await
        .expect("seeding the store");
    Arc::new(store)
}

/// An empty store, for the many tests that are about the catalog alone.
fn empty_store() -> DynReadStore {
    Arc::new(MemoryStore::default())
}

/// Serve `catalog` over an empty store; return the base URL.
fn spawn_with(catalog: MemoryCatalog) -> String {
    let (address, ..) = harness::spawn_with(empty_store(), catalog);
    address
}

/// [`spawn_with`] over a stated connection status.
fn spawn_with_status(catalog: MemoryCatalog, status: harness::StatedStatus) -> String {
    let (address, ..) = harness::spawn_with_status(empty_store(), catalog, status);
    address
}

/// `GET SCOPE+path`, returning the status and the parsed JSON body — every
/// assertion in this suite is about the pair.
///
/// `path` is relative to [`SCOPE`]: the scope is what the suite is organized
/// around, so a route that moved out of it should fail every test here at once.
async fn get(address: &str, path: &str) -> (u16, serde_json::Value) {
    let response = reqwest::get(format!("{address}{SCOPE}{path}"))
        .await
        .expect("issuing the request");
    let status = response.status().as_u16();
    let body = response.json().await.expect("parsing the response body");
    (status, body)
}

/// `POST`, `PUT` or `DELETE` `SCOPE+path` with an optional JSON body, returning
/// the status, the `Location` header and the parsed body.
///
/// One helper for the three mutating verbs because every assertion about them
/// is the same triple, and three near-identical functions would be three places
/// for the scope prefix to drift.
async fn send(
    address: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, Option<String>, serde_json::Value) {
    let mut request = reqwest::Client::new().request(method, format!("{address}{SCOPE}{path}"));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("issuing the request");
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .map(|v| v.to_str().expect("a text header").to_owned());
    // Every route in this suite answers JSON, including its failures — a body
    // that will not parse is itself the finding.
    let body = response.json().await.expect("parsing the response body");
    (status, location, body)
}

async fn post(
    address: &str,
    path: &str,
    body: serde_json::Value,
) -> (u16, Option<String>, serde_json::Value) {
    send(address, reqwest::Method::POST, path, Some(body)).await
}

async fn put(
    address: &str,
    path: &str,
    body: serde_json::Value,
) -> (u16, Option<String>, serde_json::Value) {
    send(address, reqwest::Method::PUT, path, Some(body)).await
}

async fn delete(address: &str, path: &str) -> (u16, Option<String>, serde_json::Value) {
    send(address, reqwest::Method::DELETE, path, None).await
}

/// The smallest body that describes a device: the two keys with no default.
fn minimal(id: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "host": "10.0.0.9"})
}
