//! tests/writes/ — the `/v1/writes` scope, black-box over a real socket.
//!
//! Over the real [`MemoryOutbox`] rather than a double, for the reason
//! `tests/reads/` gives for using the real `MemoryStore`: a double would have
//! to restate the admission table and the epoch rules, and a handler tested
//! against a drifted double passes while the server is wrong.
//!
//! # What is asserted here, and what is not
//!
//! This suite is about the HTTP surface: status codes, headers, bodies, and the
//! translation from a URL to an [`Intent`]. Whether the freeze rule is *right*
//! is `sismatic-store`'s admission-table test, and whether the outbox enforces
//! it atomically is `sismatic-store-memory`'s. What is left over — and only
//! testable from out here — is that a `PUT` on the metadata path builds a
//! `SetMetadata` and not a `SetSetting`, that a refusal becomes a `409` and not
//! a `500`, and that the `202` names a write a caller can then fetch.
//!
//! Nothing here reaches a device: there is no relay in this process, so every
//! submitted write stays `pending` forever. That is the point — the `202`
//! means recorded, and this suite is what pins that it means only that.
//!
//! # The two halves
//!
//! [`devices`] covers `/v1/writes/devices/…` and the scope-root
//! `/v1/writes/{id}`; [`groups`] covers `/v1/writes/groups/…`. The split
//! mirrors the id-space rather than the source file — `handlers::writes` holds
//! both — because a device id and a group id are what the two halves refuse
//! from each other, and each half's refusal is its own test.
//!
//! [`Intent`]: sismatic_api_types::Intent
//! [`MemoryOutbox`]: sismatic_store_memory::MemoryOutbox

use std::sync::Arc;

use sismatic_api_types::Intent;
use sismatic_store::DynReadStore;
use sismatic_store_memory::{MemoryOutbox, MemoryStore};

// See `tests/reads/main.rs` for why this is a `#[path]` and not a plain
// `mod harness;`.
#[path = "../harness/mod.rs"]
mod harness;

mod devices;
mod groups;

/// The scope every path in this suite is built under.
///
/// The write surface is addressed by *what it does* rather than by what it
/// names, so both id-spaces and the single-write route live here together.
const SCOPE: &str = "/v1/writes";

/// The device the [`devices`] half addresses — the harness catalog's, so the
/// write routes recognise it.
const DEVICE: &str = harness::DEVICE;

/// The two members the [`groups`] half addresses, and the group over them.
///
/// Written `[atrium, annex]` — deliberately *not* alphabetical, so an expansion
/// that came back sorted rather than in configured order fails visibly instead
/// of passing by coincidence.
const ATRIUM: &str = "atrium";
const ANNEX: &str = "annex";
const GROUP: &str = harness::GROUP;

/// Start the application over an empty read store and the harness catalog;
/// return the base URL and the outbox behind the write routes.
fn spawn_app() -> (String, MemoryOutbox) {
    let store: DynReadStore = Arc::new(MemoryStore::default());
    harness::spawn(store)
}

/// [`spawn_app`] over a device group of `members`, in the order given.
fn spawn_over(members: &[&str]) -> (String, MemoryOutbox) {
    let store: DynReadStore = Arc::new(MemoryStore::default());
    let (address, outbox, _) = harness::spawn_with(store, harness::device_group(members));
    (address, outbox)
}

/// `POST base+SCOPE+path`, returning the status, the `Location` header and the
/// body.
///
/// `path` is relative to [`SCOPE`] throughout this suite: the scope is the thing
/// the suite is organized around, so a route that moved out of it should fail
/// every test here at once rather than be quietly rewritten in thirty places.
async fn post(base: &str, path: &str) -> (u16, Option<String>, serde_json::Value) {
    send(reqwest::Client::new().post(url(base, path))).await
}

/// `PUT base+SCOPE+path` with a `{"value": ...}` body.
async fn put(base: &str, path: &str, value: &str) -> (u16, Option<String>, serde_json::Value) {
    send(
        reqwest::Client::new()
            .put(url(base, path))
            .json(&serde_json::json!({ "value": value })),
    )
    .await
}

async fn get(base: &str, path: &str) -> (u16, serde_json::Value) {
    let (status, _, body) = send(reqwest::Client::new().get(url(base, path))).await;
    (status, body)
}

async fn send(request: reqwest::RequestBuilder) -> (u16, Option<String>, serde_json::Value) {
    let response = request.send().await.expect("issuing the request");
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // Every route here answers JSON, including the failures — an empty body
    // would itself be the bug.
    let body = response.json().await.expect("parsing the response body");
    (status, location, body)
}

fn url(base: &str, path: &str) -> String {
    format!("{base}{SCOPE}{path}")
}

/// Every intent the outbox holds for `device`, oldest first.
///
/// Reads the port directly rather than the `history` route: this is the
/// assertion about *what was recorded*, and routing it through a second handler
/// would make a failure ambiguous between the two.
async fn recorded_intents(outbox: &MemoryOutbox, device: &str) -> Vec<Intent> {
    use sismatic_store::outbox::WriteLog;
    let mut writes = outbox
        .writes_for(device.to_owned())
        .await
        .expect("reading the log");
    writes.reverse(); // the port promises newest-first
    writes.into_iter().map(|w| w.intent).collect()
}
