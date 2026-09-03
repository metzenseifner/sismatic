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
use sismatic_api_types::{FieldCatalog, WritingsCatalog};
use sismatic_store::catalog::{DeviceCatalog, DynDeviceCatalog};
use sismatic_store::group::{DynGroupState, GroupState};
use sismatic_store::outbox::{DynWritingLog, DynWritingSubmit, WritingLog, WritingSubmit};
use sismatic_store::status::{DeviceStatus, DynDeviceStatus};
use sismatic_store::{DynReadStore, ReadStore};

use crate::handlers::target::{INVENTORY, READINGS, WRITINGS};
use crate::handlers::{
    field_catalog, field_history, group_field_history, list_devices, list_fields,
    list_group_fields, list_group_writings, list_groups, list_writings, pause_group_recording,
    pause_recording, read_desired_recording_state, read_device, read_field, read_group,
    read_group_desired_recording_state, read_group_field, read_writing, set_group_metadata,
    set_group_setting, set_metadata, set_setting, start_group_recording, start_recording,
    stop_group_recording, stop_recording, writings_catalog,
};
use crate::health_check;
use crate::openapi::{
    Docs, OPENAPI_JSON_PATH, SCALAR_JS_PATH, SCALAR_UI_PATH, openapi_json, scalar_js, scalar_ui,
};
use crate::stamp::Stamp;

/// The collaborators the application is assembled over.
///
/// A struct rather than eight positional arguments, and the reason is not only
/// that eight of them is where a caller starts passing the catalog where the
/// status port goes. Four of these are trait objects the composition root
/// builds from *one* value — the outbox is `submit`, `log` and `group_state` at
/// once — so at the call site they are six `Arc::new(x.clone())`s of two
/// distinguishable things. Named fields make a mis-wiring a field name that
/// does not exist; positions make it a server that answers plausible nonsense.
///
/// Every *port* here is a `dyn` handle rather than a concrete adapter, which is
/// what keeps this crate unable to name a `MemoryStore` — or a `Registry` behind
/// it.
///
/// The last two fields are not ports and are deliberately not dressed as ones.
/// They carry `sismatic-core`'s instruction catalog, which is a table the
/// compiler wrote: there is no lookup to perform, no I/O to fail, and no second
/// implementation an installation could ever want. A trait over it would be
/// ceremony around a constant. What they share with the ports is the property
/// that matters — the value crosses the seam as a DTO, so this crate learns
/// *which names exist* without acquiring a compile path to the enum they came
/// from.
pub struct Ports {
    /// Reading what the sync side persisted. Never a
    /// [`WriteStore`](sismatic_store::WriteStore).
    pub store: DynReadStore,
    /// What the server was configured with — the only authority on whether an
    /// id exists.
    pub catalog: DynDeviceCatalog,
    /// Live connection state, read from the running registry without dialing.
    pub status: DynDeviceStatus,
    /// Appending an intent. The one write verb the HTTP surface has.
    pub submit: DynWritingSubmit,
    /// Reading what was appended. No `WritingDrain` beside it, deliberately.
    pub log: DynWritingLog,
    /// Reading what each device group was last told to be. Read-only by
    /// construction — see [`run`].
    pub group_state: DynGroupState,
    /// Every field a reading can be asked for, projected from core's query
    /// catalog. Served by `GET /v1/readings`.
    pub fields: FieldCatalog,
    /// Every name a write can address, projected from core's command, register
    /// and setting catalogs. Served by `GET /v1/writings`.
    pub writings: WritingsCatalog,
}

