//! Wiring shared by the black-box suites.
//!
//! [`run`](sismatic_http_api::run) takes nine collaborators, and rarely more
//! than one of them is what a given suite is actually about. This module
//! supplies the rest so a test file states the part it cares about and nothing
//! else — and so the next collaborator is one edit here rather than one per
//! suite, which is what this already saved when the catalog arrived, and again
//! when the two instruction catalogs did.
//!
//! The outbox and the catalog are the real adapters rather than doubles, for
//! the reason `tests/reads/` already gives for using the real
//! `MemoryStore`: a double would have to restate the admission table and the
//! epoch rules, and a test of a handler over a double that drifted from the
//! adapter would pass while the server was wrong.

#![allow(dead_code)] // Each suite uses the part of this it needs.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sismatic_api_types::{
    Barrier, ConfigDocument, ConfigPatch, ConnectionStatus, DeviceSummary, FieldCatalog,
    FieldSettings, GroupSummary, HttpSettings, InstructionSummary, RelaySettings, StoreSettings,
    SyncSettings, Timestamp, WritesCatalog,
};
use sismatic_http_api::Stamp;
use sismatic_http_api::config::{ConfigRefusal, DynLiveConfig, LiveConfig};
use sismatic_store::group::DynGroupState;
use sismatic_store::outbox::{DynWriteLog, DynWriteSubmit};
use sismatic_store::status::DeviceStatus;
use sismatic_store::{DynDeviceCatalog, DynDeviceStatus, DynReadStore};
use sismatic_store_memory::{MemoryCatalog, MemoryOutbox};

/// The instant every submitted write is stamped with. Fixed, because no
/// assertion here is about time passing, and a real clock would put an
/// unpredictable value in a body a test wants to compare whole.
pub const AT: &str = "2026-08-17T00:00:00.000Z";

/// Ids that count: `cmd-1`, `cmd-2`, … in submission order.
///
/// The whole reason [`Stamp`] is injected. With a UUID a test can assert that
/// *an* id came back and that the `Location` header contains *something*; with
/// a counter it can assert the header names the write the body does, and that
/// a second submission got a second id rather than reusing the first.
pub fn counting_stamp() -> Stamp {
    let issued = AtomicUsize::new(0);
    Stamp::new(move || {
        let n = issued.fetch_add(1, Ordering::SeqCst) + 1;
        (format!("cmd-{n}"), Timestamp(AT.to_owned()))
    })
}

/// The device every suite addresses, and the group over it.
///
/// A catalog with something in it is the default because the write routes now
/// `404` an id they do not recognise: a suite handed an empty catalog would
/// find every `POST` answering `404` for a reason it was not testing.
pub const DEVICE: &str = "atrium-101";
pub const GROUP: &str = "atrium-room";

/// The catalog the suites run against unless they build their own: one device
/// and one group over it.
pub fn catalog() -> MemoryCatalog {
    device_group(&[DEVICE])
}

/// A catalog of one group over `members`, in the order given.
///
/// The order is the point: `MemoryCatalog` sorts *devices* by id and leaves a
/// group's member list alone, so a device group written `[atrium, annex]` reads
/// back in that sequence — and the group routes promise their member lists in
/// exactly that order. A helper that sorted here would make that promise
/// untestable.
pub fn device_group(members: &[&str]) -> MemoryCatalog {
    MemoryCatalog::new(
        members
            .iter()
            .map(|id| DeviceSummary {
                id: (*id).to_owned(),
                host: "10.0.0.7".to_owned(),
                port: 22023,
                eager: false,
                status: ConnectionStatus::Unknown,
            })
            .collect(),
        vec![GroupSummary {
            id: GROUP.to_owned(),
            members: members.iter().map(|id| (*id).to_owned()).collect(),
            barrier_timeout_secs: 15,
            barrier: Barrier::FailBatch,
        }],
    )
}

/// One entry of a stated instruction catalog.
pub fn instruction(name: &str, aliases: &[&str], description: &str) -> InstructionSummary {
    InstructionSummary {
        name: name.to_owned(),
        aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
        description: description.to_owned(),
    }
}

/// The field catalog the suites run against unless they state their own.
///
/// Stated rather than real, and this is the one place that is a *property*
/// rather than a compromise: the real catalog is `sismatic-core`'s `Query::ALL`,
/// which this crate may not name — the same seam that makes [`StatedStatus`] a
/// double. So these suites pin that whatever the composition root hands over is
/// what the route serves, and `sismatic-server`'s own tests pin that what it
/// hands over is core's whole catalog. Neither crate can check both halves, and
/// between them nothing is unchecked.
///
/// The entries are real names with a real alias, so a suite asserting a body
/// reads like the API it is testing.
pub fn field_catalog() -> FieldCatalog {
    FieldCatalog {
        fields: vec![
            instruction("RUNNING_STATE", &[], "Current recording state."),
            instruction("STREAM_1_NAME", &["STREAM_NAME_1"], "Name of stream 1."),
        ],
    }
}

