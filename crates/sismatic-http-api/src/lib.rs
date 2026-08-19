//! The read side: an HTTP application over the store's [`ReadStore`] port.
//!
//! This crate is one of the two halves the composition root joins. `sismatic-sync`
//! drives the *write* side — it polls devices through `sismatic-core` and persists
//! what it reads through [`WriteStore`] — and this crate answers questions about
//! what was persisted, through [`ReadStore`]. The two never meet: they share a
//! store, not a call graph.
//!
//! # Why this crate cannot see `sismatic-core`
//!
//! The one internal dependency is `sismatic-store`, and the absence of
//! `sismatic-core` beside it is the point rather than an accident of what has
//! been needed so far. A read is answered from what the write side persisted,
//! so there is nothing here for a device, a transport or an SSH session to do —
//! and the moment this crate could name one, every consumer of the API would
//! acquire a compile path to `russh` and to the device model. The seam that
//! keeps the two subgraphs apart is the store port: this side depends on the
//! `ReadStore` trait, the write side on `WriteStore`, and only the composition
//! root knows they are one object.
//!
//! That also fixes what "the API is up" can possibly mean here. This crate can
//! observe the store; it cannot observe a device. A route that wanted to report
//! on a device's reachability would be reporting on the *freshness of a reading*,
//! which is a question about stored data — see [`health_check()`] for where that
//! line is drawn.
//!
//! # The routes
//!
//! ```text
//! GET  /health_check                               liveness; consults nothing
//!
//! GET  /v1/devices                                 every configured device
//! GET  /v1/devices/{id}                            one device + its readings
//! GET  /v1/groups                                  every configured group
//! GET  /v1/groups/{id}                             one group and its members
//!
//! GET  /v1/devices/{id}/fields                     every field's latest value
//! GET  /v1/devices/{id}/fields/{field}             one field's latest value
//! GET  /v1/devices/{id}/fields/{field}/history     one field over a time span
//!
//! POST /v1/devices/{id}/recording/start            begin a recording
//! POST /v1/devices/{id}/recording/stop             end one
//! POST /v1/devices/{id}/recording/pause            suspend one
//! PUT  /v1/devices/{id}/metadata/{field}           write a metadata register
//! PUT  /v1/devices/{id}/settings/{field}           write a device setting
//! GET  /v1/devices/{id}/recording                  the write side's phase/epoch
//! GET  /v1/devices/{id}/commands                   what this device was asked
//! GET  /v1/commands/{id}                           what became of one request
//!
//! GET  /swagger-ui/                                the routes above, browsable
//! GET  /api-docs/openapi.json                      the same, as OpenAPI 3.1
//! ```
//!
//! Three routes cover every field core can query, and will still cover them
//! after core's catalog grows, because `{field}` is a path parameter passed
//! through to the store rather than a symbol this crate was compiled against.
//! A field added to core's catalog is expanded by the `'*'` sync schedule,
//! polled, and stored — and then served here with no code change in any crate.
//! [`routes::readings`] has the argument for why that is a better property than
//! a route generated per field would be.
//!
//! The last two are the same routes described to a reader: an OpenAPI document
//! derived from the handlers and the DTOs themselves, and Swagger UI served over
//! it so the API can be browsed and exercised from a browser with nothing
//! installed. Both are compiled in — no CDN, no static-asset step — for the
//! reasons in [`openapi`].
//!
//! # Capability, not connection
//!
//! [`run`] takes a [`DynReadStore`] — `Arc<dyn ReadStore>` — and never a
//! [`WriteStore`], so no handler can write a reading no matter what it asks
//! for. The store the composition root passes in is the very same object the
//! sync driver writes through; what differs is the type each side sees it as.
//! Narrowing at the boundary rather than reviewing call sites is what makes
//! "the read side never writes" a property of the build instead of a
//! convention.
//!
//! The write routes extend that arrangement rather than relaxing it. They are
//! handed `CommandSubmit` and `CommandLog` — append an intent, read what was
//! appended — and never `CommandDrain`, which is what claims a command and
//! settles it. Draining belongs to `sismatic-intent-relay`, and a handler able
//! to claim could reorder a device's queue. So the capability this crate gained
//! is exactly one verb, and it is one that *records a request* rather than
//! performing it: no handler here can reach a device, which is what keeps the
//! `sismatic-core` seam intact while the API gained a write side.
//!
//! The [`DeviceCatalog`] is the same trick applied to a third question. The
//! inventory routes need to know which devices exist, and the obvious source —
//! `sismatic-core`'s `Registry` — is exactly the type this crate may not name.
//! So the composition root projects it into DTOs once at startup and passes the
//! projection. This crate learns *what* is configured and nothing about how to
//! reach it: `DeviceSummary` has no credential field at all, so there is no
//! secret here to leak rather than a secret that is carefully not printed.
//!
//! [`DeviceCatalog`]: sismatic_store::DeviceCatalog
//!
//! The store is registered with [`web::Data::from`], which adopts the existing
//! `Arc` rather than wrapping it in a second one, so a handler's
//! `web::Data<dyn ReadStore>` is a clone of the composition root's handle and not
//! an `Arc<Arc<_>>`.
//!
//! [`ReadStore`]: sismatic_store::ReadStore
//! [`WriteStore`]: sismatic_store::WriteStore
//! [`DynReadStore`]: sismatic_store::DynReadStore
//! [`web::Data::from`]: actix_web::web::Data

pub mod openapi;
pub mod routes;
pub mod stamp;
pub mod startup;

pub use openapi::ApiDoc;
pub use routes::health_check;
pub use stamp::Stamp;
pub use startup::run;

/// The running server's stop handle, re-exported so the composition root can
/// *name* the thing [`run`]'s `Server` hands it — a shutdown step it wants to
/// put in its own instrumented function needs the type in a signature, and
/// spelling it would otherwise mean taking a direct `actix-web` dependency for
/// one path.
pub use actix_web::dev::ServerHandle;
