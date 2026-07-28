//! `sismatic-sync` — the populate / keep-in-sync layer.
//!
//! This is the unique crate that sees both `sismatic-core` (the device model) and
//! `sismatic-api-types` (the serialization contract), so it is where the two are translated into
//! each other. The poll loop and rate/backpressure policy will land here too; for now the crate
//! carries the [`dto`] conversion — the seam that keeps the copied serialization types from
//! drifting away from core's decoded values.

pub mod dto;

// The write-side driver. Behind the `driver` feature because, unlike `dto`, it
// pulls tokio + a clock and turns on `sismatic-core/ssh` — see the Cargo.toml
// note on keeping the pure conversion's dependency cone light.
#[cfg(feature = "driver")]
mod driver;
#[cfg(feature = "driver")]
pub use driver::{SyncConfig, SyncHandle, spawn};
