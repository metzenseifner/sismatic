//! The two impure values a write handler needs, behind one injected function.
//!
//! A submission carries a fresh [`CommandId`] and the instant it arrived.
//! Calling `Uuid::new_v4()` and `Utc::now()` inside a handler would work and
//! would cost two things worth more than the twelve lines of wiring here.
//!
//! The first is testability. A handler that mints its own id returns a
//! different body on every call, so a test asserting on what came back has to
//! parse the response to learn what to assert against — it can pin the *shape*
//! of an id but never the id itself, and never that the `Location` header names
//! the command the body does. With the effect injected, a test supplies a
//! counter and asserts on `"cmd-1"` directly.
//!
//! The second is that it keeps this crate's dependency cone honest. `uuid` and
//! `chrono` are the composition root's dependencies, not the HTTP surface's:
//! the HTTP surface's job is to accept a request and hand a `Submission` to a
//! port, and "what a fresh identifier looks like" is a decision the process
//! makes once, not one every handler re-makes.
//!
//! Curried deliberately. The root supplies the effect once, and every handler
//! thereafter is a pure function of its extractors.

use sismatic_api_types::{CommandId, Timestamp};

/// A minted `(id, instant)` pair, grouped because a submission needs both and
/// they are produced together — and because two separate injections would let a
/// test wire a deterministic clock and a real id generator, which is a state
/// nothing wants.
pub struct Stamp(Box<dyn Fn() -> (CommandId, Timestamp) + Send + Sync>);

impl Stamp {
    pub fn new(f: impl Fn() -> (CommandId, Timestamp) + Send + Sync + 'static) -> Self {
        Stamp(Box::new(f))
    }

    /// One `(id, instant)`. Called once per accepted submission.
    pub fn next(&self) -> (CommandId, Timestamp) {
        (self.0)()
    }
}

impl std::fmt::Debug for Stamp {
    /// `Stamp` holds a closure, so there is nothing to print. An impl exists
    /// anyway because `web::Data<Stamp>` is the kind of thing that ends up in a
    /// derived `Debug` somewhere, and a missing impl there is a confusing error
    /// a long way from here.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Stamp(..)")
    }
}
