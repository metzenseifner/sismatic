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
//! Three routes cover every field core can query, and will still cover them
//! after core's catalog grows, because `{field}` is a path parameter passed
//! through to the store rather than a symbol this crate was compiled against.
//! A field added to core's catalog is expanded by the `'*'` sync schedule,
//! polled, and stored — and then served here with no code change in any crate.
//! [`handlers::readings`] has the argument for why that is a better property than
//! a route generated per field would be.
//!
//! The `/groups` half of each scope asks those same questions of a *device
//! group* — every member's answer in one object, beside what the device group
//! was last told to be, so a member that quietly did not start is visible
//! without comparing five responses by hand.
//!
//! On the root of each of those two scopes sits the route that says which names
//! the rest of the scope accepts: `/v1/readings` lists every queryable field and
//! `/v1/commands` every command, metadata register and setting. They are the
//! other side of the `{field}`-as-a-parameter design above — that choice is what
//! lets a field reach this API with no code change, and what leaves a misspelled
//! name indistinguishable from an unpolled one. Publishing the catalog answers
//! that before the mistake rather than after it, and it is the only way for a
//! caller to learn a synonym like `STREAM_NAME_1`, which no normalization rule
//! derives. See [`handlers::instructions`], and [`startup::Ports`] for why the
//! two lists arrive as values rather than as a seventh port.
//!
//! Devices and groups share one id namespace, but the two halves of a scope are
//! not interchangeable: a group id under `/devices` is refused with the
//! `/groups` URL that answers instead, and a device id under `/groups` likewise.
//! The alternative — fanning a group id out from the device space — cannot be
//! made correct for the two status routes, which read an outbox keyed by device
//! and would report an idle device that does not exist. See
//! [`handlers::target`] for the whole argument, [`handlers::group_readings`]
//! for the group-shaped reads, and [`handlers::commands`] for the status routes
//! themselves.
//!
//! The last two are the same routes described to a reader: an OpenAPI document
//! derived from the handlers and the DTOs themselves, and the Scalar API
//! reference served over it so the API can be browsed and exercised from a
//! browser with nothing installed. Both are compiled in — no CDN, no
//! static-asset step — for the reasons in [`openapi`].
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
//! The two instruction catalogs on [`Ports`] are the same move once more, minus
//! the trait. `Query::ALL` is as unnameable here as a `Registry` is, so the root
//! projects it into DTOs and hands them over — but there is nothing to *ask* a
//! table the compiler wrote, so it crosses the seam as a value rather than as a
//! port. What is shared is the property that matters: the list arrives as data,
//! not as a dependency edge.
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

pub mod handlers;
pub mod openapi;
pub mod stamp;
pub mod startup;

pub use handlers::health_check;
pub use openapi::ApiDoc;
pub use stamp::Stamp;
pub use startup::{Ports, run};

/// The running server's stop handle, re-exported so the composition root can
/// *name* the thing [`run`]'s `Server` hands it — a shutdown step it wants to
/// put in its own instrumented function needs the type in a signature, and
/// spelling it would otherwise mean taking a direct `actix-web` dependency for
/// one path.
pub use actix_web::dev::ServerHandle;
