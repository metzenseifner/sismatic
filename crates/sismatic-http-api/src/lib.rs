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
//! which is a question about stored data — see [`health_check`] for where that
//! line is drawn.
//!
//! # Capability, not connection
//!
//! [`run`] takes a [`DynReadStore`] — `Arc<dyn ReadStore>` — and never a
//! [`WriteStore`], so no handler can write no matter what it asks for. The store
//! the composition root passes in is the very same object the sync driver writes
//! through; what differs is the type each side sees it as. Narrowing at the
//! boundary rather than reviewing call sites is what makes "the read side never
//! writes" a property of the build instead of a convention.
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

pub mod routes;
pub mod startup;

pub use routes::health_check;
pub use startup::run;

/// The running server's stop handle, re-exported so the composition root can
/// *name* the thing [`run`]'s `Server` hands it — a shutdown step it wants to
/// put in its own instrumented function needs the type in a signature, and
/// spelling it would otherwise mean taking a direct `actix-web` dependency for
/// one path.
pub use actix_web::dev::ServerHandle;
