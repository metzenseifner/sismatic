//! `sismatic-sync` — the populate / keep-in-sync layer.
//!
//! This is the unique crate that sees both `sismatic-core` (the device model)
//! and `sismatic-api-types` (the wire contract), so it is where the two are
//! translated into each other. The poll loop and rate/backpressure policy
//! (design note §4, Deep dive A) will land here too; for now the crate carries
//! the [`dto`] conversion — the seam that keeps the copied wire types from
//! drifting away from core's decoded values.

pub mod dto;