/// The write-side catalog the suites run against unless they state their own.
pub fn writes_catalog() -> WritesCatalog {
    WritesCatalog {
        commands: vec![instruction(
            "STARTRECORDING",
            &["START"],
            "Start recording.",
        )],
        metadata: vec![instruction("TITLE", &[], "Recording title.")],
        settings: vec![instruction("TIMEZONE", &[], "Configured timezone.")],
    }
}

/// The settings the suites run against unless they state their own.
///
/// Realistic rather than minimal — the built-in defaults, roughly — because the
/// config suite compares whole bodies, and a document of zeroes would pass a
/// route that served a `Default::default()` of its own.
pub fn settings() -> ConfigDocument {
    ConfigDocument {
        sync: SyncSettings {
            interval_secs: 30,
            fields: vec![FieldSettings {
                name: "RUNNING_STATE".to_owned(),
                interval_secs: Some(5),
            }],
        },
        store: StoreSettings {
            retain: "1day".to_owned(),
            cleanup_interval: "5m".to_owned(),
            max_memory: "256MiB".to_owned(),
        },
        intent_relay: RelaySettings {
            poll_ms: 250,
            max_attempts: 3,
        },
        http: HttpSettings {
            host: "127.0.0.1".to_owned(),
            port: 8080,
        },
        devices_config_path: "/etc/sismatic/devices.toml".to_owned(),
    }
}

/// A [`LiveConfig`] that answers with what it was told to and records what it
/// was asked.
///
/// A double rather than the real adapter, and for the reason [`StatedStatus`] is
/// one rather than a weaker one: the real implementation lives in
/// `sismatic-server`, which this crate may not name — it is the composition
/// root, and the port exists precisely so these routes need not know that.
///
/// So the split is the same one the instruction catalogs use. These suites pin
/// what the *routes* are responsible for: that a body reaches the port parsed,
/// that what the port answers is what a caller receives, and that each refusal
/// becomes the status its case calls for. That folding a patch onto a config
/// produces the right config is `sismatic-server`'s own half, tested there over
/// values. Neither crate can check both, and between them nothing is unchecked.
pub struct StatedConfig {
    document: ConfigDocument,
    /// What every change refuses with, or `None` to accept them.
    refusal: Option<ConfigRefusal>,
    /// Every patch that arrived, in order — the only evidence that a body was
    /// deserialized rather than dropped.
    applied: std::sync::Mutex<Vec<ConfigPatch>>,
    reloads: AtomicUsize,
}

impl Default for StatedConfig {
    fn default() -> Self {
        Self::stating(settings())
    }
}

impl StatedConfig {
    /// One that accepts every change and reports `document`.
    pub fn stating(document: ConfigDocument) -> Self {
        Self {
            document,
            refusal: None,
            applied: std::sync::Mutex::new(Vec::new()),
            reloads: AtomicUsize::new(0),
        }
    }

    /// One that refuses every change with `refusal`.
    pub fn refusing(refusal: ConfigRefusal) -> Self {
        Self {
            refusal: Some(refusal),
            ..Self::default()
        }
    }

    /// The patches that reached the port.
    pub fn applied(&self) -> Vec<ConfigPatch> {
        self.applied.lock().expect("lock").clone()
    }

    /// How many times a reload was asked for.
    pub fn reloads(&self) -> usize {
        self.reloads.load(Ordering::SeqCst)
    }

    /// The answer to a change: the stated refusal, or the stated document.
    fn answer(&self) -> Result<ConfigDocument, ConfigRefusal> {
        match &self.refusal {
            Some(refusal) => Err(refusal.clone()),
            None => Ok(self.document.clone()),
        }
    }
}

#[async_trait::async_trait]
impl LiveConfig for StatedConfig {
    async fn current(&self) -> ConfigDocument {
        self.document.clone()
    }

    async fn apply(&self, patch: ConfigPatch) -> Result<ConfigDocument, ConfigRefusal> {
        self.applied.lock().expect("lock").push(patch);
        self.answer()
    }

    async fn reload(&self) -> Result<ConfigDocument, ConfigRefusal> {
        self.reloads.fetch_add(1, Ordering::SeqCst);
        self.answer()
    }
}

/// Serve `store` and a fresh outbox on `listener`, detached; hand the outbox
/// back so a test can inspect the write side directly.
///
/// The task is dropped rather than joined: dropping a `JoinHandle` leaves the
/// task running, so the server lives exactly as long as the test's runtime and
/// no test has to remember to stop it.
pub fn serve(listener: TcpListener, store: DynReadStore) -> MemoryOutbox {
    serve_with(listener, store, catalog())
}

/// [`serve`] over a stated catalog, for the suites that are about what the
/// catalog does or does not contain.
pub fn serve_with(
    listener: TcpListener,
    store: DynReadStore,
    catalog: MemoryCatalog,
) -> MemoryOutbox {
    serve_with_status(listener, store, catalog, StatedStatus::default())
}

/// [`serve_with`] over a stated connection status, for the suites that are
/// about what the status port reports.
pub fn serve_with_status(
    listener: TcpListener,
    store: DynReadStore,
    catalog: MemoryCatalog,
    status: StatedStatus,
) -> MemoryOutbox {
    serve_all(
        listener,
        store,
        catalog,
        status,
        field_catalog(),
        writes_catalog(),
        Arc::new(StatedConfig::default()),
    )
}

