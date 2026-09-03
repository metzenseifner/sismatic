//! The server runtime. Given an already-resolved [`ServerConfig`] and an
//! already-loaded device set, it starts both sides of the system on the caller's
//! Tokio runtime and runs until `shutdown` completes.
//!
//! - the **poll side**, [`sismatic_sync::spawn`], which polls devices through
//!   `sismatic-core` and persists what it reads through a [`DynWriteStore`];
//! - the **HTTP side**, [`sismatic_http_api::run`], which answers questions
//!   about what was persisted through a [`DynReadStore`], and records requests
//!   to change a device through a `WritingSubmit`;
//! - the **relay side**, [`sismatic_intent_relay::spawn`], which drains those
//!   recorded requests and applies them to devices.
//!
//! The three meet at two ports and nowhere else. That is what lets the HTTP
//! side accept a write without being able to name a `Device`: it appends an
//! intent, and the relay — the only one of the three that may name one —
//! performs it. This function is the only place that knows the store behind
//! `DynReadStore` and `DynWriteStore` is one object, and the outbox behind the
//! three writing ports is another.
//!
//! Alongside the write side it runs [`SisKeepalive`], core's keep-warm
//! supervisor, which opens and holds a connection to every device the devices
//! file marks `eager`. Without it those settings resolve into [`Resolved`] and
//! are then read by nobody, and the first poll of every field races to open the
//! same connection.
pub mod configuration;
pub mod status;
pub mod telemetry;

use std::net::TcpListener;
use std::sync::Arc;

use chrono::{SecondsFormat, Utc};
use sismatic_api_types::{
    Barrier as ApiBarrier, ConnectionStatus, DeviceSummary, FieldCatalog, GroupSummary,
    InstructionSummary, Timestamp, WritingsCatalog,
};
use sismatic_core::devices::config::{Barrier, DeviceConfig, GroupConfig, Resolved};
use sismatic_core::devices::registry::Registry;
use sismatic_core::devices::sis_keepalive::SisKeepalive;
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_core::protocol::instructions::commands::Command;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_core::protocol::instructions::register::Register;
use sismatic_core::protocol::instructions::setting::Setting;
use sismatic_http_api::{Ports, ServerHandle, Stamp};
use sismatic_store::group::DynGroupState;
use sismatic_store::outbox::{DynWritingDrain, DynWritingLog, DynWritingSubmit};
use sismatic_store::{DynDeviceCatalog, DynDeviceStatus};
use sismatic_store::{DynReadStore, DynWriteStore};
use sismatic_store_memory::{MemoryCatalog, MemoryOutbox, MemoryStore};
use tokio::task::JoinHandle;
use tracing::{info, instrument};
use uuid::Uuid;

use crate::configuration::ServerConfig;
use crate::status::RegistryStatus;

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

    // One object, four capabilities. Only this function knows they are the
    // same value — the same arrangement `ReadStore`/`WriteStore` already use,
    // and the reason neither side can perform the other's half by accident: the
    // HTTP surface holds a handle that can only append and read, the relay one
    // that can only drain, and neither type admits the other's methods.
    //
    // `group_state` is the read-only view of what each device group was last
    // told. It is on the outbox rather than the store because an expectation is
    // write-side belief, recorded inside the same critical section that admits
    // the submission — so there is no write half to hand anyone, and nothing
    // outside `submit` can claim a device group was asked for something.
    let outbox = MemoryOutbox::with_max_attempts(cfg.intent_relay.max_attempts);
    let submit: DynWritingSubmit = Arc::new(outbox.clone());
    let log: DynWritingLog = Arc::new(outbox.clone());
    let group_state: DynGroupState = Arc::new(outbox.clone());
    let drain: DynWritingDrain = Arc::new(outbox);

    // Built from the resolved config *before* the registry consumes it: the
    // catalog is the public, secret-free projection of the same device set, and
    // this is the only place both shapes are in scope. `DeviceSummary` has no
    // `username` or `password` field at all, so there is nothing to redact —
    // see `sismatic_api_types::device`.
    let catalog: DynDeviceCatalog = Arc::new(MemoryCatalog::new(
        devices.devices.iter().map(summarize).collect(),
        devices.groups.iter().map(summarize_group).collect(),
    ));

    let registry = Arc::new(Registry::build(
        devices.devices,
        devices.groups,
        Arc::new(RusshConnector),
    ));

    // Over the registry rather than the config: this is the one read whose
    // answer changes without anything being written, which is the whole reason
    // it is a port of its own rather than a field the catalog could fill.
    let status: DynDeviceStatus = Arc::new(RegistryStatus::new(Arc::clone(&registry)));

    // Bound and built before anything is *started*, so the one failure that is
    // likely here — the port is taken — is reported by a process that has
    // opened no SSH connection and started no poll loop, and has therefore
    // nothing to unwind.
    let listener = TcpListener::bind((cfg.http.host.as_str(), cfg.http.port))?;
    let server = sismatic_http_api::run(
        listener,
        Ports {
            store: read,
            catalog,
            status,
            submit,
            log,
            group_state,
            // The instruction catalog projected the same way the device set was
            // a few lines up, and for the same reason: the read side may not
            // name a `Query` any more than it may name a `DeviceConfig`, so what
            // crosses the seam is a list of DTOs rather than a dependency edge.
            fields: field_catalog(),
            writings: writings_catalog(),
        },
        stamp(),
    )?;
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

    let intent_relay = sismatic_intent_relay::spawn(
        Arc::clone(&registry),
        Arc::clone(&drain),
        sismatic_intent_relay::RelayConfig {
            poll: cfg.intent_relay.poll,
        },
    );

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
            // Both reconciliation paths are wired, and they close different
            // gaps. The relay re-reads the state immediately before a metadata
            // write, which is the one intent the freeze protects. This hook
            // keeps the desired recording state honest for a device nobody is
            // writing to, at the cost of nothing: `RUNNING_STATE` is already
            // being polled, and one poll now serves the read side and the
            // write side both.
            reconciler: Some(drain),
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

    // Order matters, and it is the reverse of startup. The HTTP server is
    // already down, so no new intent can arrive; draining the relay next lets
    // every accepted writing reach its device before the process exits.
    // Draining sync first would only add poll traffic the relay then queues
    // behind.
    intent_relay.shutdown().await;

    // Instrumented by the driver itself (`sync_shutdown`), which is where the
    // number of loops being drained is known.
    sync.shutdown().await;
    served
}

