//! tests/reads/ — the `/v1/reads` scope as a client meets it.
//!
//! Black-box, like every suite here: each test starts the real server on an
//! ephemeral port and talks to it over HTTP, so what is pinned is the status
//! code and the JSON on the wire rather than a handler's return type. Nothing
//! below names a handler.
//!
//! The store under the server is the real [`MemoryStore`], not a double. These
//! routes are almost entirely *about* the store's semantics — which field wins,
//! what order a list comes back in, which rows fall inside a span — so a double
//! restating those semantics would be the thing under test rather than the thing
//! being tested against.
//!
//! # The four quarters
//!
//! One suite per URL scope, and one module per route family inside it, which is
//! the shape `src/handlers` has: [`devices`] covers `/v1/reads/devices/…`
//! against `handlers::reads`, [`groups`] covers `/v1/reads/groups/…`
//! against `handlers::group_reads`, [`fleet`] covers the bare
//! `/v1/reads/devices` against `handlers::fleet_reads`, and [`group_fleet`] the
//! bare `/v1/reads/groups` against `handlers::fleet_group_reads`. They are
//! modules of one binary rather
//! than four top-level suites because the helpers below are shared by them, and
//! because a scope is the unit worth being able to run on its own —
//! `cargo test --test reads` is then exactly "the reads scope".
//!
//! A directory with a `main.rs` rather than `reads.rs` beside a `reads/`:
//! Cargo builds both as one test target, but a crate *root* resolves `mod foo;`
//! against its own directory, so `tests/reads.rs` would look for
//! `tests/devices.rs` and find the wrong file — or none.
//!
//! [`MemoryStore`]: sismatic_store_memory::MemoryStore

use std::net::TcpListener;
use std::sync::Arc;

use sismatic_api_types::{DeviceId, FieldName, Read, ReadValue, TimeSpan, Timestamp};
use sismatic_store::{DynReadStore, ReadError, ReadStore, WriteStore};
use sismatic_store_memory::MemoryStore;

// Shared with the other scope suites, so it lives beside them rather than in
// any one of them. `tests/harness/` holds no `main.rs`, which is what keeps
// Cargo from building it as a fourth test binary.
#[path = "../harness/mod.rs"]
mod harness;

mod devices;
mod fleet;
mod group_fleet;
mod groups;

/// The scope every path in this suite is built under.
///
/// Spelled once here rather than inlined into thirty `format!`s, because the
/// scope is the thing the suite is organized around: a route that moved out of
/// it should fail every test in this file at once, not be quietly rewritten
/// thirty times.
const SCOPE: &str = "/v1/reads";

/// The two ids the group tests address, and the group over them.
///
/// Written `[atrium, annex]` — deliberately *not* alphabetical, so a response
/// that came back sorted rather than in configured order fails visibly instead
/// of passing by coincidence.
const ATRIUM: &str = "atrium";
const ANNEX: &str = "annex";
const GROUP: &str = harness::GROUP;

/// A `Read` for `device`/`field` stamped at `at`.
fn read_at(device: &str, field: &str, value: ReadValue, at: &str) -> Read {
    Read {
        device: device.into(),
        field: field.into(),
        value,
        at: Timestamp(at.into()),
    }
}

/// Start the application over `store` on an ephemeral port; return its base URL.
fn spawn_app(store: DynReadStore) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();

    harness::serve(listener, store);

    format!("http://127.0.0.1:{port}")
}

/// Start the application over a [`MemoryStore`] pre-loaded with `reads`, and
/// a catalog of `members` under one device group.
async fn spawn_over(reads: impl IntoIterator<Item = Read>, members: &[&str]) -> String {
    let store = MemoryStore::default();
    for r in reads {
        store.upsert_latest(r).await.expect("seeding the store");
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    harness::serve_with(listener, Arc::new(store), harness::device_group(members));

    format!("http://127.0.0.1:{port}")
}

/// A store whose every read fails, for the tests about a backend outage.
///
/// Shared by the suites rather than declared in each, because "the disk caught
/// fire" is one condition and every route in the scope owes the same answer to
/// it: a `500`, never the `404` that would tell a dashboard to stop asking.
pub struct FailingStore;

#[async_trait::async_trait]
impl ReadStore for FailingStore {
    async fn latest(&self, _dev: DeviceId, _field: FieldName) -> Result<Option<Read>, ReadError> {
        Err(ReadError::backend("the disk caught fire"))
    }

    async fn latest_all(&self, _dev: DeviceId) -> Result<Vec<Read>, ReadError> {
        Err(ReadError::backend("the disk caught fire"))
    }

    async fn between(
        &self,
        _dev: DeviceId,
        _field: FieldName,
        _span: TimeSpan,
    ) -> Result<Vec<Read>, ReadError> {
        Err(ReadError::backend("the disk caught fire"))
    }
}

/// `GET url`, returning the status and the parsed JSON body together — most
/// assertions here are about the pair.
async fn get(url: String) -> (u16, serde_json::Value) {
    let response = reqwest::get(url).await.expect("issuing the request");
    let status = response.status().as_u16();
    let body = response.json().await.expect("parsing the response body");
    (status, body)
}

/// `GET SCOPE+path` and parse its body, asserting `200` first so a failure
/// reports the code rather than a confusing parse error.
async fn get_json(address: &str, path: &str) -> serde_json::Value {
    let (status, body) = get(format!("{address}{SCOPE}{path}")).await;
    assert_eq!(status, 200, "{path} answered {status}: {body}");
    body
}