/// The one funnel every entry point above reaches: everything stated, nothing
/// defaulted.
///
/// Only `tests/instructions.rs` calls it directly — it is the suite that is
/// about the two catalogs, so it is the only one that needs to state them. The
/// rest reach it through a wrapper that fills in what they are not testing,
/// which is what keeps a new collaborator to one edit here rather than one per
/// suite.
pub fn serve_all(
    listener: TcpListener,
    store: DynReadStore,
    catalog: MemoryCatalog,
    status: StatedStatus,
    fields: FieldCatalog,
    writes: WritesCatalog,
    config: DynLiveConfig,
) -> MemoryOutbox {
    let outbox = MemoryOutbox::with_max_attempts(3);
    let catalog: DynDeviceCatalog = Arc::new(catalog);
    let status: DynDeviceStatus = Arc::new(status);
    let submit: DynWriteSubmit = Arc::new(outbox.clone());
    let log: DynWriteLog = Arc::new(outbox.clone());
    // The same object again, as the read-only view of what each device group
    // was told — the real adapter rather than a double, for the reason at the
    // top of this file: a double would have to restate when an expectation is
    // recorded, and that rule lives inside the outbox's admission critical
    // section.
    let group_state: DynGroupState = Arc::new(outbox.clone());

    let server = sismatic_http_api::run(
        listener,
        sismatic_http_api::Ports {
            store,
            catalog,
            status,
            submit,
            log,
            group_state,
            config,
            fields,
            writes,
        },
        counting_stamp(),
    )
    .expect("building the server");
    drop(tokio::spawn(server));
    outbox
}

/// Bind an ephemeral port, serve on it, and return `(base URL, outbox)`.
///
/// The listener is bound here rather than inside `run` so the port the kernel
/// chose is knowable — that is why `run` takes a [`TcpListener`] at all — and
/// so no two suites can race for a fixed one.
pub fn spawn(store: DynReadStore) -> (String, MemoryOutbox) {
    let (address, outbox, _) = spawn_with(store, catalog());
    (address, outbox)
}

/// [`spawn`] over a stated catalog. Returns the base URL, the outbox and the
/// port the kernel chose.
pub fn spawn_with(store: DynReadStore, catalog: MemoryCatalog) -> (String, MemoryOutbox, u16) {
    spawn_with_status(store, catalog, StatedStatus::default())
}

/// [`spawn_with`] over a stated connection status.
pub fn spawn_with_status(
    store: DynReadStore,
    catalog: MemoryCatalog,
    status: StatedStatus,
) -> (String, MemoryOutbox, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    let outbox = serve_with_status(listener, store, catalog, status);
    (format!("http://127.0.0.1:{port}"), outbox, port)
}

/// [`spawn`] over stated instruction catalogs, for the suite that is about what
/// the two scope roots publish.
pub fn spawn_with_instructions(
    store: DynReadStore,
    fields: FieldCatalog,
    writes: WritesCatalog,
) -> (String, MemoryOutbox) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    let outbox = serve_all(
        listener,
        store,
        catalog(),
        StatedStatus::default(),
        fields,
        writes,
        Arc::new(StatedConfig::default()),
    );
    (format!("http://127.0.0.1:{port}"), outbox)
}

/// [`spawn`] over a stated settings port, for the suite that is about the config
/// scope. Returns the base URL and the double, so a test can ask what reached
/// the port as well as what came back.
pub fn spawn_with_config(config: StatedConfig) -> (String, Arc<StatedConfig>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    let config = Arc::new(config);
    let store: DynReadStore = Arc::new(sismatic_store_memory::MemoryStore::default());
    drop(serve_all(
        listener,
        store,
        catalog(),
        StatedStatus::default(),
        field_catalog(),
        writes_catalog(),
        config.clone(),
    ));
    (format!("http://127.0.0.1:{port}"), config)
}

/// A [`DeviceStatus`] that reports whatever it was told to.
///
/// The one collaborator here that is a double rather than the real adapter, and
/// for a reason the others do not have: the real one reads a `sismatic-core`
/// `Registry`, which this crate may not name. That is the seam working — a test
/// of these routes states the status it wants observed and asserts it comes
/// back, which is the whole of what the routes are responsible for.
#[derive(Debug, Default, Clone)]
pub struct StatedStatus(pub std::collections::BTreeMap<String, ConnectionStatus>);

impl StatedStatus {
    pub fn of(pairs: &[(&str, ConnectionStatus)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(id, status)| ((*id).to_owned(), *status))
                .collect(),
        )
    }
}

#[async_trait::async_trait]
impl DeviceStatus for StatedStatus {
    async fn status(&self, id: &str) -> ConnectionStatus {
        self.0.get(id).copied().unwrap_or(ConnectionStatus::Unknown)
    }

    async fn all(&self) -> std::collections::BTreeMap<String, ConnectionStatus> {
        self.0.clone()
    }
}
