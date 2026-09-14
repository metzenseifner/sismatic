//! Loading and resolving device definitions from a `devices.toml`.
//!
//! The file has an optional `[defaults]` table and a list of `[[device]]`
//! tables. Each device inherits every default it does not set itself, so a
//! nearby device can be a few lines while a far one overrides only the timeouts
//! it needs:
//!
//! ```toml
//! [defaults]
//! port = 22023       # optional, defaults to 22023
//! connect_secs = 5   # optional, defaults to 5
//! exchange_secs = 3   # optional, defaults to 3
//! eager = true       # connect to every device at startup and keep it warm
//! sis_keepalive_secs = 120  # re-issue `Q` this often while warm; 0 disables the SIS keepalive
//! eager_retry_secs = 30     # while eager but cold, retry connecting this often; 0 disables retry
//! cold_backoff_secs = 30    # after a failed dial, refuse new ones this long; 0 disables the gate
//! auto_disable_after = 2    # consecutive refusals that take a field out of the schedule; 0 disables
//! self_heal_secs = 0        # retry an auto-disabled field this often; 0 never retries
//!
//! [[device]]
//! id = "atrium-101"
//! host = "10.0.0.7"
//! username = "admin"
//! password = "extron"
//!
//! [[device]]
//! id = "annex-far"
//! host = "10.9.40.12"
//! username = "admin"
//! password = "extron"
//! connect_secs = 20
//! exchange_secs = 10
//! # This unit's stream 2/3 features are unlicensed, so it answers E13 to these
//! # forever. Naming them means they are never asked for at all.
//! disabled_fields = ["STREAM_2_NAME", "STREAM_3_NAME", "STREAM_2_STATE"]
//! ```
//!
//! # Two vetoes, and why only one of them lives here
//!
//! A field can be unanswerable because an operator *said so* or because the
//! device *keeps saying so*. Only the first is configuration.
//!
//! [`DeviceConfig::disabled_fields`] is declared, immutable, and part of a
//! device's identity: editing it produces a different device with a different
//! [`uuid`](DeviceConfig::uuid), which is what lets a running registry tell a
//! device that changed from one that was merely re-read. The inferred set —
//! fields taken out of the schedule after [`auto_disable_after`] consecutive
//! refusals — is runtime *observation*, so it is held by the registry against a
//! device id and outlives any one `DeviceConfig`. Putting it here instead would
//! mean the system replaced a device, and dropped its warm SSH session, every
//! time it learned something.
//!
//! What this file controls about the second set is only its policy: how much
//! evidence disables a field ([`auto_disable_after`]) and how long a disabled one
//! waits before it is tried again ([`self_heal_secs`]).
//!
//! [`auto_disable_after`]: DeviceConfig::auto_disable_after
//! [`self_heal_secs`]: DeviceConfig::self_heal
//!
//! An optional list of `[[group]]` tables names one or more of those devices so
//! they can be addressed as one — every member receives an instruction sent to
//! the group. A group is only an `id` and the `devices` it contains; each must
//! name a `[[device]]` above, and group ids share the device id namespace:
//!
//! ```toml
//! [[group]]
//! id = "room-5"
//! devices = ["atrium-101", "annex-far"]
//! barrier_timeout_secs = 15   # default: the slowest member's connect + exchange
//! barrier = "fail"            # "fail" | "dispatch-ready"; default "fail"
//! ```
//!
//! The two barrier keys describe what happens when a command addressed to the
//! *group* cannot reach every member at once. The write side expands such a
//! command into one row per member and holds them all until each is ready to
//! go, so the device group acts in unison; `barrier_timeout_secs` bounds that
//! wait and `barrier` says whether a partially arrived device group is
//! dispatched or the whole batch fails. See [`Barrier`].
//!
//! Resolution is format-agnostic: [`resolve_config`] turns an already-parsed
//! [`RawConfig`] into a fully-resolved [`Resolved`] (devices plus groups) and is
//! the only step this crate guarantees in every build. Turning file *text* into a `RawConfig` is
//! delegated to a serde deserializer chosen by the caller; enabling the `toml`,
//! `json`, or `yaml` feature adds a ready-made loader (`from_toml_str` and
//! friends, plus an extension-dispatching `load`). `id` and `host` are the only
//! fields a device must state itself, and `username`/`password` must be resolvable
//! from the device or the defaults; `port`, `connect_secs`, and `exchange_secs` fall
//! back to built-in defaults (22023, 5, 3) when set in neither place.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
#[cfg(any(feature = "toml", feature = "yaml", feature = "json"))]
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

// Re-exported rather than left for each consumer to depend on `uuid` itself.
// It appears in a *public* field of `DeviceConfig`, so a crate that builds one
// has to name the type — and if it reaches for its own `uuid` dependency, a
// version skew between the two makes the field unassignable with an error that
// says nothing about versions. One re-export is the whole fix.
pub use uuid::Uuid;

use crate::protocol::instructions::query::Query;
use crate::protocol::instructions::register::Register;
use crate::protocol::instructions::setting::Setting;

/// The SIS keepalive interval applied when `eager` is on but `sis_keepalive_secs` is
/// left unset. Comfortably under the SMP's default 5-minute idle disconnect, with
/// room for one failed round-trip to self-heal before the window closes.
const DEFAULT_SIS_KEEPALIVE_SECS: u64 = 120;

/// The reconnect interval applied when `eager` is on but `eager_retry_secs` is left
/// unset. This governs the *cold* side of eager: how often to re-attempt the SSH
/// handshake for a device whose connection could not be established (or has since
/// dropped). Kept well above `connect_secs` so a genuinely unreachable device is
/// retried steadily without hammering it.
const DEFAULT_EAGER_RETRY_SECS: u64 = 30;

/// How long a device stays gated *cold* after a dial fails, when `cold_backoff_secs`
/// is left unset. This is what stops every caller of a down device from paying its
/// own `connect_secs` to rediscover the same fact: the first failed dial arms the
/// gate and everyone after it fails instantly until the window closes.
///
/// Matched to [`DEFAULT_EAGER_RETRY_SECS`] on purpose — both answer "how long is a
/// failed dial worth believing?" — and it bounds how late a recovered device is
/// noticed by a *lazy* device's poll loop, since for an eager one the keepalive
/// supervisor re-dials through the gate on its own cadence.
const DEFAULT_COLD_BACKOFF_SECS: u64 = 30;

/// The SMP's SIS-over-SSH port, used when neither the device nor `[defaults]` names one.
const DEFAULT_PORT: u16 = 22023;

/// Connect timeout applied when neither the device nor `[defaults]` names one.
const DEFAULT_CONNECT_SECS: u64 = 5;

/// Per-exchange timeout applied when neither the device nor `[defaults]` names one.
const DEFAULT_EXCHANGE_SECS: u64 = 3;

/// How many consecutive refusals of one field auto-disable it, when neither the
/// device nor `[defaults]` names a count.
///
/// Two rather than one, because a single refusal is not yet a pattern: a device
/// mid-reboot, or one answering a transient out-of-range, would otherwise take a
/// field out of the schedule on the strength of one reply. Two rather than five,
/// because the evidence does not improve with repetition — a refusal is a
/// *complete* exchange whose answer is "no" (see [`ControllerError::Rejected`]),
/// so asking a third time buys nothing but another wasted round trip per device
/// per interval.
///
/// [`ControllerError::Rejected`]: super::controller::ControllerError::Rejected
const DEFAULT_AUTO_DISABLE_AFTER: u32 = 2;

/// The self-heal interval applied when neither the device nor `[defaults]` names
/// one: zero, meaning an auto-disabled field is never retried.
///
/// Off by default because the common cause of a refusal is a *standing* fact —
/// an Extron license the unit does not have — and retrying a standing fact on a
/// timer is the cost this feature exists to remove. A deployment whose devices
/// gain capabilities without a restart (a license applied in the field) turns it
/// on; see [`DeviceConfig::self_heal`].
const DEFAULT_SELF_HEAL_SECS: u64 = 0;

/// The namespace every device UUID is derived under.
///
/// A fixed v4 UUID used as the seed for the v5 (SHA-1) derivation in
/// [`DeviceConfig::derive_uuid`], so the mapping from a device's configuration
/// to its identity is stable across processes, machines and releases. Generated
/// once and written down here; it is a constant, not a value anything computes.
const DEVICE_NAMESPACE: Uuid = Uuid::from_bytes([
    0x1f, 0x9a, 0x4c, 0x2e, 0x7b, 0x63, 0x4d, 0x18, 0x9e, 0x05, 0xc2, 0x7a, 0x83, 0x11, 0x6d, 0x40,
]);

