//! The write side: poll each device on a schedule and persist the results.
//!
//! [`spawn`] is the composition root's entry point. It mirrors [`tokio::spawn`]'s
//! contract — it must be called from *within* a Tokio runtime, it returns
//! immediately, and it hands back a [`SyncHandle`] the caller holds so it can run
//! the read-side http-api concurrently and shut the loops down cleanly.
//!
//! # Task shape
//!
//! One task per `(device, field)`, collected in a [`JoinSet`]. That granularity
//! is deliberate: a wedged SSH session polling one field must not stall the
//! others, and a panic in one loop surfaces at the root rather than silently
//! taking the fleet down.
//!
//! # Error discipline
//!
//! Inside a loop, a failed poll (device unreachable, timeout) or a failed write
//! is the *steady state*, not an exception — it is logged and the loop ticks
//! again. The **only** thing that ends a loop is an unknown field name, a
//! configuration error that cannot fix itself by retrying. No `Result` ever
//! escapes a task.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use sismatic_api_types::{Reading, Timestamp};
use sismatic_core::devices::device::Device;
use sismatic_core::devices::registry::Registry;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_store::DynWriteStore;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::dto;

/// What to poll and how often.
///
/// `fields` are canonical query names as [`Query`] spells them (e.g.
/// `"RUNNING_STATE"`) — the same string that lands in [`Reading::field`], which
/// is why it need not be mirrored as a typed enum here.
#[derive(Debug)]
pub struct SyncConfig {
    /// Delay between polls of a given field on a given device.
    pub interval: Duration,
    /// Which fields to poll on every device, by canonical query name.
    pub fields: Vec<String>,
}

/// Owns the running poll tasks. Call [`SyncHandle::shutdown`] (or drop it) to
/// stop them.
pub struct SyncHandle {
    tasks: JoinSet<()>,
    cancel: CancellationToken,
}

impl SyncHandle {
    /// Signal every loop to stop and wait for the in-flight polls to drain.
    ///
    /// Cancellation is cooperative: a loop finishes its current SSH exchange
    /// before exiting, rather than being aborted mid-command.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        while self.tasks.join_next().await.is_some() {}
    }
}

/// Start one poll loop per `(device, field)` and return a handle to them.
///
/// Must be called from within a Tokio runtime (it uses [`tokio::spawn`]).
pub fn spawn(registry: Arc<Registry>, write: DynWriteStore, cfg: SyncConfig) -> SyncHandle {
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();

    for device in registry.devices() {
        for field in &cfg.fields {
            tasks.spawn(poll_loop(
                device.clone(),
                field.clone(),
                write.clone(),
                cfg.interval,
                cancel.clone(),
            ));
        }
    }

    info!(tasks = tasks.len(), "sync driver started");
    SyncHandle { tasks, cancel }
}

/// Poll one field on one device forever, persisting each reading, until
/// cancelled.
async fn poll_loop(
    device: Arc<Device>,
    field: String,
    write: DynWriteStore,
    interval: Duration,
    cancel: CancellationToken,
) {
    // A bad field name cannot fix itself by retrying — log and never start.
    let query = match Query::from_str(&field) {
        Ok(query) => query,
        Err(_) => {
            warn!(
                device = device.id(),
                field, "unknown query field; poll loop not started"
            );
            return;
        }
    };
    let instruction = query.instruction();

    let mut ticker = tokio::time::interval(interval);
    // Re-pace from completion so a slow device does not trigger a burst of
    // catch-up ticks the moment it recovers.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                match device.run(&instruction).await {
                    Ok(value) => {
                        let reading = Reading {
                            device: device.id().to_string(),
                            field: field.clone(),
                            value: dto::to_dto(value),
                            at: Timestamp(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
                        };
                        if let Err(err) = write.upsert_latest(reading).await {
                            warn!(device = device.id(), field, %err, "failed to persist reading");
                        }
                    }
                    // A device being down is normal — log and tick again.
                    Err(err) => {
                        warn!(device = device.id(), field, %err, "poll failed; will retry next tick");
                    }
                }
            }
        }
    }

    info!(device = device.id(), field, "poll loop stopped");
}
