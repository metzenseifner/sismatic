//! Applies queued write intents from the outbox to devices.
//!
//! The half of the write path that is allowed to name a `Device`. The HTTP
//! surface records an [`Intent`](sismatic_api_types::Intent) and answers
//! `202 Accepted`; this crate reads those records and performs them, which is
//! what keeps every front-end free of a compile path to `sismatic-core`.

pub mod relay;
mod translate;

// Re-exported at the root so the composition root spells
// `sismatic_intent_relay::spawn(..)` rather than naming the module. `relay`
// stays public because `RelayHandle`'s methods are documented there.
pub use relay::{RelayConfig, RelayHandle, spawn};