impl DeviceConfig {
    /// Stamp this configuration with the identity its fields imply.
    ///
    /// [`resolve_config`] does this for every device it produces, so a config
    /// that came from a file is already stamped. This is for the other way in:
    /// a device built from an API request at runtime, or in a test, has to
    /// derive its identity through the *same* function or the registry's
    /// "same UUID means same device" comparison stops meaning anything.
    ///
    /// Takes and returns `self` rather than mutating in place, so it reads as
    /// part of the construction it belongs to and a caller cannot hold a
    /// half-built device that merely looks finished.
    #[must_use]
    pub fn derive_uuid(mut self) -> Self {
        self.uuid = fingerprint(&self);
        self
    }
}

/// A device credential held as a [`SecretString`]: redacted in `Debug` output and
/// zeroized on drop, so a password can't leak into logs or linger in memory.
///
/// Wrapping the secret in a newtype (rather than storing a bare `SecretString`)
/// lets [`DeviceConfig`] keep its derived `PartialEq`/`Eq`. `secrecy` deliberately
/// withholds equality from `SecretString` to discourage non-constant-time secret
/// comparisons; we opt back in here for the one place that needs it — asserting on
/// resolved configs in tests. `#[serde(transparent)]` forwards deserialization
/// straight to the inner string, so `password = "..."` parses unchanged and no bare
/// `String` copy of the secret is ever materialized.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Password(SecretString);

impl Password {
    /// Borrow the plaintext for the one legitimate use: handing it to SSH auth.
    /// This is the single audit point — grep `expose_secret` to find every read.
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password([REDACTED])")
    }
}

/// Compares the exposed secrets with a plain (non-constant-time) `==` — the very
/// comparison `secrecy` avoids by withholding `PartialEq`. Acceptable here because
/// configs are only compared in tests, in memory, between our own values: there is
/// no attacker-controlled input and no observable timing boundary. Reach for
/// `subtle::ConstantTimeEq` if a secret ever needs comparing on a live path.
impl PartialEq for Password {
    fn eq(&self, other: &Self) -> bool {
        self.expose_secret() == other.expose_secret()
    }
}

impl Eq for Password {}

impl From<String> for Password {
    fn from(s: String) -> Self {
        Password(SecretString::from(s))
    }
}

impl From<&str> for Password {
    fn from(s: &str) -> Self {
        Password(SecretString::from(s))
    }
}

/// A fully-resolved device: every field has a concrete value, with defaults
/// already folded in. This is what the registry consumes to open a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceConfig {
    /// This configuration's identity, derived from every other field by
    /// [`derive_uuid`](Self::derive_uuid).
    ///
    /// A device is **immutable**: there is no method that changes one, and
    /// editing any key produces a different `DeviceConfig` with a different
    /// UUID rather than a mutated one. That is what lets the registry answer
    /// "is this the same device?" by comparing two `u128`s, and it is the
    /// property the whole runtime-reconfiguration path is built on — a reload
    /// that leaves a device's keys alone leaves its UUID alone, so its warm SSH
    /// session survives. Without a *derived* identity every reload would mint
    /// fresh UUIDs and drop every connection in the fleet.
    ///
    /// Note what this is not: it is not a key anything is stored under. Reads
    /// stay filed under [`id`](Self::id), because a device that keeps its id and
    /// changes a key is the same recorder in the same room and must keep its
    /// history. This identifies a *configuration*, and the id identifies the
    /// thing configured.
    pub uuid: Uuid,
    pub id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Password,
    pub connect_timeout: Duration,
    pub exchange_timeout: Duration,
    /// Open this device's connection at startup and keep it warm, rather than
    /// waiting for the first command. The keep-warm loop is [`sis_keepalive`].
    ///
    /// [`sis_keepalive`]: DeviceConfig::sis_keepalive
    pub eager: bool,
    /// How often to re-issue the `Q` query to reset the SMP's idle-disconnect
    /// timer while eager *and warm*. `None` means never (a bare `sis_keepalive_secs = 0`),
    /// so an eager connection is warmed once and then left to self-heal. Ignored
    /// unless [`eager`] is set.
    ///
    /// [`eager`]: DeviceConfig::eager
    pub sis_keepalive: Option<Duration>,
    /// How often to re-attempt the connection while eager *and cold* — i.e. after an
    /// eager warm-up (or a later keepalive) failed to reach the device. This is what
    /// makes `eager` a standing intent to hold a warm connection rather than a
    /// one-shot connect at startup: a device that is down when the process starts, or
    /// that drops later, keeps being retried on this interval until it answers again.
    /// `None` means never (a bare `eager_retry_secs = 0`), restoring the old
    /// give-up-after-one-failure behavior. Ignored unless [`eager`] is set.
    ///
    /// [`eager`]: DeviceConfig::eager
    pub eager_retry: Option<Duration>,
    /// How long to refuse new connection attempts after one has failed. A device
    /// holds at most one connection, so without this every caller that wants it —
    /// each of a fleet poller's per-field loops, say — pays its own
    /// [`connect_timeout`] to rediscover that the device is down. The first failed
    /// dial arms the gate; callers arriving inside the window get
    /// [`DeviceError::Cold`] immediately instead, and the next dial after it closes
    /// re-tests the device for everyone.
    ///
    /// `None` means never gate (a bare `cold_backoff_secs = 0`): every call dials,
    /// which is the behavior from before this field existed.
    ///
    /// Unlike [`sis_keepalive`] and [`eager_retry`] this applies to *every* device,
    /// eager or not — it is a property of the connection, not of the keep-warm
    /// intent. [`SisKeepalive`] deliberately dials *through* the gate (see
    /// [`Device::probe`]), so an eager device's re-dial cadence stays
    /// [`eager_retry`] and the two never fight.
    ///
    /// [`connect_timeout`]: DeviceConfig::connect_timeout
    /// [`DeviceError::Cold`]: super::device::DeviceError::Cold
    /// [`Device::probe`]: super::device::Device::probe
    /// [`SisKeepalive`]: super::sis_keepalive::SisKeepalive
    /// [`sis_keepalive`]: DeviceConfig::sis_keepalive
    /// [`eager_retry`]: DeviceConfig::eager_retry
    pub cold_backoff: Option<Duration>,
    /// Fields this device is *declared* not to answer, by canonical name.
    ///
    /// The operator's half of the veto, and the cheap half: a field named here
    /// is known unsupported before the process opens a socket, so the sync
    /// driver starts no poll loop for it and the write path refuses it at
    /// submission. Nothing is ever asked, so nothing is ever refused.
    ///
    /// The case this exists for is a capability the unit does not have rather
    /// than a field nobody wants — an SMP whose stream 2 and 3 features are
    /// unlicensed answers `E13` to seven fields of a wildcard schedule, every
    /// cycle, forever. It is spelled `disabled` rather than `unsupported`
    /// because the mechanism is indifferent to the motive: a field that works
    /// and is merely too chatty to poll belongs here too, and `unsupported`
    /// would be a lie about it.
    ///
    /// Stored canonicalized — [`resolve_config`] puts every entry through the
    /// instruction catalogs — so an alias in the file (`STREAM_NAME_2`) and the
    /// name a poll loop carries (`STREAM_2_NAME`) are the same string by the
    /// time anything compares them. A `BTreeSet` because membership is the only
    /// question asked of it, and because a stable order makes the derived
    /// [`uuid`](Self::uuid) independent of how the list was written.
    ///
    /// The runtime counterpart — fields *inferred* unsupported from repeated
    /// refusals — is deliberately not here. It is mutable, this is not; see
    /// [`auto_disable_after`](Self::auto_disable_after).
    pub disabled_fields: BTreeSet<String>,
    /// How many consecutive refusals of one field take it out of the schedule by
    /// themselves.
    ///
    /// The inferred half of the veto, for the fields an operator has not
    /// written down. Only a *repeating* caller can observe "consecutive", so
    /// only the sync driver counts: a refusal of a hand-issued write is a
    /// refusal of that write and evidence of nothing, which is what keeps a
    /// one-off from disabling a field the fleet depends on.
    ///
    /// Any refusal counts, not only `E13`. The distinction the device layer
    /// already draws is between a refusal and a broken channel (see
    /// [`ControllerError::Rejected`]), and on the near side of it every code
    /// means the same thing to a scheduler: the device read the verb, decided
    /// against it, and will decide against it again.
    ///
    /// `0` disables the inference entirely, leaving
    /// [`disabled_fields`](Self::disabled_fields) as the only veto.
    ///
    /// [`ControllerError::Rejected`]: super::controller::ControllerError::Rejected
    pub auto_disable_after: u32,
    /// How long an auto-disabled field waits before it is tried again. `None`
    /// means never (a bare `self_heal_secs = 0`), which is the default.
    ///
    /// This is the cold gate one level down, and deliberately the same shape:
    /// [`cold_backoff`](Self::cold_backoff) refuses to *dial* a device until a
    /// window closes, and this refuses to *ask* for a field until one does. Both
    /// are checked lazily by the caller that would otherwise pay, so neither
    /// needs a clock or a task of its own — the poll loop is already ticking, and
    /// a tick arriving after the window has closed simply goes through.
    ///
    /// What it costs is the reason it is off by default. A healed field is one
    /// whose loop must keep ticking to do the healing, so a non-zero interval
    /// buys the retry with a timer wake-up per tick; at zero the loop can stop
    /// itself outright, because a field that will never be retried has no
    /// future work. The usual cause of a refusal — a license the unit does not
    /// have — does not change while the process runs, so paying for a retry is
    /// opting in, not opting out.
    pub self_heal: Option<Duration>,
}

