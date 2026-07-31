//! The handlers, one module per route.
//!
//! Handlers are plain `async fn`s that take extractors and return a response;
//! nothing here knows how the application is assembled or what it is served on.
//! That is [`crate::startup`]'s job, and the split is what lets a route be read
//! (and reasoned about) without reading the wiring.

mod health_check;

pub use health_check::health_check;
