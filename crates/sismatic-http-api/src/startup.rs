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
use sismatic_store::outbox::{CommandLog, CommandSubmit, DynCommandLog, DynCommandSubmit};
use sismatic_store::{DynReadStore, ReadStore};
use utoipa_swagger_ui::SwaggerUi;

use crate::openapi::{ApiDoc, OPENAPI_JSON_PATH, SWAGGER_UI_PATH};
use crate::routes::{
    field_history, health_check, list_commands, list_fields, pause_recording, read_command,
    read_field, read_phase, set_metadata, set_setting, start_recording, stop_recording,
};
use crate::stamp::Stamp;

/// Build the application over the three ports, serve it on `listener`, and hand
/// the running server back.
///
/// Fails only if the listener cannot be turned into a serving socket; every
/// later failure belongs to the returned [`Server`], which resolves when it
/// stops.
///
/// # What the argument list narrows
///
/// Four capabilities, each the smallest one its handlers need. `submit` may
/// append an intent and nothing else — it cannot dispatch one, settle one, or
/// touch a reading. There is deliberately no `CommandDrain` here: draining is
/// `sismatic-intent-relay`'s, and a handler that could claim a command could
/// reorder a device's queue. And still no [`WriteStore`], so the readings
/// routes remain unable to write no matter what they ask for.
///
/// That is the same narrowing the read side already relied on, extended rather
/// than relaxed: the write side gained exactly one verb, and it is one that
/// records a request rather than performs it.
///
/// [`WriteStore`]: sismatic_store::WriteStore
pub fn run(
    listener: TcpListener,
    store: DynReadStore,
    submit: DynCommandSubmit,
    log: DynCommandLog,
    stamp: Stamp,
) -> Result<Server, std::io::Error> {
    // Adopt the caller's `Arc` instead of wrapping it: `Data::new` would give
    // handlers an `Arc<Arc<dyn ReadStore>>` to dereference twice, and would make
    // the type say the API owns a store rather than shares one.
    let store: web::Data<dyn ReadStore> = web::Data::from(store);
    let submit: web::Data<dyn CommandSubmit> = web::Data::from(submit);
    let log: web::Data<dyn CommandLog> = web::Data::from(log);
    // `Data::new` here and not `Data::from`: the stamp arrives owned, because
    // the composition root has no reason to keep a handle to it.
    let stamp = web::Data::new(stamp);

    // Built once and cloned per worker, for the same reason the store handle is:
    // the document is identical on every worker, and assembling it inside the
    // closure would rebuild the whole thing once per thread at startup.
    let api_doc = ApiDoc::document();

    let server = HttpServer::new(move || {
        // Run once per worker thread, so everything captured here is cloned per
        // worker — hence the `Data` handle built *outside* the closure. Building
        // it inside would give each worker its own store.
        App::new()
            .app_data(store.clone())
            .app_data(submit.clone())
            .app_data(log.clone())
            .app_data(stamp.clone())
            .service(
                // A `resource` rather than `App::route`, which is otherwise the
                // same thing spelled shorter: `route` hoists the method guard onto
                // the resource, so a request with the wrong method fails to match
                // the *path* and is answered 404. Keeping the guard on the route
                // means the path matches and the method does not, which is what
                // 405 with an `Allow` header says — the answer that tells a
                // misconfigured probe what to change.
                web::resource("/health_check").route(web::get().to(health_check)),
            )
            // The readings routes. Three resources cover every queryable field
            // of every device, because `{field}` is a path parameter carried
            // through to the store rather than a symbol compiled in — see
            // `routes::readings` for why this is not a generated route per
            // field.
            //
            // Registered longest-path-first. actix matches in registration
            // order, and `/fields/{field}` would otherwise be tried against
            // `/fields/FIRMWARE/history` first; it does not match (a path
            // parameter stops at the `/`), so the order is not load-bearing
            // today — but it is the order that stays correct if a segment ever
            // becomes a tail match.
            // Grouping the versioned API paths under a "/v1" scope
            .service(
                web::scope("v1")
                    // TODO .wrap(YourAuthMiddleware::default())
                    .service(
                        web::resource("/devices/{id}/fields/{field}/history")
                            .route(web::get().to(field_history)),
                    )
                    .service(
                        web::resource("/devices/{id}/fields/{field}")
                            .route(web::get().to(read_field)),
                    )
                    .service(
                        web::resource("/devices/{id}/fields").route(web::get().to(list_fields)),
                    )
                    // The write routes. Same longest-path-first discipline, and
                    // here it earns its keep: `/recording/start` must be
                    // registered before `/recording`, or the shorter resource
                    // is tried first. It does not match (a literal segment is
                    // not a path parameter), so the order is still not
                    // load-bearing today — but the two now differ by a suffix
                    // rather than by a whole segment, which is one edit away
                    // from mattering.
                    .service(
                        web::resource("/devices/{id}/recording/start")
                            .route(web::post().to(start_recording)),
                    )
                    .service(
                        web::resource("/devices/{id}/recording/stop")
                            .route(web::post().to(stop_recording)),
                    )
                    .service(
                        web::resource("/devices/{id}/recording/pause")
                            .route(web::post().to(pause_recording)),
                    )
                    .service(
                        web::resource("/devices/{id}/recording").route(web::get().to(read_phase)),
                    )
                    .service(
                        web::resource("/devices/{id}/metadata/{field}")
                            .route(web::put().to(set_metadata)),
                    )
                    .service(
                        web::resource("/devices/{id}/settings/{field}")
                            .route(web::put().to(set_setting)),
                    )
                    .service(
                        web::resource("/devices/{id}/commands").route(web::get().to(list_commands)),
                    )
                    .service(web::resource("/commands/{id}").route(web::get().to(read_command))),
            )
            // Registered last, and this time the order *is* load-bearing: the
            // UI is mounted on a tail match (`/swagger-ui/{_:.*}`), which is
            // precisely the kind of resource the note above says the ordering
            // exists to protect against. It is also unversioned and outside the
            // scope, because a document that described only `/v1` from a `/v1`
            // path could not describe the route that lists the versions.
            .service(SwaggerUi::new(SWAGGER_UI_PATH).url(OPENAPI_JSON_PATH, api_doc.clone()))
            // `/swagger-ui` without the trailing slash, sent to `/swagger-ui/`.
            //
            // The pattern above is `/swagger-ui/{_:.*}`, whose literal prefix
            // *includes* the slash, so the slashless form matches no resource
            // and falls through to a bare 404. That is the URL a person types —
            // this is the one route in the application reached by someone typing
            // rather than by a program following a contract — so the trailing
            // slash cannot be a thing they are expected to know.
            //
            // A redirect rather than `NormalizePath` middleware: that would fold
            // the slash on *every* path, quietly making `/health_check/` a
            // working alias for `/health_check` and turning the deliberate 404s
            // and 405s above into a set of near-misses that all resolve. The
            // problem is one path's, and so is the fix.
            .service(web::redirect("/swagger-ui", "/swagger-ui/"))
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