/// What to do when a group command's barrier does not fill within its timeout.
///
/// A group-addressed command is expanded into one row per member, and none of
/// them is dispatched until every one has reached the head of its own device's
/// queue. That rendezvous is what makes "the device group starts together" true
/// rather
/// than approximately true. A member whose device is wedged, however, would
/// hold the barrier indefinitely, so the wait is bounded — and this is the
/// answer to what happens when the bound is reached.
///
/// There is no universally right answer, which is why it is configuration. A
/// lecture capture with a backup recorder wants [`DispatchReady`]: one
/// recording beats none. A stereo pair whose two halves are useless apart wants
/// [`FailBatch`]: a half-recorded take is worse than an obvious failure an
/// operator retries.
///
/// [`DispatchReady`]: Barrier::DispatchReady
/// [`FailBatch`]: Barrier::FailBatch
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Barrier {
    /// Run the members that arrived. One recorder is better than none.
    DispatchReady,
    /// Fail every row in the batch. Two recordings or neither.
    ///
    /// The default, because it is the answer that cannot silently produce a
    /// partial result: a failed batch is visible in the command log and an
    /// operator resubmits, where a half-dispatched one looks like success until
    /// someone plays back the missing half.
    #[default]
    FailBatch,
}

/// A resolved device group: a name plus the ids of its member devices, every
/// one of which is guaranteed to name a device in the same document. The
/// registry turns each `device_id` into the shared device handle when it builds
/// the group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupConfig {
    pub id: String,
    /// Member device ids, in the order written; each resolves to a `[[device]]`.
    pub device_ids: Vec<String>,
    /// How long a group command waits for every member to reach the head of its
    /// queue before [`barrier`] decides what to do.
    ///
    /// Defaults to the slowest member's `connect_secs + exchange_secs`, rounded
    /// up to the second — the time one member could legitimately spend on the
    /// command *ahead* of the batched one before it can be at the head. A
    /// shorter default would fire the barrier on a device that is merely busy
    /// rather than wedged, which is the false positive worth avoiding.
    ///
    /// [`barrier`]: GroupConfig::barrier
    pub barrier_timeout: Duration,
    /// What to do when the barrier times out.
    pub barrier: Barrier,
}

/// Everything a config file resolves to: the flat device list plus any groups
/// layered over it. Groups reference devices by id, so both are resolved
/// together and validated as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Resolved {
    pub devices: Vec<DeviceConfig>,
    pub groups: Vec<GroupConfig>,
}

/// Why a `devices.toml` could not be turned into [`DeviceConfig`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The file could not be read from disk (produced by the file loader).
    Io(String),
    /// The text did not deserialize into a [`RawConfig`], or a required
    /// `id`/`host` was absent.
    Parse(String),
    /// The file extension has no compiled-in deserializer.
    UnsupportedFormat(String),
    /// Two devices or groups share the same `id`, so one would shadow the
    /// other. Device and group ids live in one namespace because a caller
    /// addresses either by the same id.
    DuplicateId(String),
    /// A field was set neither on the device nor in `[defaults]`.
    MissingField { device: String, field: &'static str },
    /// A `disabled_fields` entry names nothing this build can query or write.
    ///
    /// Refused rather than ignored, because the failure it prevents is silent:
    /// a misspelled veto disables nothing, and the operator learns that from a
    /// device that keeps being asked for a field they believe they switched
    /// off — a fact visible only in the poll logs, if at all.
    UnknownDisabledField { device: String, field: String },
    /// A `[[group]]` lists no member devices, so it could address nothing.
    EmptyGroup(String),
    /// A `[[group]]` names a device id that no `[[device]]` defines.
    UnknownGroupMember { group: String, device: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "reading devices file: {e}"),
            ConfigError::Parse(e) => write!(f, "parsing devices file: {e}"),
            ConfigError::UnsupportedFormat(ext) => {
                write!(f, "unsupported config file extension `{ext}`")
            }
            ConfigError::DuplicateId(id) => write!(f, "duplicate device id `{id}`"),
            ConfigError::MissingField { device, field } => {
                write!(
                    f,
                    "device `{device}` is missing `{field}` (set it on the device or in [defaults])"
                )
            }
            ConfigError::UnknownDisabledField { device, field } => {
                write!(
                    f,
                    "device `{device}` disables `{field}`, which is not a field this server \
                     can query or write (see `GET /v1/reads` and `GET /v1/writes` for the \
                     accepted names)"
                )
            }
            ConfigError::EmptyGroup(id) => write!(f, "group `{id}` has no member devices"),
            ConfigError::UnknownGroupMember { group, device } => {
                write!(
                    f,
                    "group `{group}` names unknown device `{device}` (no matching [[device]])"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Read `path`, pick a deserializer from its extension, and resolve every device
/// defined in it. Only the file read is impure; everything after is a pure
/// function of the bytes. Available whenever at least one format feature
/// (`toml`, `json`, `yaml`) is enabled.
#[cfg(any(feature = "toml", feature = "yaml", feature = "json"))]
pub fn load(path: impl AsRef<Path>) -> Result<Resolved, ConfigError> {
    resolve_config(load_raw(path)?)
}

/// [`load`], stopping one step short: the document as written, with no default
/// folded in and nothing validated beyond its syntax.
///
/// For the caller that has to *amend* the document rather than only run it. A
/// device added at runtime inherits `[defaults]` exactly as one in the file
/// does, and the only way to guarantee that is to put the amended [`RawConfig`]
/// back through [`resolve_config`] — so the raw form has to survive startup.
#[cfg(any(feature = "toml", feature = "yaml", feature = "json"))]
pub fn load_raw(path: impl AsRef<Path>) -> Result<RawConfig, ConfigError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
    match path.extension().and_then(|e| e.to_str()) {
        #[cfg(feature = "toml")]
        Some("toml") => toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string())),
        #[cfg(feature = "yaml")]
        Some("yaml") | Some("yml") => {
            serde_saphyr::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))
        }
        #[cfg(feature = "json")]
        Some("json") => serde_json::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string())),
        other => Err(ConfigError::UnsupportedFormat(
            other.unwrap_or("").to_string(),
        )),
    }
}

/// Deserialize TOML text into a [`RawConfig`], then [`resolve_config`].
#[cfg(feature = "toml")]
pub fn from_toml_str(text: &str) -> Result<Resolved, ConfigError> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    resolve_config(raw)
}

/// Deserialize JSON text into a [`RawConfig`], then [`resolve_config`].
#[cfg(feature = "json")]
pub fn from_json_str(text: &str) -> Result<Resolved, ConfigError> {
    let raw: RawConfig =
        serde_json::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    resolve_config(raw)
}

/// Deserialize YAML text into a [`RawConfig`], then [`resolve_config`].
#[cfg(feature = "yaml")]
pub fn from_yaml_str(text: &str) -> Result<Resolved, ConfigError> {
    let raw: RawConfig =
        serde_saphyr::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    resolve_config(raw)
}

