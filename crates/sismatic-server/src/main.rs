//! The composition root: construct the shared store handle once and start both
//! sides of the system on one Tokio runtime — the `sync` write-side task set
//! ([`sismatic_sync::spawn`]) and, eventually, the read-side http-api.

use std::sync::Arc;
use std::time::Duration;

use sismatic_core::devices::registry::Registry;
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_store::{DynReadStore, DynWriteStore};
use sismatic_store_memory::MemoryStore;
use sismatic_sync::SyncConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // One store, two capability-narrowed views: the write side may only write,
    // the read side may only read.
    let store = MemoryStore::default();
    let _read: DynReadStore = Arc::new(store.clone());
    let write: DynWriteStore = Arc::new(store);

    // TODO: load device/group configs from the server config instead of the
    // empty registry below.
    let registry = Arc::new(Registry::build(
        Vec::new(),
        Vec::new(),
        Arc::new(RusshConnector),
    ));

    // TODO: source interval/fields from the server config (`cfg.sync`).
    let sync = sismatic_sync::spawn(
        registry,
        write,
        SyncConfig {
            interval: Duration::from_secs(30),
            fields: vec!["RUNNING_STATE".to_string()],
        },
    );

    // Until the http-api lands and holds the runtime, block on Ctrl-C.
    // sismatic_http_api::serve(_read, cfg.http).await?; // gets a ReadStore, never a WriteStore
    tokio::signal::ctrl_c().await?;

    sync.shutdown().await;
    Ok(())
}
