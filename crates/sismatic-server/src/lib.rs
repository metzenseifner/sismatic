//! The server runtime. Given an already-resolved [`ServerConfig`] and an
//! already-loaded device set, it starts both sides of the system on the caller's
//! Tokio runtime and runs until `shutdown` completes.
//!
//! Note what [`run`] does *not* do: it never reads a file. `configuration` turns
//! text into a `ServerConfig`, core turns `devices_config_path` into a
//! [`Resolved`], and both happen in the composition root (`main`). Everything
//! here is a function of its arguments, so a test drives the whole server with
//! literal values and no fixtures on disk.

pub mod configuration;

use std::sync::Arc;

use sismatic_core::devices::config::Resolved;
use sismatic_core::devices::registry::Registry;
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_store::{DynReadStore, DynWriteStore};
use sismatic_store_memory::MemoryStore;

use crate::configuration::ServerConfig;

/// Start the sync write-side (and, eventually, the read-side http-api) and run
/// until `shutdown` resolves, then stop the poll loops.
///
/// `devices` arrives resolved rather than as a path: loading it is core's job
/// and core already tests that loader, so re-reading it here would only couple
/// the server's tests to the filesystem.
pub async fn run(
    cfg: ServerConfig,
    devices: Resolved,
    shutdown: impl Future<Output = ()>,
) -> Result<(), std::io::Error> {
    // One store, two capability-narrowed views: the write side may only write,
    // the read side may only read.
    let store = MemoryStore::default();
    let _read: DynReadStore = Arc::new(store.clone());
    let write: DynWriteStore = Arc::new(store);

    let registry = Arc::new(Registry::build(
        devices.devices,
        devices.groups,
        Arc::new(RusshConnector),
    ));

    // A rename, not a decision: `configuration` already folded the per-field
    // overrides into the inherited default and decoded `interval_secs: 0` into
    // the `None` that means never, so there is nothing left to resolve here.
    let sync = sismatic_sync::spawn(
        registry,
        write,
        sismatic_sync::SyncConfig {
            fields: cfg
                .sync
                .fields
                .into_iter()
                .map(|field| sismatic_sync::FieldSchedule {
                    name: field.name,
                    interval: field.interval,
                })
                .collect(),
        },
    );

    // TODO: sismatic_http_api::serve(_read, cfg.http).await?; — the read side
    // gets a ReadStore, never a WriteStore.
    shutdown.await;

    sync.shutdown().await;
    Ok(())
}