/// Format-agnostic entry point: consume an already-parsed [`RawConfig`] and
/// resolve it into a validated [`Resolved`] (devices plus any groups over
/// them). Pure and total — the same input always yields the same output, it
/// never panics, and it reads nothing from the environment. Callers who bring
/// their own deserializer target this directly.
///
/// Devices are resolved first; then each group is checked to be non-empty, to
/// have an id that collides with no device or earlier group, and to name only
/// devices that exist — so a [`Resolved`] the registry receives is guaranteed
/// consistent.
pub fn resolve_config(raw: RawConfig) -> Result<Resolved, ConfigError> {
    // One id namespace for devices and groups: a caller addresses either by id,
    // so a group may not reuse a device's (or another group's) id.
    let mut seen = HashSet::new();
    let mut devices = Vec::with_capacity(raw.devices.len());
    for device in raw.devices {
        if !seen.insert(device.id.clone()) {
            return Err(ConfigError::DuplicateId(device.id));
        }
        devices.push(resolve(&raw.defaults, device)?);
    }

    let device_ids: HashSet<&str> = devices.iter().map(|d| d.id.as_str()).collect();
    let mut groups = Vec::with_capacity(raw.groups.len());
    for group in raw.groups {
        if !seen.insert(group.id.clone()) {
            return Err(ConfigError::DuplicateId(group.id));
        }
        if group.devices.is_empty() {
            return Err(ConfigError::EmptyGroup(group.id));
        }
        for member in &group.devices {
            if !device_ids.contains(member.as_str()) {
                return Err(ConfigError::UnknownGroupMember {
                    group: group.id.clone(),
                    device: member.clone(),
                });
            }
        }
        let barrier_timeout = match group.barrier_timeout_secs {
            Some(secs) => Duration::from_secs(secs),
            None => default_barrier_timeout(&devices, &group.devices),
        };
        groups.push(GroupConfig {
            id: group.id,
            device_ids: group.devices,
            barrier_timeout,
            barrier: group.barrier.map(Barrier::from).unwrap_or_default(),
        });
    }

    Ok(Resolved { devices, groups })
}

/// The barrier timeout a group gets when it names none: the slowest member's
/// `connect_secs + exchange_secs`, rounded up to the second.
///
/// The *slowest*, not the average, because the barrier is filled by the last
/// member to arrive — a timeout derived from a faster member would fire on the
/// slow one as a matter of course. Rounded up so a sub-second remainder cannot
/// make the bound tighter than the exchange it is meant to allow for.
///
/// Members are looked up rather than passed in already-resolved, because this
/// runs inside the same loop that is still validating them; every id here has
/// already been checked to name a device, so a miss is impossible and is
/// skipped rather than defended against.
fn default_barrier_timeout(devices: &[DeviceConfig], members: &[String]) -> Duration {
    let slowest = members
        .iter()
        .filter_map(|id| devices.iter().find(|d| &d.id == id))
        .map(|d| d.connect_timeout + d.exchange_timeout)
        .max()
        .unwrap_or_default();

    // `Duration::as_secs` truncates, so a 3.5s budget would round to 3.
    let rounded = slowest.as_secs() + u64::from(slowest.subsec_nanos() > 0);
    Duration::from_secs(rounded.max(1))
}

/// Fold the defaults into one raw device, failing if a required field is unset.
fn resolve(defaults: &Defaults, device: RawDevice) -> Result<DeviceConfig, ConfigError> {
    let id = device.id;

    let port = device.port.or(defaults.port).unwrap_or(DEFAULT_PORT);
    let username = require(
        &id,
        "username",
        device.username.or_else(|| defaults.username.clone()),
    )?;
    let password = require(
        &id,
        "password",
        device.password.or_else(|| defaults.password.clone()),
    )?;
    let connect_secs = device
        .connect_secs
        .or(defaults.connect_secs)
        .unwrap_or(DEFAULT_CONNECT_SECS);
    let exchange_secs = device
        .exchange_secs
        .or(defaults.exchange_secs)
        .unwrap_or(DEFAULT_EXCHANGE_SECS);

    // `eager`, `sis_keepalive_secs`, and `eager_retry_secs` are optional everywhere: a
    // device that sets none behaves exactly as before (lazy connect, no keep-warm loop).
    let eager = device.eager.or(defaults.eager).unwrap_or(false);
    let sis_keepalive_secs = device
        .sis_keepalive_secs
        .or(defaults.sis_keepalive_secs)
        .unwrap_or(DEFAULT_SIS_KEEPALIVE_SECS);
    let sis_keepalive = (sis_keepalive_secs > 0).then(|| Duration::from_secs(sis_keepalive_secs));
    let eager_retry_secs = device
        .eager_retry_secs
        .or(defaults.eager_retry_secs)
        .unwrap_or(DEFAULT_EAGER_RETRY_SECS);
    let eager_retry = (eager_retry_secs > 0).then(|| Duration::from_secs(eager_retry_secs));

    // Unlike the three above, this one is not scoped to `eager`: the cold gate
    // guards the connection itself, so a lazy device wants it just as much.
    let cold_backoff_secs = device
        .cold_backoff_secs
        .or(defaults.cold_backoff_secs)
        .unwrap_or(DEFAULT_COLD_BACKOFF_SECS);
    let cold_backoff = (cold_backoff_secs > 0).then(|| Duration::from_secs(cold_backoff_secs));

    // A device's list *replaces* the fleet's rather than adding to it, which is
    // the one place the veto's inheritance differs from every scalar key above.
    // Merging would read better right up to the case that matters: a fleet-wide
    // `disabled_fields` covering the stream fields, and the one unit that does
    // hold the license. Under merge that unit could never be excepted, because
    // nothing in a *veto* can un-veto — so `disabled_fields = []` has to be the
    // way to say "this one answers everything", and that only means anything if
    // the device's list stands alone.
    let disabled_fields = canonicalize_disabled(
        &id,
        device
            .disabled_fields
            .as_ref()
            .or(defaults.disabled_fields.as_ref()),
    )?;

    let auto_disable_after = device
        .auto_disable_after
        .or(defaults.auto_disable_after)
        .unwrap_or(DEFAULT_AUTO_DISABLE_AFTER);

    let self_heal_secs = device
        .self_heal_secs
        .or(defaults.self_heal_secs)
        .unwrap_or(DEFAULT_SELF_HEAL_SECS);
    let self_heal = (self_heal_secs > 0).then(|| Duration::from_secs(self_heal_secs));

    let mut resolved = DeviceConfig {
        // Overwritten a line below. A placeholder rather than a parameter
        // because `fingerprint` reads the whole resolved device, and threading
        // eleven values into it separately is eleven chances for one to be left
        // out — which would not fail to compile, and would silently make two
        // different devices share an identity.
        uuid: Uuid::nil(),
        host: device.host,
        port,
        username,
        password,
        connect_timeout: Duration::from_secs(connect_secs),
        exchange_timeout: Duration::from_secs(exchange_secs),
        eager,
        sis_keepalive,
        eager_retry,
        cold_backoff,
        disabled_fields,
        auto_disable_after,
        self_heal,
        id,
    };
    resolved.uuid = fingerprint(&resolved);
    Ok(resolved)
}

/// Put every `disabled_fields` entry through the instruction catalogs, failing
/// on one that names nothing.
///
/// Canonicalizing here rather than at the point of comparison is what makes the
/// veto work on an alias. A poll loop carries the canonical name (`Query::name`
/// is what [`Instruction`] is built with), so a file that says `STREAM_NAME_2`
/// would otherwise silently veto nothing — the two strings never meet.
///
/// Three catalogs, because a field is anything addressable as one: [`Query`] is
/// what a read can ask for, [`Setting`] and [`Register`] are what a write can
/// name. [`Command`] is deliberately absent — `STARTRECORDING` is a verb, not a
/// field, and a deployment that wants to stop a recorder recording removes it
/// from the fleet rather than vetoing its verbs.
///
/// A name in two catalogs resolves to one entry and vetoes both directions,
/// which is the behavior an unlicensed stream wants: `STREAM_2_NAME` is a
/// `Query` *and* a `Setting`, and a unit that cannot answer the read cannot
/// service the write either.
///
/// [`Instruction`]: crate::protocol::instructions::Instruction
/// [`Command`]: crate::protocol::instructions::commands::Command
fn canonicalize_disabled(
    device: &str,
    fields: Option<&Vec<String>>,
) -> Result<BTreeSet<String>, ConfigError> {
    let Some(fields) = fields else {
        return Ok(BTreeSet::new());
    };

    fields
        .iter()
        .map(|written| {
            canonical_field(written).ok_or_else(|| ConfigError::UnknownDisabledField {
                device: device.to_string(),
                field: written.clone(),
            })
        })
        .collect()
}

/// The canonical spelling of `written`, from whichever catalog claims it.
///
/// Ordered by how a field is most often named rather than arbitrarily: a read
/// catalog entry is the common case, since a veto is usually written after a
/// wildcard poll started refusing.
fn canonical_field(written: &str) -> Option<String> {
    Query::from_str(written)
        .map(|q| q.name().to_owned())
        .or_else(|_| Setting::from_str(written).map(|s| s.name().to_owned()))
        .or_else(|_| Register::from_str(written).map(|r| r.name().to_owned()))
        .ok()
}

