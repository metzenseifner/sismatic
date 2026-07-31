//! The server runtime. Given an already-resolved [`ServerConfig`] and an
//! already-loaded device set, it starts both sides of the system on the caller's
//! Tokio runtime and runs until `shutdown` completes.
//!
//! - the **write side**, [`sismatic_sync::spawn`], which polls devices through
//!   `sismatic-core` and persists what it reads through a [`DynWriteStore`];
//! - the **read side**, [`sismatic_http_api::run`], which answers HTTP requests
//!   about what was persisted, through a [`DynReadStore`].
//!
//! Alongside the write side it runs [`SisKeepalive`], core's keep-warm
//! supervisor, which opens and holds a connection to every device the devices
//! file marks `eager`. Without it those settings resolve into [`Resolved`] and
//! are then read by nobody, and the first poll of every field races to open the
//! same connection.
pub mod configuration;
pub mod telemetry;

use std::net::TcpListener;
use std::sync::Arc;

use sismatic_core::devices::config::Resolved;
use sismatic_core::devices::registry::Registry;
use sismatic_core::devices::sis_keepalive::SisKeepalive;
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_http_api::ServerHandle;
use sismatic_store::{DynReadStore, DynWriteStore};
use sismatic_store_memory::MemoryStore;
use tokio::task::JoinHandle;
use tracing::{info, instrument};

use crate::configuration::ServerConfig;

/// Start the "sync" write-side and the read-side "http-api", and run until
/// `shutdown` resolves — or until the server stops on its own — then stop the
/// poll loops.
pub async fn run(
    cfg: ServerConfig,
    devices: Resolved,
    shutdown: impl Future<Output = ()>,
) -> Result<(), std::io::Error> {
    let store = MemoryStore::default();
    let read: DynReadStore = Arc::new(store.clone());
    let write: DynWriteStore = Arc::new(store);

    let registry = Arc::new(Registry::build(
        devices.devices,
        devices.groups,
        Arc::new(RusshConnector),
    ));

    // Bound and built before anything is *started*, so the one failure that is
    // likely here — the port is taken — is reported by a process that has
    // opened no SSH connection and started no poll loop, and has therefore
    // nothing to unwind.
    let listener = TcpListener::bind((cfg.http.host.as_str(), cfg.http.port))?;
    let server = sismatic_http_api::run(listener, read)?;
    let handle = server.handle();

    // Started *before* the poll loops so that for an eager device the first
    // connection is opened by the one task whose job that is, rather than raced
    // for by every field loop at once. It is not a barrier — sync does not wait
    // on it — because it must not be one: a device that is down at startup would
    // then never be polled at all. The two compose through the cold gate instead
    // (see `Device::probe`): this supervisor is what re-dials a down device, and
    // the gate is what stops the poll loops from doing so in between.
    //
    // The guard must outlive the loops it warms connections for — dropping it
    // aborts every task — so it is bound here and dropped explicitly at shutdown.
    let keepalive = SisKeepalive::spawn(&tokio::runtime::Handle::current(), registry.devices());

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

    let mut serving = tokio::spawn(server);
    let served = tokio::select! {
        joined = &mut serving => {
            // Nothing to stop on the read side — it is already down. Said out
            // loud because the two ways out of this select are worth telling
            // apart in a log: this one is a failure, the other is an operator.
            info!("the http server stopped on its own");
            joined.expect("the http server task")
        }
        () = shutdown => stop_http(handle, serving).await,
    };

    // Aborted before the drain, not after: keeping connections warm is pointless
    // once we are on the way out, and a keepalive probe starting now would only
    // add an SSH exchange for the drain below to wait behind.
    drop(keepalive);

    // Instrumented by the driver itself (`sync_shutdown`), which is where the
    // number of loops being drained is known.
    sync.shutdown().await;
    served
}

/// Stop the read side and wait for it to finish serving.
///
/// A named function, rather than the body of the `select!` arm it was lifted
/// out of, so that [`macro@instrument`] has an item to attach to: the macro
/// wraps a function's future in a span, and an inline block is not a function.
/// The span is what makes the drain *measurable* — it opens when the signal
/// lands and closes when the last in-flight request has been answered, so the
/// formatter's `elapsed_milliseconds` on close is the drain time, which is the
/// number an operator tuning a stop timeout actually needs. No amount of
/// `info!` at either end gives that without the reader subtracting timestamps.
///
/// `skip_all` because neither argument is a value worth recording: [`JoinHandle`]
/// would render as an opaque task id, and [`ServerHandle`] does not implement
/// `Debug` at all — the macro records every argument unless told otherwise, so
/// omitting this would not compile.
#[instrument(name = "http_shutdown", skip_all)]
async fn stop_http(
    handle: ServerHandle,
    serving: JoinHandle<Result<(), std::io::Error>>,
) -> Result<(), std::io::Error> {
    info!("shutdown signal received; draining in-flight requests");
    // `true` drains: in-flight requests get to finish rather than losing a
    // response to a connection closed mid-write.
    handle.stop(true).await;
    let served = serving.await.expect("the http server task");
    info!("the http server stopped");
    served
}
