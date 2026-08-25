//! Wiring shared by the black-box suites.
//!
//! [`run`](sismatic_http_api::run) takes six collaborators, and rarely more
//! than one of them is what a given suite is actually about. This module
//! supplies the rest so a test file states the part it cares about and nothing
//! else — and so the next collaborator is one edit here rather than one per
//! suite, which is what this already saved when the catalog arrived.
//!
//! The outbox and the catalog are the real adapters rather than doubles, for
//! the reason `tests/readings/` already gives for using the real
//! `MemoryStore`: a double would have to restate the admission table and the
//! epoch rules, and a test of a handler over a double that drifted from the
//! adapter would pass while the server was wrong.

#![allow(dead_code)] // Each suite uses the part of this it needs.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sismatic_api_types::{Barrier, ConnectionStatus, DeviceSummary, GroupSummary, Timestamp};
use sismatic_http_api::Stamp;
use sismatic_store::group::DynGroupState;
use sismatic_store::outbox::{DynCommandLog, DynCommandSubmit};
use sismatic_store::status::DeviceStatus;
use sismatic_store::{DynDeviceCatalog, DynDeviceStatus, DynReadStore};
use sismatic_store_memory::{MemoryCatalog, MemoryOutbox};

/// The instant every submitted command is stamped with. Fixed, because no
/// assertion here is about time passing, and a real clock would put an
/// unpredictable value in a body a test wants to compare whole.
pub const AT: &str = "2026-08-17T00:00:00.000Z";

/// Ids that count: `cmd-1`, `cmd-2`, … in submission order.
///
/// The whole reason [`Stamp`] is injected. With a UUID a test can assert that
/// *an* id came back and that the `Location` header contains *something*; with
/// a counter it can assert the header names the command the body does, and that
/// a second submission got a second id rather than reusing the first.
pub fn counting_stamp() -> Stamp {
    let issued = AtomicUsize::new(0);
    Stamp::new(move || {
        let n = issued.fetch_add(1, Ordering::SeqCst) + 1;
        (format!("cmd-{n}"), Timestamp(AT.to_owned()))
    })
}

/// The device every suite addresses, and the group over it.
///
/// A catalog with something in it is the default because the write routes now
/// `404` an id they do not recognise: a suite handed an empty catalog would
/// find every `POST` answering `404` for a reason it was not testing.
pub const DEVICE: &str = "atrium-101";
pub const GROUP: &str = "atrium-room";

/// The catalog the suites run against unless they build their own: one device
/// and one group over it.
pub fn catalog() -> MemoryCatalog {
    device_group(&[DEVICE])
}

/// A catalog of one group over `members`, in the order given.
///
/// The order is the point: `MemoryCatalog` sorts *devices* by id and leaves a
/// group's member list alone, so a device group written `[atrium, annex]` reads
/// back in that sequence — and the group routes promise their member lists in
/// exactly that order. A helper that sorted here would make that promise
/// untestable.
pub fn device_group(members: &[&str]) -> MemoryCatalog {
    MemoryCatalog::new(
        members
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

/// Serve `store` and a fresh outbox on `listener`, detached; hand the outbox
/// back so a test can inspect the write side directly.
///
/// The task is dropped rather than joined: dropping a `JoinHandle` leaves the
/// task running, so the server lives exactly as long as the test's runtime and
/// no test has to remember to stop it.
pub fn serve(listener: TcpListener, store: DynReadStore) -> MemoryOutbox {
    serve_with(listener, store, catalog())
}

/// [`serve`] over a stated catalog, for the suites that are about what the
/// catalog does or does not contain.
pub fn serve_with(
    listener: TcpListener,
    store: DynReadStore,
    catalog: MemoryCatalog,
) -> MemoryOutbox {
    serve_with_status(listener, store, catalog, StatedStatus::default())
}

/// [`serve_with`] over a stated connection status, for the suites that are
/// about what the status port reports.
pub fn serve_with_status(
    listener: TcpListener,
    store: DynReadStore,
    catalog: MemoryCatalog,
    status: StatedStatus,
) -> MemoryOutbox {
    let outbox = MemoryOutbox::with_max_attempts(3);
    let catalog: DynDeviceCatalog = Arc::new(catalog);
    let status: DynDeviceStatus = Arc::new(status);
    let submit: DynCommandSubmit = Arc::new(outbox.clone());
    let log: DynCommandLog = Arc::new(outbox.clone());
    // The same object again, as the read-only view of what each device group
    // was told — the real adapter rather than a double, for the reason at the
    // top of this file: a double would have to restate when an expectation is
    // recorded, and that rule lives inside the outbox's admission critical
    // section.
    let group_state: DynGroupState = Arc::new(outbox.clone());

    let server = sismatic_http_api::run(
        listener,
        sismatic_http_api::Ports {
            store,
            catalog,
            status,
            submit,
            log,
            group_state,
        },
        counting_stamp(),
    )
    .expect("building the server");
    drop(tokio::spawn(server));
    outbox
}

/// Bind an ephemeral port, serve on it, and return `(base URL, outbox)`.
///
/// The listener is bound here rather than inside `run` so the port the kernel
/// chose is knowable — that is why `run` takes a [`TcpListener`] at all — and
/// so no two suites can race for a fixed one.
pub fn spawn(store: DynReadStore) -> (String, MemoryOutbox) {
    let (address, outbox, _) = spawn_with(store, catalog());
    (address, outbox)
}

/// [`spawn`] over a stated catalog. Returns the base URL, the outbox and the
/// port the kernel chose.
pub fn spawn_with(store: DynReadStore, catalog: MemoryCatalog) -> (String, MemoryOutbox, u16) {
    spawn_with_status(store, catalog, StatedStatus::default())
}

/// [`spawn_with`] over a stated connection status.
pub fn spawn_with_status(
    store: DynReadStore,
    catalog: MemoryCatalog,
    status: StatedStatus,
) -> (String, MemoryOutbox, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    let outbox = serve_with_status(listener, store, catalog, status);
    (format!("http://127.0.0.1:{port}"), outbox, port)
}

/// A [`DeviceStatus`] that reports whatever it was told to.
///
/// The one collaborator here that is a double rather than the real adapter, and
/// for a reason the others do not have: the real one reads a `sismatic-core`
/// `Registry`, which this crate may not name. That is the seam working — a test
/// of these routes states the status it wants observed and asserts it comes
/// back, which is the whole of what the routes are responsible for.
#[derive(Debug, Default, Clone)]
pub struct StatedStatus(pub std::collections::BTreeMap<String, ConnectionStatus>);

impl StatedStatus {
    pub fn of(pairs: &[(&str, ConnectionStatus)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(id, status)| ((*id).to_owned(), *status))
                .collect(),
        )
    }
}

#[async_trait::async_trait]
impl DeviceStatus for StatedStatus {
    async fn status(&self, id: &str) -> ConnectionStatus {
        self.0.get(id).copied().unwrap_or(ConnectionStatus::Unknown)
    }

    async fn all(&self) -> std::collections::BTreeMap<String, ConnectionStatus> {
        self.0.clone()
    }
}