/// Derive a device's identity from its configuration.
///
/// UUIDv5 (SHA-1 over a namespace and a name) rather than v4, and that choice is
/// load-bearing rather than aesthetic. A random identity would change on every
/// reload, so the registry could not tell a device whose configuration moved
/// from one that was merely re-read — and every reload would replace every
/// device, tearing down the fleet's SSH sessions to apply a change to one of
/// them. Deriving it makes identity a pure function of configuration: equal
/// configs are one device, and a reload that changes nothing changes nothing.
///
/// Every field participates, including the password: rotating a credential
/// *should* mint a new device and redial, because the session held under the old
/// one is no longer the session the config describes. That is the one call to
/// [`ExposeSecret`] on this path, and it feeds a one-way hash — the UUID carries
/// no more of the secret than a digest does.
///
/// The encoding is explicit and length-prefixed rather than a `{:?}` of the
/// struct, because a `Debug` format is not a stable contract and
/// [`Password`]'s deliberately prints `[REDACTED]` — which would make every
/// device that differs only by credential share an identity.
fn fingerprint(config: &DeviceConfig) -> Uuid {
    /// Append a length-prefixed field, so `("ab", "c")` and `("a", "bc")` cannot
    /// hash alike.
    fn put(buf: &mut Vec<u8>, bytes: &[u8]) {
        buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        buf.extend_from_slice(bytes);
    }

    /// An optional duration, distinguishing `None` from `Some(0)` even though
    /// the resolver cannot currently produce the latter.
    fn put_opt_duration(buf: &mut Vec<u8>, value: Option<Duration>) {
        match value {
            None => put(buf, b"-"),
            Some(d) => put(buf, &d.as_nanos().to_le_bytes()),
        }
    }

    let mut buf = Vec::new();
    put(&mut buf, config.id.as_bytes());
    put(&mut buf, config.host.as_bytes());
    put(&mut buf, &config.port.to_le_bytes());
    put(&mut buf, config.username.as_bytes());
    put(&mut buf, config.password.expose_secret().as_bytes());
    put(&mut buf, &config.connect_timeout.as_nanos().to_le_bytes());
    put(&mut buf, &config.exchange_timeout.as_nanos().to_le_bytes());
    put(&mut buf, &[u8::from(config.eager)]);
    put_opt_duration(&mut buf, config.sis_keepalive);
    put_opt_duration(&mut buf, config.eager_retry);
    put_opt_duration(&mut buf, config.cold_backoff);
    // Iterated from a `BTreeSet`, so the order is the set's and not the file's:
    // two devices that disable the same fields in a different order are one
    // device, which is what makes an operator's reordering a no-op.
    put(
        &mut buf,
        &(config.disabled_fields.len() as u64).to_le_bytes(),
    );
    for field in &config.disabled_fields {
        put(&mut buf, field.as_bytes());
    }
    put(&mut buf, &config.auto_disable_after.to_le_bytes());
    put_opt_duration(&mut buf, config.self_heal);

    Uuid::new_v5(&DEVICE_NAMESPACE, &buf)
}

/// Return the value or a [`ConfigError::MissingField`] naming the device.
fn require<T>(device: &str, field: &'static str, value: Option<T>) -> Result<T, ConfigError> {
    value.ok_or_else(|| ConfigError::MissingField {
        device: device.to_string(),
        field,
    })
}

// ---- raw deserialization mirror of the file ------------------------------

/// The file as written, before any default is folded in.
///
/// Public, fields and all, because the composition root holds one for the life
/// of the process: a device added at runtime has to inherit `[defaults]` exactly
/// as a device in the file does, and the only way to guarantee that is to put
/// the amended document back through [`resolve_config`] — the one function that
/// enforces every invariant a [`Resolved`] promises. A parallel "add one device"
/// path would be a second place for duplicate ids, group membership and unknown
/// field names to be checked, and a second place for one of them to be forgotten.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default, alias = "device", alias = "devices")]
    pub devices: Vec<RawDevice>,
    #[serde(default, alias = "group", alias = "groups")]
    pub groups: Vec<RawGroup>,
}

/// Every field is optional: a default only applies where a device omits it.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<Password>,
    pub connect_secs: Option<u64>,
    pub exchange_secs: Option<u64>,
    pub eager: Option<bool>,
    pub sis_keepalive_secs: Option<u64>,
    pub eager_retry_secs: Option<u64>,
    pub cold_backoff_secs: Option<u64>,
    /// Inherited whole or not at all, unlike every scalar key above: a device
    /// that writes its own list *replaces* this one rather than adding to it.
    ///
    /// `Option` rather than a defaulted `Vec` precisely so "the device wrote an
    /// empty list" and "the device wrote nothing" stay distinguishable, which is
    /// what lets one licensed unit opt out of a fleet-wide veto with
    /// `disabled_fields = []`.
    pub disabled_fields: Option<Vec<String>>,
    pub auto_disable_after: Option<u32>,
    pub self_heal_secs: Option<u64>,
}

/// A device as written: `id` and `host` are required, the rest may inherit.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDevice {
    pub id: String,
    pub host: String,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<Password>,
    pub connect_secs: Option<u64>,
    pub exchange_secs: Option<u64>,
    pub eager: Option<bool>,
    pub sis_keepalive_secs: Option<u64>,
    pub eager_retry_secs: Option<u64>,
    pub cold_backoff_secs: Option<u64>,
    pub disabled_fields: Option<Vec<String>>,
    pub auto_disable_after: Option<u32>,
    pub self_heal_secs: Option<u64>,
}

/// A group as written: an `id`, the ids of the member devices, and how the
/// rendezvous behaves. Nothing here inherits from `[defaults]`; a group is a
/// name over existing devices plus a policy of its own, and a fleet-wide
/// default barrier would be a claim about device groups this file cannot make.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawGroup {
    pub id: String,
    pub devices: Vec<String>,
    /// Unset means "derive it from the members" — see [`GroupConfig::barrier_timeout`].
    pub barrier_timeout_secs: Option<u64>,
    pub barrier: Option<RawBarrier>,
}

/// The barrier policy as spelled in a config file.
///
/// A separate type from [`Barrier`] so the wire spellings (`"fail"`,
/// `"dispatch-ready"`) live next to the parser rather than being imposed on the
/// domain enum by a `#[serde(rename)]`. `deny_unknown_fields` has no equivalent
/// for an enum, so a misspelling is caught by there being no variant to match —
/// `barrier = "failed"` is a parse error naming the value, not a silent
/// fallback to the default.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RawBarrier {
    Fail,
    DispatchReady,
}

impl From<RawBarrier> for Barrier {
    fn from(raw: RawBarrier) -> Self {
        match raw {
            RawBarrier::Fail => Barrier::FailBatch,
            RawBarrier::DispatchReady => Barrier::DispatchReady,
        }
    }
}

#[cfg(all(test, feature = "toml"))]
mod tests {
    use super::*;

    const USER_EXAMPLE: &str = r#"
[defaults]
port = 22023
connect_secs = 5
exchange_secs = 3

[[device]]
id = "atrium-101"
host = "10.0.0.7"
username = "admin"
password = "extron"

[[device]]
id = "annex-far"
host = "10.9.40.12"
username = "admin"
password = "extron"
connect_secs = 20
exchange_secs = 10
"#;

    fn get<'a>(devices: &'a [DeviceConfig], id: &str) -> &'a DeviceConfig {
        devices.iter().find(|d| d.id == id).expect("device present")
    }

    #[test]
    fn resolves_example_with_inheritance_and_overrides() {
        let devices = from_toml_str(USER_EXAMPLE).unwrap().devices;
        assert_eq!(devices.len(), 2);

        // Order is preserved from the file.
        assert_eq!(devices[0].id, "atrium-101");
        assert_eq!(devices[1].id, "annex-far");

        // Nearby device inherits every default.
        let atrium = get(&devices, "atrium-101");
        assert_eq!(atrium.host, "10.0.0.7");
        assert_eq!(atrium.port, 22023);
        assert_eq!(atrium.connect_timeout, Duration::from_secs(5));
        assert_eq!(atrium.exchange_timeout, Duration::from_secs(3));

        // Far device overrides only the timeouts, still inherits the port.
        let annex = get(&devices, "annex-far");
        assert_eq!(annex.port, 22023);
        assert_eq!(annex.connect_timeout, Duration::from_secs(20));
        assert_eq!(annex.exchange_timeout, Duration::from_secs(10));
    }

    #[test]
    fn device_overrides_default_port() {
        let text = r#"
[defaults]
port = 22023
connect_secs = 5
exchange_secs = 3

[[device]]
id = "odd-port"
host = "10.0.0.9"
username = "admin"
password = "extron"
port = 22
"#;
        assert_eq!(from_toml_str(text).unwrap().devices[0].port, 22);
    }

    #[test]
    fn credentials_may_come_from_defaults() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
exchange_secs = 3

