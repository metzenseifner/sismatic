//! tests/inventory/ — the `/v1/inventory` scope: what this server was
//! configured with.
//!
//! One thing is worth pinning from out here above all others: the index answers
//! from the *catalog* and not the store, which is what makes a `404` here mean
//! "not in the devices file" rather than "nothing polled yet" — the distinction
//! the readings routes deliberately cannot draw.
//!
//! # What moved out of this suite
//!
//! The catalog also guards the write path: a submission to an id it does not
//! hold is refused before anything is recorded. That guard is exercised through
//! `/v1/writings/…` URLs, so it is tested in `tests/writings/` beside the other
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

use sismatic_api_types::{ConnectionStatus, DeviceSummary, Reading, ReadingValue, Timestamp};
use sismatic_store::{DynReadStore, WriteStore};
use sismatic_store_memory::{MemoryCatalog, MemoryStore};

// See `tests/readings/main.rs` for why this is a `#[path]` and not a plain
// `mod harness;`.
#[path = "../harness/mod.rs"]
mod harness;

mod devices;
mod groups;

/// The scope every path in this suite is built under.
const SCOPE: &str = "/v1/inventory";

const DEVICE: &str = harness::DEVICE;
const GROUP: &str = harness::GROUP;

fn summary(id: &str, host: &str, eager: bool) -> DeviceSummary {
    DeviceSummary {
        id: id.to_owned(),
        host: host.to_owned(),
        port: 22023,
        eager,
        // Always `unknown` in a catalog: it is a snapshot of configuration taken
        // before the process connected to anything. The live value is overlaid
        // by the status port — see `devices::the_index_reports_each_devices_live_connection_state`.
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
