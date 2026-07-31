//! Assembling the application: routes, shared state, and the socket it is
//! served on.
//!
//! [`run`] is deliberately *not* `async` and deliberately does not await the
//! server it builds. It returns the running [`Server`] to its caller, who decides
//! what to do with it — the composition root awaits it alongside a shutdown
//! signal, an integration test spawns it and then talks to it over a socket.
//! Awaiting inside would make the two indistinguishable and the second
//! impossible: a test that called a `run` which only returns when the server
//! stops would hang before it could issue a request.
//!
//! For the same reason the [`TcpListener`] is an argument rather than a host and
//! a port. Binding is the caller's step, so the caller is the one holding the
//! socket and can ask it what it bound to. That is what makes `127.0.0.1:0` —
//! "any free port", the kernel picks — usable in a test: bind, read
//! `local_addr()`, hand the listener over, then address the port the kernel
//! chose. Tests therefore never race each other for a fixed port and never need
//! one reserved for them.

use std::net::TcpListener;

use actix_web::dev::Server;
use actix_web::{App, HttpServer, web};
use sismatic_store::{DynReadStore, ReadStore};

use crate::routes::health_check;

/// Build the application over `store`, serve it on `listener`, and hand the
/// running server back.
///
/// Fails only if the listener cannot be turned into a serving socket; every
/// later failure belongs to the returned [`Server`], which resolves when it
/// stops.
pub fn run(listener: TcpListener, store: DynReadStore) -> Result<Server, std::io::Error> {
    // Adopt the caller's `Arc` instead of wrapping it: `Data::new` would give
    // handlers an `Arc<Arc<dyn ReadStore>>` to dereference twice, and would make
    // the type say the API owns a store rather than shares one.
    let store: web::Data<dyn ReadStore> = web::Data::from(store);

    let server = HttpServer::new(move || {
        // Run once per worker thread, so everything captured here is cloned per
        // worker — hence the `Data` handle built *outside* the closure. Building
        // it inside would give each worker its own store.
        App::new().app_data(store.clone()).service(
            // A `resource` rather than `App::route`, which is otherwise the
            // same thing spelled shorter: `route` hoists the method guard onto
            // the resource, so a request with the wrong method fails to match
            // the *path* and is answered 404. Keeping the guard on the route
            // means the path matches and the method does not, which is what
            // 405 with an `Allow` header says — the answer that tells a
            // misconfigured probe what to change.
            web::resource("/health_check").route(web::get().to(health_check)),
        )
    })
    .listen(listener)?
    // The composition root owns the process's shutdown signal (it has a sync
    // driver to stop as well, in an order this crate knows nothing about).
    // Left enabled, actix installs its own SIGINT/SIGTERM handlers and two
    // owners race for one ctrl-c.
    .disable_signals()
    .run();

    Ok(server)
}