[[device]]
id = "bare"
host = "10.0.0.5"
"#;
        let bare = &from_toml_str(text).unwrap().devices[0];
        assert_eq!(bare.username, "admin");
        assert_eq!(bare.password.expose_secret(), "extron");
    }

    #[test]
    fn missing_resolvable_field_names_device_and_field() {
        // No password anywhere.
        let text = r#"
[defaults]
port = 22023
username = "admin"
connect_secs = 5
exchange_secs = 3

[[device]]
id = "no-pass"
host = "10.0.0.5"
"#;
        assert_eq!(
            from_toml_str(text).unwrap_err(),
            ConfigError::MissingField {
                device: "no-pass".into(),
                field: "password",
            }
        );
    }

    #[test]
    fn missing_required_host_is_a_toml_error() {
        let text = r#"
[[device]]
id = "no-host"
"#;
        assert!(matches!(
            from_toml_str(text).unwrap_err(),
            ConfigError::Parse(_)
        ));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
exchange_secs = 3

[[device]]
id = "dup"
host = "10.0.0.1"

[[device]]
id = "dup"
host = "10.0.0.2"
"#;
        assert_eq!(
            from_toml_str(text).unwrap_err(),
            ConfigError::DuplicateId("dup".into())
        );
    }

    #[test]
    fn unknown_field_is_rejected() {
        let text = r#"
[[device]]
id = "typo"
host = "10.0.0.1"
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
exchange_secs = 3
hostname = "oops"
"#;
        assert!(matches!(
            from_toml_str(text).unwrap_err(),
            ConfigError::Parse(_)
        ));
    }

    #[test]
    fn empty_config_yields_no_devices() {
        assert_eq!(from_toml_str("").unwrap(), Resolved::default());
    }

    #[test]
    fn no_defaults_table_is_fine_when_devices_are_complete() {
        let text = r#"
[[device]]
id = "self-contained"
host = "10.0.0.1"
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
exchange_secs = 3
"#;
        assert_eq!(from_toml_str(text).unwrap().devices.len(), 1);
    }

    #[test]
    fn port_and_timeouts_fall_back_to_built_in_defaults() {
        // Neither the device nor a `[defaults]` table names port/connect_secs/exchange_secs.
        let text = r#"
[[device]]
id = "sparse"
host = "10.0.0.5"
username = "admin"
password = "extron"
"#;
        let device = &from_toml_str(text).unwrap().devices[0];
        assert_eq!(device.port, 22023);
        assert_eq!(device.connect_timeout, Duration::from_secs(5));
        assert_eq!(device.exchange_timeout, Duration::from_secs(3));
    }

    #[test]
    fn eager_defaults_off_with_a_standard_sis_keepalive_interval() {
        // A device that mentions neither field is unchanged: lazy, and its
        // (irrelevant-while-lazy) SIS keepalive falls back to the built-in default.
        let atrium = &from_toml_str(USER_EXAMPLE).unwrap().devices[0];
        assert!(!atrium.eager);
        assert_eq!(atrium.sis_keepalive, Some(Duration::from_secs(120)));
        assert_eq!(atrium.eager_retry, Some(Duration::from_secs(30)));
    }

    #[test]
    fn eager_retry_inherits_from_defaults_and_overrides_per_device() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
exchange_secs = 3
eager = true
eager_retry_secs = 45

[[device]]
id = "inherits"
host = "10.0.0.5"

[[device]]
id = "overrides"
host = "10.0.0.6"
eager_retry_secs = 10
"#;
        let devices = from_toml_str(text).unwrap().devices;
        assert_eq!(
            get(&devices, "inherits").eager_retry,
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            get(&devices, "overrides").eager_retry,
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn eager_retry_secs_zero_disables_the_retry() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
exchange_secs = 3
eager = true
eager_retry_secs = 0

[[device]]
id = "give-up"
host = "10.0.0.5"
"#;
        let device = &from_toml_str(text).unwrap().devices[0];
        assert!(device.eager);
        assert_eq!(device.eager_retry, None);
    }

    #[test]
    fn cold_backoff_applies_to_a_lazy_device_too() {
        // The gate guards the connection, not the keep-warm intent, so a device
        // that never mentions `eager` still gets one.
        let atrium = &from_toml_str(USER_EXAMPLE).unwrap().devices[0];
        assert!(!atrium.eager);
        assert_eq!(atrium.cold_backoff, Some(Duration::from_secs(30)));
    }

    #[test]
    fn cold_backoff_inherits_from_defaults_and_overrides_per_device() {
        let text = r#"
[defaults]
username = "admin"
password = "extron"
cold_backoff_secs = 45

[[device]]
id = "inherits"
host = "10.0.0.5"

[[device]]
id = "overrides"
host = "10.0.0.6"
cold_backoff_secs = 10
"#;
        let devices = from_toml_str(text).unwrap().devices;
        assert_eq!(
            get(&devices, "inherits").cold_backoff,
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            get(&devices, "overrides").cold_backoff,
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn cold_backoff_secs_zero_disables_the_gate() {
        // The opt-out: every caller dials, as before the gate existed.
        let text = r#"
[defaults]
username = "admin"
password = "extron"
cold_backoff_secs = 0

[[device]]
id = "always-dials"
host = "10.0.0.5"
"#;
        assert_eq!(from_toml_str(text).unwrap().devices[0].cold_backoff, None);
    }

    #[test]
    fn eager_and_sis_keepalive_inherit_from_defaults() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
exchange_secs = 3
eager = true
sis_keepalive_secs = 90

[[device]]
id = "warm"
host = "10.0.0.5"
"#;
        let warm = &from_toml_str(text).unwrap().devices[0];
        assert!(warm.eager);
        assert_eq!(warm.sis_keepalive, Some(Duration::from_secs(90)));
    }

    #[test]
    fn sis_keepalive_secs_zero_disables_the_sis_keepalive() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
exchange_secs = 3
eager = true
sis_keepalive_secs = 0

[[device]]
id = "warm-once"
host = "10.0.0.5"
"#;
        let device = &from_toml_str(text).unwrap().devices[0];
        assert!(device.eager);
        assert_eq!(device.sis_keepalive, None);
    }

    #[test]
    fn a_device_overrides_eager_and_sis_keepalive_from_defaults() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
exchange_secs = 3
eager = true
sis_keepalive_secs = 120

[[device]]
id = "lazy-one"
host = "10.0.0.5"
eager = false

[[device]]
id = "slow-poll"
host = "10.0.0.6"
sis_keepalive_secs = 30
"#;
        let devices = from_toml_str(text).unwrap().devices;
        let lazy = get(&devices, "lazy-one");
        assert!(!lazy.eager);
        let slow = get(&devices, "slow-poll");
        assert!(slow.eager);
        assert_eq!(slow.sis_keepalive, Some(Duration::from_secs(30)));
    }

    const GROUP_EXAMPLE: &str = r#"
[defaults]
username = "admin"
password = "extron"

[[device]]
id = "room-5-front"
host = "10.0.0.7"

[[device]]
id = "room-5-back"
host = "10.0.0.8"