/// Build the application over its [`Ports`], serve it on `listener`, and hand
/// the running server back.
///
/// Fails only if the listener cannot be turned into a serving socket; every
/// later failure belongs to the returned [`Server`], which resolves when it
/// stops.
///
/// # What the argument list narrows
///
/// Six capabilities, each the smallest one its handlers need. `submit` may
/// append an intent and nothing else — it cannot dispatch one, settle one, or
/// touch a reading. There is deliberately no `WritingDrain` here: draining is
/// `sismatic-intent-relay`'s, and a handler that could claim a writing could
/// reorder a device's queue. And still no [`WriteStore`], so the readings
/// routes remain unable to write no matter what they ask for.
///
/// [`Ports::group_state`] is the newest and the narrowest: it can *read* what a
/// device group was told and cannot record it. Recording happens inside
/// `submit`, atomically with the admission it describes, which is why there is
/// no write half to hand over — a handler able to set an expectation directly
/// could claim a device group had been asked for something no writing was ever
/// queued for.
///
/// That is the same narrowing the read side already relied on, extended rather
/// than relaxed: the write side still has exactly one verb, and it is one that
/// records a request rather than performs it.
///
/// The two catalogs beside them narrow nothing, because there is nothing there
/// to narrow: they are constants, and a handler holding one can do exactly what
/// a reader of this file can — say which names exist.
///
/// [`WriteStore`]: sismatic_store::WriteStore
pub fn run(listener: TcpListener, ports: Ports, stamp: Stamp) -> Result<Server, std::io::Error> {
    let Ports {
        store,
        catalog,
        status,
        submit,
        log,
        group_state,
        fields,
        writings,
    } = ports;

    // Adopt the caller's `Arc` instead of wrapping it: `Data::new` would give
    // handlers an `Arc<Arc<dyn ReadStore>>` to dereference twice, and would make
    // the type say the API owns a store rather than shares one.
    let store: web::Data<dyn ReadStore> = web::Data::from(store);
    let catalog: web::Data<dyn DeviceCatalog> = web::Data::from(catalog);
    let status: web::Data<dyn DeviceStatus> = web::Data::from(status);
    let submit: web::Data<dyn WritingSubmit> = web::Data::from(submit);
    let log: web::Data<dyn WritingLog> = web::Data::from(log);
    let group_state: web::Data<dyn GroupState> = web::Data::from(group_state);
    // `Data::new` here and not `Data::from`: the stamp arrives owned, because
    // the composition root has no reason to keep a handle to it. The two
    // instruction catalogs arrive owned for the same reason — they are values
    // the root built and handed over, not objects it shares.
    let stamp = web::Data::new(stamp);
    let fields = web::Data::new(fields);
    let writings = web::Data::new(writings);

    // Built once and cloned per worker, for the same reason the store handle is:
    // the docs are identical on every worker, and assembling them inside the
    // closure would rebuild the whole thing — document, page and a four-megabyte
    // bundle — once per thread at startup.
    let docs = web::Data::new(Docs::render());

    let server = HttpServer::new(move || {
        // Run once per worker thread, so everything captured here is cloned per
        // worker — hence the `Data` handle built *outside* the closure. Building
        // it inside would give each worker its own store.
        App::new()
            .app_data(store.clone())
            .app_data(catalog.clone())
            .app_data(status.clone())
            .app_data(submit.clone())
            .app_data(log.clone())
            .app_data(group_state.clone())
            .app_data(stamp.clone())
            .app_data(fields.clone())
            .app_data(writings.clone())
            .app_data(docs.clone())
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
            // `handlers::readings` for why this is not a generated route per
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
                web::scope("/v1")
                    // Each scope's segment comes from `handlers::target` rather
                    // than a literal here, because that module builds the "try
                    // this URL instead" half of every cross-space 404 out of the
                    // same constant. Two literals could disagree, and the way
                    // they would disagree is a refusal pointing at a second 404.
                    .service(
                        web::scope(READINGS)
                            // TODO .wrap(YourAuthMiddleware::default())
                            //
                            // The scope root: which fields the routes below
                            // accept. An empty resource path matches the scope
                            // itself and nothing under it, so unlike every
                            // other registration here its position is free —
                            // it can neither swallow a longer path nor be
                            // swallowed by one. First, because it is where a
                            // reader with no field name yet has to start.
                            .service(web::resource("").route(web::get().to(field_catalog)))
                            .service(
                                web::resource("/devices/{id}/fields/{field}/history")
                                    .route(web::get().to(field_history)),
                            )
                            .service(
                                web::resource("/devices/{id}/fields/{field}")
                                    .route(web::get().to(read_field)),
                            )
                            .service(
                                web::resource("/devices/{id}/fields")
                                    .route(web::get().to(list_fields)),
                            )
                            // The group routes, read and write, in the same
                            // longest-path-first order and ahead of the bare
                            // `/groups/{id}` for the same reason `/devices/{id}` comes
                            // after everything under it.
                            .service(
                                web::resource("/groups/{id}/fields/{field}/history")
                                    .route(web::get().to(group_field_history)),
                            )
                            .service(
                                web::resource("/groups/{id}/fields/{field}")
                                    .route(web::get().to(read_group_field)),
                            )
                            .service(
                                web::resource("/groups/{id}/fields")
                                    .route(web::get().to(list_group_fields)),
                            ),
                    )
                    .service(
                        web::scope(WRITINGS)
                            // The scope root, as under `readings` above: which
                            // names a write may address. Worth contrasting with
                            // the `/{id}` resource at the bottom of this scope,
                            // which is the registration whose order *is* load-
                            // bearing: `/{id}` matches one segment and this
                            // matches none, so a writing id can never be read as
                            // this route and this route can never shadow a
                            // writing id.
                            .service(web::resource("").route(web::get().to(writings_catalog)))
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
                                web::resource("/devices/{id}/recording")
                                    .route(web::get().to(read_desired_recording_state)),
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
                                web::resource("/devices/{id}/history")
                                    .route(web::get().to(list_writings)),
                            )
                            .service(
                                web::resource("/groups/{id}/recording/start")
                                    .route(web::post().to(start_group_recording)),
                            )
                            .service(
                                web::resource("/groups/{id}/recording/stop")
                                    .route(web::post().to(stop_group_recording)),
                            )
                            .service(
                                web::resource("/groups/{id}/recording/pause")
                                    .route(web::post().to(pause_group_recording)),
                            )
                            .service(
                                web::resource("/groups/{id}/recording")
                                    .route(web::get().to(read_group_desired_recording_state)),
                            )
                            .service(
                                web::resource("/groups/{id}/metadata/{field}")
                                    .route(web::put().to(set_group_metadata)),
                            )
                            .service(
                                web::resource("/groups/{id}/settings/{field}")
                                    .route(web::put().to(set_group_setting)),
                            )
                            .service(
                                web::resource("/groups/{id}/history")
                                    .route(web::get().to(list_group_writings)),
                            )
                            // Last in the scope, and this is the registration
                            // whose order carries weight. `/{id}` is a single
                            // path parameter on the scope root, so it would
                            // match `devices` or `groups` if it were tried
                            // first — every route above it would become
                            // `GET /v1/writings/{id}` with the id `devices`.
                            // A path parameter stops at the `/`, so a longer
                            // path cannot reach it, and one segment is exactly
                            // what a writing id is.
                            .service(web::resource("/{id}").route(web::get().to(read_writing))),
                    )
                    .service(
                        web::scope(INVENTORY)
                            // The inventory routes, registered after everything under
                            // `/devices/{id}/…` so the bare `{id}` resource cannot be
                            // tried against a longer path first. `/devices` last of the
                            // three, because it is the shortest.
                            .service(
                                web::resource("/devices/{id}").route(web::get().to(read_device)),
                            )
                            .service(web::resource("/devices").route(web::get().to(list_devices)))
                            .service(web::resource("/groups/{id}").route(web::get().to(read_group)))
                            .service(web::resource("/groups").route(web::get().to(list_groups))),
                    ),
            )
            // The docs, registered last and unversioned. Outside the scope
            // because a document that described only `/v1` from a `/v1` path
            // could not describe the route that lists the versions.
            //
            // Three exact paths, no tail match: the page loads exactly one
            // asset and it is named here, so there is no `{_:.*}` resource that
            // has to be kept out of the way of everything registered above.
            // Ordering is therefore not load-bearing for these three the way it
            // is for the readings routes — the note there stands on its own.
            .service(web::resource(OPENAPI_JSON_PATH).route(web::get().to(openapi_json)))
            .service(web::resource(SCALAR_UI_PATH).route(web::get().to(scalar_ui)))
            .service(web::resource(SCALAR_JS_PATH).route(web::get().to(scalar_js)))
            // `/scalar/` with a trailing slash, sent to `/scalar`.
            //
            // The resource above is the exact path `/scalar`, so the slashed
            // form matches nothing and falls through to a bare 404. That is a
            // URL a person types — this is the one route in the application
            // reached by someone typing rather than by a program following a
            // contract — so the trailing slash cannot be a thing they are
            // expected to get right.
            //
            // A redirect rather than `NormalizePath` middleware: that would fold
            // the slash on *every* path, quietly making `/health_check/` a
            // working alias for `/health_check` and turning the deliberate 404s
            // and 405s above into a set of near-misses that all resolve. The
            // problem is one path's, and so is the fix.
            .service(web::redirect("/api/", SCALAR_UI_PATH))
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
