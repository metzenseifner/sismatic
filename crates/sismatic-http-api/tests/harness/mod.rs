//! Wiring shared by the black-box suites.
//!
//! [`run`](sismatic_http_api::run) takes four collaborators, and only one of
//! them — the read store — is what any given suite is actually about. This
//! module supplies the other three so a test file states the part it cares
//! about and nothing else, and so adding a fifth collaborator is one edit
//! rather than four.
//!
//! The outbox is the real [`MemoryOutbox`] rather than a double, for the reason
//! `tests/readings.rs` already gives for using the real `MemoryStore`: a double
//! would have to restate the admission table and the epoch rules, and a test of
//! a handler over a double that drifted from the adapter would pass while the
//! server was wrong.

#![allow(dead_code)] // Each suite uses the part of this it needs.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sismatic_api_types::Timestamp;
use sismatic_http_api::Stamp;
use sismatic_store::DynReadStore;
use sismatic_store::outbox::{DynCommandLog, DynCommandSubmit};
use sismatic_store_memory::MemoryOutbox;

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

/// Serve `store` and a fresh outbox on `listener`, detached; hand the outbox
/// back so a test can inspect the write side directly.
///
/// The task is dropped rather than joined: dropping a `JoinHandle` leaves the
/// task running, so the server lives exactly as long as the test's runtime and
/// no test has to remember to stop it.
pub fn serve(listener: TcpListener, store: DynReadStore) -> MemoryOutbox {
    let outbox = MemoryOutbox::with_max_attempts(3);
    let submit: DynCommandSubmit = Arc::new(outbox.clone());
    let log: DynCommandLog = Arc::new(outbox.clone());

    let server = sismatic_http_api::run(listener, store, submit, log, counting_stamp())
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
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    let outbox = serve(listener, store);
    (format!("http://127.0.0.1:{port}"), outbox)
}