[[group]]
id = "room-5"
devices = ["room-5-front", "room-5-back"]
"#;

    #[test]
    fn a_group_resolves_over_its_member_devices() {
        let resolved = from_toml_str(GROUP_EXAMPLE).unwrap();
        assert_eq!(resolved.devices.len(), 2);
        assert_eq!(resolved.groups.len(), 1);
        assert_eq!(resolved.groups[0].id, "room-5");
        assert_eq!(
            resolved.groups[0].device_ids,
            vec!["room-5-front", "room-5-back"]
        );
    }

    // ---- the barrier -----------------------------------------------------

    #[test]
    fn a_group_that_names_no_barrier_gets_the_safe_default() {
        let group = &from_toml_str(GROUP_EXAMPLE).unwrap().groups[0];
        // `FailBatch`, because it is the policy that cannot silently produce a
        // half-recorded take.
        assert_eq!(group.barrier, Barrier::FailBatch);
    }

    #[test]
    fn both_barrier_policies_parse_from_their_wire_spellings() {
        for (written, expected) in [
            ("fail", Barrier::FailBatch),
            ("dispatch-ready", Barrier::DispatchReady),
        ] {
            let text = format!("{GROUP_EXAMPLE}barrier = \"{written}\"\n");
            assert_eq!(from_toml_str(&text).unwrap().groups[0].barrier, expected);
        }
    }

    /// A misspelling is a startup error naming the value, not a silent fallback
    /// to the default — which for `barrier` would quietly change what happens
    /// to a half-arrived device group.
    #[test]
    fn a_misspelled_barrier_is_rejected_by_name() {
        let text = format!("{GROUP_EXAMPLE}barrier = \"failed\"\n");
        let err = from_toml_str(&text).unwrap_err();
        assert!(
            format!("{err}").contains("failed"),
            "expected the bad value named, got: {err}"
        );
    }

    #[test]
    fn an_explicit_barrier_timeout_is_taken_as_written() {
        let text = format!("{GROUP_EXAMPLE}barrier_timeout_secs = 15\n");
        assert_eq!(
            from_toml_str(&text).unwrap().groups[0].barrier_timeout,
            Duration::from_secs(15)
        );
    }

    /// Derived from the *slowest* member, because the barrier is filled by the
    /// last member to arrive: a timeout taken from a faster one would fire on
    /// the slow member as a matter of course rather than only when it is stuck.
    #[test]
    fn the_default_barrier_timeout_follows_the_slowest_member() {
        let text = r#"
[defaults]
username = "admin"
password = "extron"

[[device]]
id = "quick"
host = "10.0.0.7"
connect_secs = 1
exchange_secs = 1

[[device]]
id = "slow"
host = "10.0.0.8"
connect_secs = 20
exchange_secs = 5

[[group]]
id = "room-5"
devices = ["quick", "slow"]
"#;
        assert_eq!(
            from_toml_str(text).unwrap().groups[0].barrier_timeout,
            Duration::from_secs(25)
        );
    }

    #[test]
    fn a_group_naming_an_unknown_device_is_rejected() {
        let text = r#"
[[device]]
id = "room-5-front"
host = "10.0.0.7"
username = "admin"
password = "extron"

[[group]]
id = "room-5"
devices = ["room-5-front", "ghost"]
"#;
        assert_eq!(
            from_toml_str(text).unwrap_err(),
            ConfigError::UnknownGroupMember {
                group: "room-5".into(),
                device: "ghost".into(),
            }
        );
    }

    #[test]
    fn an_empty_group_is_rejected() {
        let text = r#"
[[device]]
id = "a"
host = "10.0.0.7"
username = "admin"
password = "extron"

[[group]]
id = "empty"
devices = []
"#;
        assert_eq!(
            from_toml_str(text).unwrap_err(),
            ConfigError::EmptyGroup("empty".into())
        );
    }

    #[test]
    fn a_group_id_colliding_with_a_device_is_rejected() {
        // Groups and devices share one id namespace, so a group may not reuse a
        // device id.
        let text = r#"
[[device]]
id = "room-5"
host = "10.0.0.7"
username = "admin"
password = "extron"

[[group]]
id = "room-5"
devices = ["room-5"]
"#;
        assert_eq!(
            from_toml_str(text).unwrap_err(),
            ConfigError::DuplicateId("room-5".into())
        );
    }

    #[test]
    fn load_reads_from_disk() {
        let path =
            std::env::temp_dir().join(format!("sismatic-devices-{}.toml", std::process::id()));
        std::fs::write(&path, USER_EXAMPLE).unwrap();
        let devices = load(&path).unwrap().devices;
        std::fs::remove_file(&path).ok();
        assert_eq!(devices.len(), 2);
    }

    #[test]
    fn load_rejects_an_extension_without_a_deserializer() {
        let path = std::env::temp_dir().join(format!("sismatic-x-{}.ini", std::process::id()));
        std::fs::write(&path, "irrelevant").unwrap();
        let err = load(&path).unwrap_err();
        std::fs::remove_file(&path).ok();
        assert_eq!(err, ConfigError::UnsupportedFormat("ini".into()));
    }

    // ---- the declared veto ------------------------------------------------

    /// One device, with whatever `[defaults]` and device keys `extra` supplies.
    fn one_device(defaults: &str, extra: &str) -> Result<DeviceConfig, ConfigError> {
        let text = format!(
            "[defaults]\nusername = \"admin\"\npassword = \"extron\"\n{defaults}\n\
             [[device]]\nid = \"smp\"\nhost = \"10.0.0.7\"\n{extra}\n"
        );
        from_toml_str(&text).map(|resolved| resolved.devices.into_iter().next().unwrap())
    }

    #[test]
    fn a_device_with_no_veto_disables_nothing() {
        let device = one_device("", "").unwrap();
        assert!(device.disabled_fields.is_empty());
        assert_eq!(device.auto_disable_after, DEFAULT_AUTO_DISABLE_AFTER);
        assert_eq!(device.self_heal, None, "self-heal is off unless asked for");
    }

    #[test]
    fn a_veto_is_inherited_from_the_defaults_table() {
        let device = one_device("disabled_fields = [\"STREAM_2_NAME\"]", "").unwrap();
        assert_eq!(
            device.disabled_fields,
            BTreeSet::from(["STREAM_2_NAME".to_owned()])
        );
    }

    /// The case that decides `disabled_fields` inherits by *replacement*: a
    /// fleet-wide veto, and the one unit that does hold the license. Under
    /// merge semantics this device could never be excepted, because nothing in
    /// a veto can un-veto.
    #[test]
    fn a_device_list_replaces_the_fleet_list_rather_than_adding_to_it() {
        let licensed = one_device(
            "disabled_fields = [\"STREAM_2_NAME\", \"STREAM_3_NAME\"]",
            "disabled_fields = []",
        )
        .unwrap();
        assert!(
            licensed.disabled_fields.is_empty(),
            "an empty list is how a device says it answers everything"
        );

        let narrowed = one_device(
            "disabled_fields = [\"STREAM_2_NAME\", \"STREAM_3_NAME\"]",
            "disabled_fields = [\"STREAM_3_NAME\"]",
        )
        .unwrap();
        assert_eq!(
            narrowed.disabled_fields,
            BTreeSet::from(["STREAM_3_NAME".to_owned()])
        );
    }

    /// A veto written in an accepted alias has to become the canonical name, or
    /// it vetoes nothing: a poll loop carries `Query::name`, and the two strings
    /// would never meet.
    #[test]
    fn a_veto_written_as_an_alias_is_canonicalized() {
        let device = one_device("", "disabled_fields = [\"stream-name-2\"]").unwrap();
        assert_eq!(
            device.disabled_fields,
            BTreeSet::from(["STREAM_2_NAME".to_owned()])
        );
    }

    /// The three catalogs a field can come from. `TITLE` is a metadata
    /// register and reaches the veto through `Register`, which a read-only
    /// lookup would have refused.
    #[test]
    fn a_veto_may_name_a_write_only_field() {
        let device = one_device("", "disabled_fields = [\"TITLE\"]").unwrap();
        assert_eq!(device.disabled_fields, BTreeSet::from(["TITLE".to_owned()]));
    }

    /// Refused rather than ignored: a misspelled veto disables nothing, and an
    /// operator would learn that only from a device that keeps being polled for
    /// a field they believe they switched off.
    #[test]
    fn a_veto_naming_nothing_is_refused_and_says_which_device() {
        let err = one_device("", "disabled_fields = [\"STREAM_9_NAME\"]").unwrap_err();
        assert_eq!(
            err,
            ConfigError::UnknownDisabledField {
                device: "smp".into(),
                field: "STREAM_9_NAME".into(),
            }
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("smp") && rendered.contains("STREAM_9_NAME"),
            "{rendered}"
        );
    }

    #[test]
    fn the_inference_policy_inherits_like_every_other_key() {
        let inherited = one_device("auto_disable_after = 5\nself_heal_secs = 600", "").unwrap();
        assert_eq!(inherited.auto_disable_after, 5);
        assert_eq!(inherited.self_heal, Some(Duration::from_secs(600)));

        let overridden = one_device(
            "auto_disable_after = 5\nself_heal_secs = 600",
            "auto_disable_after = 0\nself_heal_secs = 0",
        )
        .unwrap();
        assert_eq!(
            overridden.auto_disable_after, 0,
            "zero switches the inference off entirely"
        );
        assert_eq!(
            overridden.self_heal, None,
            "zero is never, the spelling every other interval key uses"
        );
    }

    // ---- derived identity -------------------------------------------------

    /// The property the whole runtime-reconfiguration path rests on: re-reading
    /// an unchanged file yields the same identity, so the registry replaces
    /// nothing and the fleet keeps its warm SSH sessions.
    #[test]
    fn identity_is_a_function_of_configuration() {
        let once = one_device("", "disabled_fields = [\"STREAM_2_NAME\"]").unwrap();
        let twice = one_device("", "disabled_fields = [\"STREAM_2_NAME\"]").unwrap();
        assert_eq!(once.uuid, twice.uuid);
        assert_eq!(once, twice);
    }

    /// ...and any key moving moves it, which is what makes "this device changed"
    /// a `u128` comparison rather than a field-by-field diff.
    #[test]
    fn every_key_participates_in_identity() {
        let base = one_device("", "").unwrap();
        for (defaults, extra) in [
            // `host` is set by the helper, so it is exercised by
            // `two_hosts_are_two_devices` below rather than here.
            ("", "username = \"operator\""),
            ("", "port = 23023"),
            ("", "connect_secs = 20"),
            ("", "eager = true"),
            ("", "cold_backoff_secs = 90"),
            ("", "disabled_fields = [\"STREAM_2_NAME\"]"),
            ("", "auto_disable_after = 5"),
            ("", "self_heal_secs = 600"),
        ] {
            let moved = one_device(defaults, extra).unwrap();
            assert_ne!(
                base.uuid, moved.uuid,
                "`{extra}` left the identity unchanged"
            );
        }
    }

    /// Rotating a credential mints a new device, because the session held under
    /// the old one is no longer the session the config describes. This is also
    /// the assertion that would fail if `fingerprint` ever hashed `Password`'s
    /// `Debug` output, which prints `[REDACTED]` for every value alike.
    #[test]
    fn a_rotated_password_is_a_different_device() {
        let before = one_device("", "password = \"extron\"").unwrap();
        let after = one_device("", "password = \"rotated\"").unwrap();
        assert_ne!(before.uuid, after.uuid);
    }

    /// Reordering a veto is not a change. The set is what the device means, so
    /// an operator tidying the list must not cost a redial.
    #[test]
    fn a_reordered_veto_is_the_same_device() {
        let written = one_device(
            "",
            "disabled_fields = [\"STREAM_3_NAME\", \"STREAM_2_NAME\"]",
        )
        .unwrap();
        let tidied = one_device(
            "",
            "disabled_fields = [\"STREAM_2_NAME\", \"STREAM_3_NAME\"]",
        )
        .unwrap();
        assert_eq!(written.uuid, tidied.uuid);
    }

    /// The same recorder id pointed at a different address is a different
    /// device, and must redial rather than keep the session it holds to the old
    /// one.
    #[test]
    fn two_hosts_are_two_devices() {
        let text = "[defaults]\nusername = \"admin\"\npassword = \"extron\"\n\
                    [[device]]\nid = \"a\"\nhost = \"10.0.0.7\"\n\
                    [[device]]\nid = \"a-moved\"\nhost = \"10.0.0.8\"\n";
        let devices = from_toml_str(text).unwrap().devices;
        assert_ne!(devices[0].uuid, devices[1].uuid);
    }

    /// Two devices differing only by id are two devices — the obvious case, and
    /// the one a fingerprint that forgot to include the id would break.
    #[test]
    fn two_ids_are_two_devices() {
        let text = "[defaults]\nusername = \"admin\"\npassword = \"extron\"\n\
                    [[device]]\nid = \"a\"\nhost = \"10.0.0.7\"\n\
                    [[device]]\nid = \"b\"\nhost = \"10.0.0.7\"\n";
        let devices = from_toml_str(text).unwrap().devices;
        assert_ne!(devices[0].uuid, devices[1].uuid);
    }

    /// Durations are fingerprinted at full precision, not truncated to seconds.
    ///
    /// A regression test for a real collision: hashing `as_secs()` made every
    /// sub-second timeout identical, so two devices differing only below a
    /// second shared a UUID and the registry read them as the same device —
    /// leaving the replacement's tasks bound to the old handle. Nothing in a
    /// *file* can produce a sub-second timeout (`connect_secs` is whole
    /// seconds), which is exactly why this went unnoticed: it is reachable only
    /// by constructing a [`DeviceConfig`] directly, which is public API and is
    /// what every test in the workspace does.
    #[test]
    fn two_timeouts_under_a_second_apart_are_two_devices() {
        let base = one_device("", "").unwrap();
        let quick = DeviceConfig {
            connect_timeout: Duration::from_millis(500),
            ..base.clone()
        }
        .derive_uuid();
        let quicker = DeviceConfig {
            connect_timeout: Duration::from_millis(900),
            ..base
        }
        .derive_uuid();

        assert_ne!(quick.uuid, quicker.uuid);
    }

    /// A length-prefixed encoding, stated as the property it buys: two devices
    /// whose veto lists concatenate to the same bytes are still two devices.
    #[test]
    fn adjacent_fields_cannot_run_together_in_the_fingerprint() {
        let split = one_device(
            "",
            "disabled_fields = [\"STREAM_2_NAME\", \"STREAM_3_NAME\"]",
        )
        .unwrap();
        let joined = one_device("", "disabled_fields = [\"STREAM_2_NAME\"]").unwrap();
        assert_ne!(split.uuid, joined.uuid);
    }
}