/// Project a device's config onto the secret-free summary the API serves.
///
/// `status` is [`ConnectionStatus::Unknown`], and stays that way: the catalog
/// is a snapshot of *configuration* taken before anything connects, so it has
/// nothing else it could truthfully say. The live value comes from
/// [`RegistryStatus`] and is overlaid by the two device routes — which is why
/// this is a second port rather than a field this function could fill.
fn summarize(config: &DeviceConfig) -> DeviceSummary {
    DeviceSummary {
        id: config.id.clone(),
        host: config.host.clone(),
        port: config.port,
        eager: config.eager,
        status: ConnectionStatus::Unknown,
    }
}

/// Project a group's config onto the summary the API serves and the outbox
/// arms its barrier from.
///
/// The barrier policy crosses the seam here, and this is the only place the two
/// spellings of it meet: `core` parses the config into its own [`Barrier`] and
/// the outbox enforces the wire [`ApiBarrier`], because neither crate can see
/// the other. The match below is wildcard-free, so a third policy is a build
/// error at this seam rather than a silent mapping onto an existing one — the
/// same drift sentinel `sismatic_sync::dto` uses for the read direction.
///
/// [`ApiBarrier`]: sismatic_api_types::Barrier
fn summarize_group(config: &GroupConfig) -> GroupSummary {
    GroupSummary {
        id: config.id.clone(),
        members: config.device_ids.clone(),
        barrier_timeout_secs: config.barrier_timeout.as_secs(),
        barrier: match config.barrier {
            Barrier::DispatchReady => ApiBarrier::DispatchReady,
            Barrier::FailBatch => ApiBarrier::FailBatch,
        },
    }
}

/// Project one instruction enum's catalog onto the summaries the API serves.
///
/// A macro rather than a function, because the four catalogs share every method
/// this needs — `ALL`, `name`, `accepted`, `description` — and share no *trait*
/// that names them: `instruction_catalog!` generates them as inherent methods on
/// four unrelated enums. The alternatives are a trait added to `core` purely so
/// this one call site can be generic, or four near-identical functions; a macro
/// over the four is the smaller of the three, and it inherits the property that
/// matters from `ALL`, which is that nothing here can list a subset. A variant
/// added to any of the four appears in the published catalog with no edit.
///
/// The canonical name is dropped from `aliases` — [`accepted`] yields it first,
/// and a summary that repeated its own `name` in its synonym list would read as
/// though the field had one more spelling than it does.
///
/// [`accepted`]: sismatic_core::protocol::instructions::query::Query::accepted
macro_rules! summarize_catalog {
    ($Enum:ty) => {
        <$Enum>::ALL
            .iter()
            .map(|instruction| InstructionSummary {
                name: instruction.name().to_owned(),
                aliases: instruction.accepted()[1..]
                    .iter()
                    .map(|alias| (*alias).to_owned())
                    .collect(),
                description: instruction.description().to_owned(),
            })
            .collect()
    };
}

/// Every field a reading can be asked for, as `GET /v1/readings` serves it.
///
/// Catalog order rather than sorted by name, which is the opposite of what
/// [`MemoryCatalog`](sismatic_store_memory::MemoryCatalog) does to the device
/// set — and for a reason that does not apply here. The device list is sorted
/// because its input order is an accident of a file, so a rendered index would
/// otherwise diff against nothing stable. This list's order is a compile-time
/// constant and so is already stable, and it is the order the catalog was
/// *written* in: the three stream names sit together, the six RTMP live states
/// sit together, and alphabetizing would interleave them with `SNMP_STATE`.
fn field_catalog() -> FieldCatalog {
    FieldCatalog {
        fields: summarize_catalog!(Query),
    }
}

