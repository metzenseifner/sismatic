//! Wiring shared by the black-box suites.
//!
//! [`run`](sismatic_http_api::run) takes five collaborators, and rarely more
//! than one of them is what a given suite is actually about. This module
//! supplies the rest so a test file states the part it cares about and nothing
//! else — and so the next collaborator is one edit here rather than one per
//! suite, which is what this already saved when the catalog arrived.
//!
//! The outbox and the catalog are the real adapters rather than doubles, for
//! the reason `tests/readings.rs` already gives for using the real
//! `MemoryStore`: a double would have to restate the admission table and the
//! epoch rules, and a test of a handler over a double that drifted from the
//! adapter would pass while the server was wrong.

#![allow(dead_code)] // Each suite uses the part of this it needs.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sismatic_api_types::{ConnectionStatus, DeviceSummary, GroupSummary, Timestamp};
use sismatic_http_api::Stamp;
use sismatic_store::outbox::{DynCommandLog, DynCommandSubmit};
use sismatic_store::{DynDeviceCatalog, DynReadStore};
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
    MemoryCatalog::new(
        vec![DeviceSummary {
            id: DEVICE.to_owned(),
            host: "10.0.0.7".to_owned(),
            port: 22023,
            eager: false,
            status: ConnectionStatus::Unknown,
        }],
        vec![GroupSummary {
            id: GROUP.to_owned(),
            members: vec![DEVICE.to_owned()],
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
    let outbox = MemoryOutbox::with_max_attempts(3);
    let catalog: DynDeviceCatalog = Arc::new(catalog);
    let submit: DynCommandSubmit = Arc::new(outbox.clone());
    let log: DynCommandLog = Arc::new(outbox.clone());

    let server = sismatic_http_api::run(listener, store, catalog, submit, log, counting_stamp())
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
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    let outbox = serve_with(listener, store, catalog);
    (format!("http://127.0.0.1:{port}"), outbox, port)
}