/// The resolution core carries no format feature, so it is tested by building a
/// [`RawConfig`] directly — no TOML/YAML/JSON, no filesystem.
#[cfg(test)]
mod resolve_config_tests {
    use super::*;

    #[test]
    fn folds_defaults_and_resolves_directly() {
        let raw = RawConfig {
            defaults: Defaults {
                port: Some(22023),
                username: Some("admin".into()),
                password: Some("extron".into()),
                connect_secs: Some(5),
                exchange_secs: Some(3),
                eager: None,
                sis_keepalive_secs: None,
                eager_retry_secs: None,
                cold_backoff_secs: None,
                disabled_fields: None,
                auto_disable_after: None,
                self_heal_secs: None,
            },
            devices: vec![RawDevice {
                id: "bare".into(),
                host: "10.0.0.5".into(),
                port: None,
                username: None,
                password: None,
                connect_secs: None,
                exchange_secs: None,
                eager: None,
                sis_keepalive_secs: None,
                eager_retry_secs: None,
                cold_backoff_secs: None,
                disabled_fields: None,
                auto_disable_after: None,
                self_heal_secs: None,
            }],
            groups: Vec::new(),
        };
        let resolved = resolve_config(raw).unwrap().devices;
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].port, 22023);
        assert_eq!(resolved[0].username, "admin");
        assert_eq!(resolved[0].connect_timeout, Duration::from_secs(5));
    }

    #[test]
    fn duplicate_ids_are_rejected_without_a_format() {
        let raw = RawConfig {
            defaults: Defaults::default(),
            devices: vec![
                RawDevice {
                    id: "dup".into(),
                    host: "10.0.0.1".into(),
                    port: Some(22023),
                    username: Some("admin".into()),
                    password: Some("extron".into()),
                    connect_secs: Some(5),
                    exchange_secs: Some(3),
                    eager: None,
                    sis_keepalive_secs: None,
                    eager_retry_secs: None,
                    cold_backoff_secs: None,
                    disabled_fields: None,
                    auto_disable_after: None,
                    self_heal_secs: None,
                },
                RawDevice {
                    id: "dup".into(),
                    host: "10.0.0.2".into(),
                    port: Some(22023),
                    username: Some("admin".into()),
                    password: Some("extron".into()),
                    connect_secs: Some(5),
                    exchange_secs: Some(3),
                    eager: None,
                    sis_keepalive_secs: None,
                    eager_retry_secs: None,
                    cold_backoff_secs: None,
                    disabled_fields: None,
                    auto_disable_after: None,
                    self_heal_secs: None,
                },
            ],
            groups: Vec::new(),
        };
        assert_eq!(
            resolve_config(raw).unwrap_err(),
            ConfigError::DuplicateId("dup".into())
        );
    }
}

/// The same logical document in JSON resolves to the same devices as its TOML
/// twin, because both feed one `serde::Deserialize` — see the design note
/// `docs/format-agnostic-config-opt-in-features.md`, Deep dive A.
#[cfg(all(test, feature = "json"))]
mod json_tests {
    use super::*;

    #[test]
    fn json_document_resolves() {
        let text = r#"
        {
          "defaults": { "port": 22023, "username": "admin", "password": "extron",
                        "connect_secs": 5, "exchange_secs": 3 },
          "device": [ { "id": "a", "host": "10.0.0.1" } ]
        }"#;
        let devices = from_json_str(text).unwrap().devices;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "a");
        assert_eq!(devices[0].port, 22023);
        assert_eq!(devices[0].username, "admin");
    }
}

/// The YAML twin resolves identically, exercising the `serde-saphyr` deserializer.
#[cfg(all(test, feature = "yaml"))]
mod yaml_tests {
    use super::*;

    #[test]
    fn yaml_document_resolves() {
        let text = "
defaults:
  port: 22023
  username: admin
  password: extron
  connect_secs: 5
  exchange_secs: 3
device:
  - id: a
    host: \"10.0.0.1\"
";
        let devices = from_yaml_str(text).unwrap().devices;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "a");
        assert_eq!(devices[0].port, 22023);
        assert_eq!(devices[0].username, "admin");
    }
}