/// Everything a write can name, as `GET /v1/writings` serves it.
///
/// Three catalogs, kept apart because the API keeps them apart: a metadata
/// register is refused by the settings route and vice versa, which is what stops
/// the recording freeze from being sidestepped (see
/// `sismatic_intent_relay::translate`). Merging them here would advertise a
/// single list of names, half of which each write route refuses.
fn writings_catalog() -> WritingsCatalog {
    WritingsCatalog {
        commands: summarize_catalog!(Command),
        metadata: summarize_catalog!(Register),
        settings: summarize_catalog!(Setting),
    }
}

/// The real id-and-instant source the write handlers are given.
///
/// The one place in the process that decides what a fresh writing id looks
/// like. A v4 UUID because an id travels in a `Location` header and in a
/// client's retry logic, so it has to be unguessable enough not to be typed by
/// hand and unique without a coordinator.
///
/// Lives here rather than in `sismatic-http-api` so that crate needs neither
/// `uuid` nor a clock — see [`Stamp`]'s docs for what that buys its tests.
fn stamp() -> Stamp {
    Stamp::new(|| {
        (
            Uuid::new_v4().to_string(),
            // The same rendering the relay and the sync driver use, which is
            // what lets the outbox compare a `not_before` against a claim time
            // as plain strings.
            Timestamp(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
        )
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// One catalog's worth of the assertion below: same length, same order, same
    /// names, and every published spelling resolves back to the instruction it
    /// was projected from.
    ///
    /// The round trip is the half worth having. Length and order would pass for
    /// a projection that published `description` where `name` belongs — a body
    /// full of prose that no route accepts — whereas `from_str` is the very
    /// lookup the relay and the sync driver perform, so a name that survives it
    /// is a name the rest of the system will honour.
    macro_rules! assert_covers {
        ($Enum:ty, $published:expr) => {{
            let published: &[InstructionSummary] = &$published;
            assert_eq!(
                published.len(),
                <$Enum>::ALL.len(),
                "the published catalog is not core's",
            );

            for (instruction, summary) in <$Enum>::ALL.iter().zip(published) {
                assert_eq!(summary.name, instruction.name(), "out of catalog order");
                assert_eq!(summary.description, instruction.description());

                let aliases: Vec<&str> = summary.aliases.iter().map(String::as_str).collect();
                assert_eq!(
                    aliases,
                    &instruction.accepted()[1..],
                    "{} publishes the wrong synonyms",
                    instruction.name(),
                );

                // Every spelling this server advertises is one it will accept
                // back — a caller that pastes a name out of the catalog into a
                // URL must not be told there is no such field.
                for spelling in std::iter::once(&summary.name).chain(&summary.aliases) {
                    assert_eq!(
                        <$Enum>::from_str(spelling).map(|i| i.name()).ok(),
                        Some(instruction.name()),
                        "'{spelling}' is published but does not resolve to {}",
                        instruction.name(),
                    );
                }
            }
        }};
    }

    /// The half of the exploratory routes that only this crate can check.
    ///
    /// `sismatic-http-api` pins that whatever `run` was handed is what the two
    /// scope roots serve, and it cannot do more than that: it may not name a
    /// `Query`, which is the whole point of the seam. This is the other side —
    /// that what it is handed is core's catalog entire — and it is the assertion
    /// a shipped API depends on, since a projection that quietly dropped the
    /// RTMP fields would leave every test in both crates green while callers
    /// were told those names do not exist.
    #[test]
    fn the_published_catalogs_cover_every_instruction_core_has() {
        assert_covers!(Query, field_catalog().fields);

        let writings = writings_catalog();
        assert_covers!(Command, writings.commands);
        assert_covers!(Register, writings.metadata);
        assert_covers!(Setting, writings.settings);
    }

    /// The catalogs are worth publishing at all, which a vacuous
    /// `0 == ALL.len()` would not notice.
    #[test]
    fn the_published_catalogs_are_not_empty() {
        assert!(field_catalog().fields.len() > 20);
        let writings = writings_catalog();
        assert_eq!(writings.commands.len(), 3, "start, stop and pause");
        assert!(!writings.metadata.is_empty());
        assert!(!writings.settings.is_empty());
    }

    /// A caller reading `GET /v1/writings` has to be able to tell which list to
    /// write a name through, and the two write routes refuse each other's names
    /// — `sismatic_intent_relay::translate` turns a register submitted as a
    /// setting into a failure rather than a write with no recording freeze. Two
    /// lists that shared a name would advertise a choice that does not exist.
    #[test]
    fn no_name_is_published_as_both_a_register_and_a_setting() {
        let writings = writings_catalog();
        for register in &writings.metadata {
            assert!(
                Setting::from_str(&register.name).is_err(),
                "'{}' is published under both metadata and settings",
                register.name,
            );
        }
    }
}
