//! The device's mutable field store, seeded from a YAML config.
//!
//! # Why values are wire text
//!
//! A field's type is not introspectable from the catalog — it lives implicitly
//! in which parser [`Query::instruction`] picks. Rather than maintain a second
//! table mapping each field to its type (a fresh source of drift), the config
//! stores each field's **wire representation** as a string, and "well-typed" is
//! defined as *the production parser accepts it*. That works because every
//! query reply in this protocol has the same shape — an untagged value
//! terminated by `CR LF` — so rendering is uniformly `format!("{value}\r\n")`
//! for all 37 queries, with no per-field knowledge at all.
//!
//! The cost is that a bad value is caught at check time rather than parse time;
//! [`DeviceState::check`] closes that gap by running every field through the
//! real parser (see [`Issue`]).
//!
//! # Config is the *initial* state
//!
//! The map is mutable at runtime: register and setting writes overwrite a field
//! and recording commands rewrite `RUNNING_STATE`. The file is a starting
//! point, not a fixed answer table.
//!
//! # License, which is not state at all
//!
//! An Extron SMP sells its second and third stream as licensed options, and a
//! unit without the license does not answer those fields with a default — it
//! *refuses* them, with a SIS error code, every time. That is a different thing
//! from an unset field (which this simulator answers with silence) and a
//! different thing again from a field whose value happens to be empty, and the
//! client distinguishes all three: a refusal feeds `auto_disable_after`, while
//! silence feeds `exchange_timeout`.
//!
//! So the license sits beside the field store rather than in it, as a
//! [`License`] that answers one question — is this field refused — ahead of
//! every lookup. Which fields a stream owns is read out of their canonical
//! names (see [`stream_index`]) rather than tabulated, so the usual rule holds:
//! a stream field added to the catalog is classified the moment it is named.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;

use serde::Deserialize;

use crate::protocol::Step;
use crate::protocol::Value;
use crate::protocol::control_chars::RCDR_LOWER;
use crate::protocol::instructions::commands::Command;
use crate::protocol::instructions::query::Query;
use crate::protocol::instructions::register::{MAX_VALUE_LEN, Register};
use crate::protocol::instructions::setting::Setting;
use crate::protocol::payload_helpers::shorten;
use crate::simulator::dispatch::Request;

/// Credentials the simulated device will accept over keyboard-interactive auth.
#[derive(Debug, Clone, Deserialize)]
pub struct Credentials {
    /// SSH username.
    pub username: String,
    /// SSH password, answered to the device's single `Password:` prompt.
    pub password: String,
}

/// A YAML scalar accepted as a field value.
///
/// Everything is stored as wire text, but requiring the file to quote every
/// number would make it read badly. This accepts the three scalar forms YAML
/// resolves naturally and normalises them: `22023` → `"22023"`, and — the one
/// real convenience — `true`/`false` → `"1"`/`"0"`, which is exactly what
/// `boolean_flag` expects for `SNMP_STATE` and `DHCP_MODE`.
#[derive(Debug, Clone)]
struct WireScalar(String);

impl<'de> Deserialize<'de> for WireScalar {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = WireScalar;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string, integer, or boolean field value")
            }
            fn visit_str<E>(self, v: &str) -> Result<WireScalar, E> {
                Ok(WireScalar(v.to_string()))
            }
            fn visit_i64<E>(self, v: i64) -> Result<WireScalar, E> {
                Ok(WireScalar(v.to_string()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<WireScalar, E> {
                Ok(WireScalar(v.to_string()))
            }
            fn visit_bool<E>(self, v: bool) -> Result<WireScalar, E> {
                Ok(WireScalar(if v { "1" } else { "0" }.to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

/// The on-disk shape of a device config file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(flatten)]
    credentials: Credentials,
    fields: BTreeMap<String, WireScalar>,
    /// Absent means "every stream the catalog knows about", which is what keeps
    /// a config written before licensing existed meaning what it meant.
    #[serde(default)]
    licensed_streams: Option<BTreeSet<u8>>,
    #[serde(default)]
    unlicensed_fields: BTreeSet<String>,
    #[serde(default)]
    refusal_code: Option<u8>,
}

/// Which optional features a unit admits to having.
///
/// Two ways to say it, because operators have two reasons to. `streams` is the
/// one that matches how Extron sells the hardware — stream 2 is Dual Mode,
/// stream 3 is Confidence — and vetoes every field addressing a stream the unit
/// did not buy. `fields` is the escape hatch for everything else a particular
/// unit refuses, named one field at a time; it is the device-side mirror of the
/// client's own `disabled_fields`.
#[derive(Debug, Clone)]
pub struct License {
    /// Stream indices this unit is licensed for.
    streams: BTreeSet<u8>,
    /// Fields refused by name, whatever stream they belong to.
    fields: BTreeSet<String>,
    /// The SIS code sent in place of an answer.
    code: u8,
}

/// The code an unlicensed field is refused with.
///
/// `E13` is "invalid parameter (number is out of range)", which is what an
/// unlicensed stream *is* on the wire: the request addresses stream 2 on a unit
/// whose streams stop at 1, so the index is out of range. It is also the code
/// the field-veto design note names as the one such a unit answers with.
/// Configurable because the scheduler treats every code alike — any refusal
/// counts — so a unit that answers `E14` is worth being able to reproduce.
pub const DEFAULT_REFUSAL_CODE: u8 = 13;

impl Default for License {
    /// Everything licensed — a fully-loaded unit, which is what a config that
    /// says nothing about licensing describes.
    fn default() -> Self {
        License {
            streams: catalog_streams(),
            fields: BTreeSet::new(),
            code: DEFAULT_REFUSAL_CODE,
        }
    }
}

impl License {
    /// Whether this unit refuses `field` outright.
    pub fn refuses(&self, field: &str) -> bool {
        self.fields.contains(field)
            || stream_index(field).is_some_and(|n| !self.streams.contains(&n))
    }

    /// The reply a refusal puts on the wire: the bare `E<code>` line that
    /// [`SisError`](crate::protocol::SisError) reads back.
    pub fn refusal(&self) -> String {
        format!("E{:02}\r\n", self.code)
    }

    /// The streams this unit is licensed for.
    pub fn streams(&self) -> &BTreeSet<u8> {
        &self.streams
    }

    /// The SIS code refusals are answered with.
    pub fn code(&self) -> u8 {
        self.code
    }

    /// Every catalog field this unit refuses — what `--print-config` reports
    /// and what [`DeviceState::check`] exempts from coverage.
    pub fn refused_fields(&self) -> BTreeSet<String> {
        catalog_fields()
            .into_iter()
            .filter(|f| self.refuses(f))
            .collect()
    }
}

/// A way the config disagrees with the instruction catalog. Reported together
/// so `--check-config` can show every problem in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// A config key that is in none of [`Query`], [`Register`] or [`Setting`].
    Unknown {
        /// The key as written in the file.
        key: String,
    },
    /// A catalog field with no entry in the config.
    Missing {
        /// Canonical field name.
        field: String,
    },
    /// The production parser could not decode this field's rendered reply.
    Unparsable {
        /// Canonical field name.
        field: String,
        /// The configured wire text.
        value: String,
    },
    /// A text field decoded to something other than what was configured —
    /// almost always an embedded `CR`, which truncates the reply.
    Mismatch {
        /// Canonical field name.
        field: String,
        /// The configured wire text.
        value: String,
        /// What the parser actually read back.
        decoded: String,
    },
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Issue::Unknown { key } => {
                write!(f, "unknown field `{key}`: not in the instruction catalog")
            }
            Issue::Missing { field } => {
                write!(
                    f,
                    "missing field `{field}`: in the catalog but not the config"
                )
            }
            Issue::Unparsable { field, value } => write!(
                f,
                "field `{field}` = {value:?} is not valid wire text: the parser rejected it"
            ),
            Issue::Mismatch {
                field,
                value,
                decoded,
            } => write!(
                f,
                "field `{field}` = {value:?} decoded as {decoded:?} (an embedded control character?)"
            ),
        }
    }
}

/// Why a config file could not be loaded at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError(pub String);

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoadError {}

/// A simulated device's credentials and live field store.
///
/// Shared across connections behind an `Arc`, so a register written on one
/// session is visible to the next — the simulator models one device, not one
/// device per client.
#[derive(Debug)]
pub struct DeviceState {
    credentials: Credentials,
    /// Canonical field name → current wire text.
    fields: Mutex<BTreeMap<String, String>>,
    /// Keys that matched no catalog entry, kept for [`check`](Self::check)
    /// instead of failing the load, so one pass reports every problem.
    unknown: Vec<String>,
    /// Which fields this unit refuses outright. Not behind the `Mutex`: a
    /// license is what the unit *is*, and no request can change it.
    license: License,
}

/// Values that override a loaded config, in the order a command line beats a
/// file.
///
/// Separate from [`RawConfig`] rather than the same type with everything
/// optional, because the two answer different questions. A file states what the
/// device *is*, and leaving `fields` out of it is an error the drift net
/// reports; an override states what to *change*, and leaving everything out is
/// the normal case. Collapsing them would make a missing `fields:` key
/// indistinguishable from a command line that did not mention fields.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    /// Replace the configured SSH username.
    pub username: Option<String>,
    /// Replace the configured SSH password.
    pub password: Option<String>,
    /// Replace the licensed stream set wholesale. Like the client's
    /// `disabled_fields`, this does not merge — an operator saying `1` means
    /// "this unit has one stream", not "one stream in addition to the file's".
    pub licensed_streams: Option<BTreeSet<u8>>,
    /// Replace the by-name refusal set wholesale, for the same reason.
    pub unlicensed_fields: Option<BTreeSet<String>>,
    /// Replace the SIS code refusals are answered with.
    pub refusal_code: Option<u8>,
    /// Seed individual fields over whatever the file said. Keys are
    /// canonicalised like the file's are, so `--field ssh-port=22` works.
    pub fields: BTreeMap<String, String>,
}

impl DeviceState {
    /// Parse a YAML config into a device.
    ///
    /// Keys are canonicalized through the catalog's own `FromStr`, so the file
    /// inherits its case- and `-`/`_`-insensitivity and its aliases for free:
    /// `ssh-port`, `SSH_PORT` and `Ssh_Port` all name the same field.
    /// Unrecognised keys are retained and reported by [`check`](Self::check).
    pub fn from_yaml_str(text: &str) -> Result<Self, LoadError> {
        Self::from_yaml_str_with(text, &Overrides::default())
    }

    /// Parse a YAML config and apply `overrides` on top of it.
    ///
    /// This is the one loader; [`from_yaml_str`](Self::from_yaml_str) is it with
    /// nothing overridden. Precedence is the conventional one — a command line
    /// beats a file, and a file beats the built-in default — and it is resolved
    /// here rather than in the binary so the integration tests exercise the
    /// same merge an operator gets.
    pub fn from_yaml_str_with(text: &str, overrides: &Overrides) -> Result<Self, LoadError> {
        let raw: RawConfig = serde_saphyr::from_str(text).map_err(|e| LoadError(format!("{e}")))?;

        let mut fields = BTreeMap::new();
        let mut unknown = Vec::new();
        for (key, WireScalar(value)) in raw.fields {
            match canonical_name(&key) {
                Some(name) => {
                    fields.insert(name, value);
                }
                None => unknown.push(key),
            }
        }
        for (key, value) in &overrides.fields {
            match canonical_name(key) {
                Some(name) => {
                    fields.insert(name, value.clone());
                }
                None => unknown.push(key.clone()),
            }
        }

        let mut credentials = raw.credentials;
        if let Some(username) = &overrides.username {
            credentials.username = username.clone();
        }
        if let Some(password) = &overrides.password {
            credentials.password = password.clone();
        }

        // `licensed_streams` absent from both means a fully-loaded unit, not a
        // unit with no streams — so the fallback is the whole catalog, and only
        // an explicit list narrows it.
        let streams = overrides
            .licensed_streams
            .clone()
            .or(raw.licensed_streams)
            .unwrap_or_else(catalog_streams);
        let refused = overrides
            .unlicensed_fields
            .clone()
            .unwrap_or(raw.unlicensed_fields);

        // Canonicalise the by-name veto through the catalog too, so
        // `unlicensed_fields: [stream-2-name]` vetoes the same field
        // `STREAM_2_NAME` names. A name in neither catalog is reported rather
        // than silently vetoing nothing.
        let mut fields_refused = BTreeSet::new();
        for key in refused {
            match canonical_name(&key) {
                Some(name) => {
                    fields_refused.insert(name);
                }
                None => unknown.push(key),
            }
        }

        let license = License {
            streams,
            fields: fields_refused,
            code: overrides
                .refusal_code
                .or(raw.refusal_code)
                .unwrap_or(DEFAULT_REFUSAL_CODE),
        };

        Ok(DeviceState {
            credentials,
            fields: Mutex::new(fields),
            unknown,
            license,
        })
    }

    /// What this unit is licensed for.
    pub fn license(&self) -> &License {
        &self.license
    }

    /// The credentials this device accepts.
    pub fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    /// Current wire text of a field, if set.
    pub fn get(&self, field: &str) -> Option<String> {
        self.lock().get(field).cloned()
    }

    /// Overwrite a field's wire text.
    pub fn set(&self, field: &str, value: impl Into<String>) {
        self.lock().insert(field.to_string(), value.into());
    }

    /// Apply a decoded request and produce the device's reply, or `None` when
    /// the device would stay silent (an unset field, or a setting write whose
    /// value the field's own read parser rejects — either surfaces to the
    /// client as a command timeout, exactly like an unmodelled request).
    pub fn handle(&self, request: Request) -> Option<String> {
        // Ahead of every lookup, because a refusal is not an answer about the
        // field — it is the device declining to have one. An unlicensed field
        // is refused whether or not the config left a value in its cell, and
        // refused on writes as well as reads: a unit without Dual Mode will not
        // be talked into naming its second stream.
        if let Some(field) = request.field()
            && self.license.refuses(field)
        {
            tracing::debug!(
                field,
                code = self.license.code,
                "refusing an unlicensed field"
            );
            return Some(self.license.refusal());
        }

        match request {
            Request::Get(q) => self.get(q.name()).map(|value| format!("{value}\r\n")),

            Request::Set(r, value) => {
                // Mirror the device's own truncation so a client that writes an
                // over-long value reads back what the real hardware would.
                let value = shorten(&value, MAX_VALUE_LEN);
                self.set(r.name(), value.clone());
                Some(format!("{RCDR_LOWER}{}*{value}\r\n", r.reg()))
            }

            Request::Do(c) => {
                self.set(Query::RunningState.name(), running_state_code(c));
                Some(format!("{RCDR_LOWER}{}\r\n", c.verb()))
            }

            // The echo comes from the catalog rather than being spelled here:
            // a setting's reply shape depends on its `Form`, and the addressed
            // ones (every stream and RTMP field) repeat the address back with
            // the verb capitalised. `Setting::echo` is built from the same
            // `form`/`verb` its parser is, so the two cannot disagree.
            Request::Configure(s, value) => survives_the_read_path(s.name(), &value).then(|| {
                self.set(s.name(), value.clone());
                s.echo(&value)
            }),
        }
    }

    /// The SMP login banner, built from the configured fields so it cannot
    /// disagree with what a client subsequently queries.
    pub fn banner(&self) -> String {
        let field = |q: Query| self.get(q.name()).unwrap_or_default();
        format!(
            "\r\n(c) Copyright 2014-2023, Extron Electronics, {}, V{}, {}\r\n{BANNER_DATE}\r\n",
            field(Query::ModelName),
            field(Query::Firmware),
            field(Query::PartNumber),
        )
    }

    /// Cross-check the config against the instruction catalog.
    ///
    /// Three checks, in the order the failures matter:
    ///
    /// 1. **Coverage**, both directions — every config key is a catalog field,
    ///    and every catalog field (`Query::ALL ∪ Register::ALL`) has a value.
    ///    Adding a catalog variant makes this fail until the config follows.
    /// 2. **Decodability** — each query's rendered reply is fed to that query's
    ///    *production* parser. This is the check that no hand-written table can
    ///    fake: if the simulator's rendering and the client's parser ever
    ///    disagree, it fails here rather than as a mysterious timeout.
    /// 3. **Fidelity**, for text fields only — the decoded text must equal the
    ///    configured text, which catches embedded control characters that
    ///    silently truncate a reply.
    ///
    /// Checks 2 and 3 need a read path, so a write-only field (in `Register`
    /// but not `Query`) would get only check 1. There are none today, and
    /// `every_settable_register_has_a_read_path` keeps it that way.
    pub fn check(&self) -> Vec<Issue> {
        let mut issues: Vec<Issue> = self
            .unknown
            .iter()
            .map(|key| Issue::Unknown { key: key.clone() })
            .collect();

        let fields = self.lock().clone();
        for field in catalog_fields() {
            // A refused field is never served, so a value for it would be
            // decoration. Requiring one would make a one-stream unit's config
            // carry eight fields it will never answer, and the operator would
            // be inventing values to satisfy a check about a field the device
            // denies having.
            if self.license.refuses(&field) {
                continue;
            }
            if !fields.contains_key(&field) {
                issues.push(Issue::Missing { field });
            }
        }

        for &q in Query::ALL {
            if self.license.refuses(q.name()) {
                continue;
            }
            let Some(value) = fields.get(q.name()) else {
                continue; // already reported as Missing
            };
            let rendered = format!("{value}\r\n");
            match q.instruction().parse_step(&rendered) {
                Step::Done(Value::Text(decoded)) if &decoded != value => {
                    issues.push(Issue::Mismatch {
                        field: q.name().to_string(),
                        value: value.clone(),
                        decoded,
                    })
                }
                Step::Done(_) => {}
                Step::NeedMore => issues.push(Issue::Unparsable {
                    field: q.name().to_string(),
                    value: value.clone(),
                }),
            }
        }

        issues
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.fields.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Fixed timestamp in the login banner. The banner is drained by the client
/// before its first query, so nothing parses it; keeping it constant keeps the
/// simulator's output reproducible.
const BANNER_DATE: &str = "Mon, 13 Jul 2026 08:16:21";

/// Every field the protocol can read or write:
/// `Query::ALL ∪ Register::ALL ∪ Setting::ALL`, joined on the canonical name.
/// `TITLE` appears in two of them and is one field — which is what makes a
/// write visible to a later query without any pairing table.
///
/// `Setting::ALL` contributes nothing to the union today, because every setting
/// names a field some query also reads (`every_setting_has_a_read_path`). It is
/// chained in anyway so that a write-only setting would be *reported* as a
/// missing config field rather than quietly escaping coverage.
pub fn catalog_fields() -> BTreeSet<String> {
    Query::ALL
        .iter()
        .map(|q| q.name().to_string())
        .chain(Register::ALL.iter().map(|r| r.name().to_string()))
        .chain(Setting::ALL.iter().map(|s| s.name().to_string()))
        .collect()
}

/// Which stream a field addresses, read out of its canonical name.
///
/// Derived rather than tabulated, for the reason everything else here is: the
/// index is already *in* the name — `STREAM_2_NAME`, `RTMP_2_URL`,
/// `RTMP_BACKUP_STREAM_2_LIVE_STATE` — and the catalog puts it in the same
/// place throughout (see the naming note in `setting.rs`), so reading it back
/// needs no second list to keep in step. A stream field added later is
/// classified the moment it is named.
///
/// The prefix guard is what keeps the digit from being over-read: only the two
/// stream-addressed families are considered, so `HTTPS_PORT` or
/// `SNMP_PUBLIC_COMMUNITY_STRING` can never be mistaken for a stream however
/// they are spelled. `stream_fields_are_exactly_the_stream_addressed_catalog`
/// pins the resulting partition against the catalog, so a rename that moves the
/// index out of reach fails a test rather than quietly licensing a field that
/// should have been refused.
pub fn stream_index(field: &str) -> Option<u8> {
    if !(field.starts_with("STREAM_") || field.starts_with("RTMP_")) {
        return None;
    }
    // The first `_`-delimited component that is a number. No upper bound: if
    // the catalog ever grows a stream 4, it is licensed like the other three
    // rather than silently exempt.
    field.split('_').find_map(|part| part.parse::<u8>().ok())
}

/// Every stream index the catalog mentions — the license default, and the set a
/// `licensed_streams` list is understood against.
pub fn catalog_streams() -> BTreeSet<u8> {
    catalog_fields()
        .iter()
        .filter_map(|f| stream_index(f))
        .collect()
}

/// Resolve a config key to its canonical catalog name, trying every catalog.
pub fn canonical_name(key: &str) -> Option<String> {
    Query::from_str(key)
        .map(|q| q.name().to_string())
        .or_else(|_| Register::from_str(key).map(|r| r.name().to_string()))
        .or_else(|_| Setting::from_str(key).map(|s| s.name().to_string()))
        .ok()
}

/// Whether `value` would survive being read back out of `field`: the
/// simulator's own rendering of it, fed to that field's *production* parser.
///
/// The two conditions are exactly what [`DeviceState::check`] reports as
/// [`Issue::Unparsable`] and [`Issue::Mismatch`], applied to a runtime write
/// instead of the config file — so the store's invariant ("every cell holds
/// wire text the client can decode") holds for a running device and not only
/// for its starting state. Without it, one hand-crafted write could poison a
/// cell and turn every later read of that field into a mysterious timeout.
///
/// Refusing here is also what the device does. A value it will not take is
/// answered with an error token this catalog does not model
/// (see [`Setting::instruction`]), which reaches the caller as a command
/// timeout — which is what the simulator's silence produces.
///
/// This never refuses a value that came from [`Setting::instruction`]: a flag
/// is already `0`/`1`, a port is already in range, and free text is already
/// within its ceiling. What it catches is a request no client of ours built.
fn survives_the_read_path(field: &str, value: &str) -> bool {
    let Ok(query) = Query::from_str(field) else {
        // A write-only field has no read path to disagree with. There are none
        // (`every_setting_has_a_read_path`); this keeps the simulator serving
        // if one is added deliberately.
        return true;
    };
    match query.instruction().parse_step(&format!("{value}\r\n")) {
        // An embedded CR truncates the reply, so the device would read back
        // something other than what was written.
        Step::Done(Value::Text(decoded)) => decoded == value,
        Step::Done(_) => true,
        Step::NeedMore => false,
    }
}

/// The `RUNNING_STATE` wire code a command leaves behind. Written as an
/// explicit match so a new [`Command`] cannot compile without deciding what it
/// does to the recording state; `command_sets_running_state` in the integration
/// tests pins these codes against [`Query::RunningState`]'s parser.
fn running_state_code(c: Command) -> &'static str {
    match c {
        Command::Stop => "0",
        Command::Start => "1",
        Command::Pause => "2",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "\
username: admin
password: extron
fields:
  ssh-port: 22023
  SNMP_STATE: true
  DHCP_MODE: false
  TITLE: Lecture 1
  not_a_field: nope
";

    #[test]
    fn keys_are_canonicalised_through_the_catalog() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        // `ssh-port` normalises to the canonical `SSH_PORT`, and the YAML
        // integer became wire text.
        assert_eq!(state.get("SSH_PORT").as_deref(), Some("22023"));
    }

    #[test]
    fn booleans_become_flag_codes() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        assert_eq!(state.get("SNMP_STATE").as_deref(), Some("1"));
        assert_eq!(state.get("DHCP_MODE").as_deref(), Some("0"));
    }

    #[test]
    fn unknown_keys_are_reported_not_ignored() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        assert!(state.check().contains(&Issue::Unknown {
            key: "not_a_field".into()
        }));
    }

    #[test]
    fn a_sparse_config_reports_every_absent_catalog_field() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        let missing: BTreeSet<String> = state
            .check()
            .into_iter()
            .filter_map(|i| match i {
                Issue::Missing { field } => Some(field),
                _ => None,
            })
            .collect();
        assert!(missing.contains("FIRMWARE"));
        assert!(missing.contains("PUBLISHER"));
        // The four fields that *are* configured must not be reported.
        assert!(!missing.contains("SSH_PORT"));
        assert!(!missing.contains("TITLE"));
    }

    #[test]
    fn a_badly_typed_value_is_reported() {
        let text = "\
username: admin
password: extron
fields:
  SSH_PORT: not-a-port
";
        let state = DeviceState::from_yaml_str(text).expect("loads");
        assert!(state.check().contains(&Issue::Unparsable {
            field: "SSH_PORT".into(),
            value: "not-a-port".into()
        }));
    }

    /// An embedded CR truncates the reply: the parser reads `b`, not `a\rb`.
    #[test]
    fn an_embedded_control_character_is_reported_as_a_mismatch() {
        let state = DeviceState::from_yaml_str(
            "username: admin\npassword: extron\nfields:\n  UNIT_NAME: \"a\\rb\"\n",
        )
        .expect("loads");
        assert!(
            state
                .check()
                .iter()
                .any(|i| matches!(i, Issue::Mismatch { field, .. } if field == "UNIT_NAME"))
        );
    }

    #[test]
    fn a_register_write_is_visible_to_the_matching_query() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        state.handle(Request::Set(Register::Title, "New Title".into()));
        // Both catalogs canonicalise to "TITLE", so the write lands where the
        // read looks — no pairing table involved.
        assert_eq!(
            state.handle(Request::Get(Query::Title)),
            Some("New Title\r\n".into())
        );
    }

    #[test]
    fn an_over_long_register_write_is_truncated_like_the_device() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        let long = "x".repeat(MAX_VALUE_LEN + 10);
        state.handle(Request::Set(Register::Title, long));
        assert_eq!(state.get("TITLE").map(|v| v.len()), Some(MAX_VALUE_LEN));
    }

    /// The settings half of `a_register_write_is_visible_to_the_matching_query`.
    /// `Setting` and `Query` spell `TIMEZONE` the same way, so the write lands
    /// in the cell the read looks in with no pairing table involved.
    #[test]
    fn a_setting_write_is_visible_to_the_matching_query() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        state.handle(Request::Configure(
            Setting::Timezone,
            "Europe/Vienna".into(),
        ));
        assert_eq!(
            state.handle(Request::Get(Query::Timezone)),
            Some("Europe/Vienna\r\n".into())
        );
    }

    /// The drift net for the write echo, without a socket: the bytes the
    /// simulator answers a setting write with are fed to *that setting's own
    /// production parser*. The echo shape lives in two places by necessity —
    /// `setting_echo` parses it and `handle` renders it — and this is what
    /// keeps them the same shape.
    #[test]
    fn a_setting_write_echo_satisfies_the_clients_own_parser() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        let value = "Europe/Vienna";
        let echo = state
            .handle(Request::Configure(Setting::Timezone, value.into()))
            .expect("a legal setting write is answered");
        assert_eq!(
            Setting::Timezone
                .instruction(value)
                .expect("a timezone is free text")
                .parse_step(&echo),
            Step::Done(Value::Text(value.into())),
            "the client's parser could not read the simulator's echo"
        );
    }

    /// A value no `Setting::instruction` would ever produce, arriving anyway.
    /// Storing it would leave `DHCP_MODE` holding text the flag parser cannot
    /// read, so every later read of the field would time out instead.
    #[test]
    fn a_setting_write_the_read_parser_would_reject_is_ignored() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        assert_eq!(
            state.handle(Request::Configure(Setting::DhcpMode, "maybe".into())),
            None
        );
        assert_eq!(state.get("DHCP_MODE").as_deref(), Some("0"), "unchanged");
    }

    // ---- license ------------------------------------------------------

    /// The guard the whole license rests on: reading the index out of a field
    /// name must partition the catalog exactly the way the catalog is shaped.
    ///
    /// Spelled as a set comparison rather than a handful of spot checks,
    /// because both directions can fail and they fail differently. A stream
    /// field that stops being recognised is silently *licensed* on a unit that
    /// should refuse it — the dangerous direction, and invisible without this.
    /// A non-stream field that starts being recognised gets refused by a
    /// `licensed_streams` list that never meant to name it.
    #[test]
    fn stream_fields_are_exactly_the_stream_addressed_catalog() {
        let derived: BTreeSet<String> = catalog_fields()
            .into_iter()
            .filter(|f| stream_index(f).is_some())
            .collect();

        let expected: BTreeSet<String> = [
            "STREAM_1_NAME",
            "STREAM_2_NAME",
            "STREAM_3_NAME",
            "STREAM_1_STATE",
            "STREAM_2_STATE",
            "STREAM_3_STATE",
            "RTMP_1_URL",
            "RTMP_2_URL",
            "RTMP_3_URL",
            "RTMP_1_BACKUP_URL",
            "RTMP_2_BACKUP_URL",
            "RTMP_3_BACKUP_URL",
            "RTMP_1_STATE",
            "RTMP_2_STATE",
            "RTMP_3_STATE",
            "RTMP_STREAM_1_LIVE_STATE",
            "RTMP_STREAM_2_LIVE_STATE",
            "RTMP_STREAM_3_LIVE_STATE",
            "RTMP_BACKUP_STREAM_1_LIVE_STATE",
            "RTMP_BACKUP_STREAM_2_LIVE_STATE",
            "RTMP_BACKUP_STREAM_3_LIVE_STATE",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        assert_eq!(
            derived, expected,
            "the fields `stream_index` recognises no longer match the catalog"
        );
        assert_eq!(catalog_streams(), BTreeSet::from([1, 2, 3]));
    }

    /// Each index claims its own fields and no others, so a one-stream license
    /// cannot refuse stream 1 by association.
    #[test]
    fn every_stream_field_is_claimed_by_its_own_index() {
        for field in catalog_fields() {
            let Some(n) = stream_index(&field) else {
                continue;
            };
            assert!(
                field.contains(&format!("_{n}_")) || field.ends_with(&format!("_{n}")),
                "{field} was read as stream {n}"
            );
        }
    }

    /// The fields a port or a community string are named with must never be
    /// read as streams, whatever digits they contain.
    #[test]
    fn a_field_that_merely_contains_a_digit_is_not_a_stream() {
        for field in ["HTTPS_PORT", "SNMP_PORT", "SNMP_PUBLIC_COMMUNITY_STRING"] {
            assert_eq!(stream_index(field), None, "{field}");
        }
    }

    const BASE_UNIT: &str = "\
username: admin
password: extron
licensed_streams: [1]
fields:
  STREAM_1_NAME: Archive Channel A
";

    /// A one-stream unit refuses stream 2 and 3 and still answers stream 1 —
    /// the Extron license this exists to reproduce.
    #[test]
    fn an_unlicensed_stream_is_refused_and_a_licensed_one_is_answered() {
        let state = DeviceState::from_yaml_str(BASE_UNIT).expect("loads");

        assert_eq!(
            state.handle(Request::Get(Query::Stream1Name)),
            Some("Archive Channel A\r\n".into())
        );
        assert_eq!(
            state.handle(Request::Get(Query::Stream2Name)),
            Some("E13\r\n".into())
        );
        assert_eq!(
            state.handle(Request::Get(Query::Stream3State)),
            Some("E13\r\n".into())
        );
    }

    /// The refusal is what [`SisError`](crate::protocol::SisError) reads, not
    /// merely a line that looks like one. This is the pairing that makes the
    /// simulator able to drive `auto_disable_after`: the client counts refusals
    /// by running exactly this function over what arrives.
    #[test]
    fn a_refusal_is_read_back_as_a_sis_error_by_the_client() {
        use crate::protocol::SisError;

        let state = DeviceState::from_yaml_str(BASE_UNIT).expect("loads");
        let reply = state
            .handle(Request::Get(Query::Stream2Name))
            .expect("a refusal is a reply");

        assert_eq!(SisError::in_reply(&reply), Some(SisError { code: 13 }));
    }

    /// An unlicensed stream cannot be talked into existing: the write is
    /// refused too, not merely unanswered, and the cell stays untouched.
    #[test]
    fn a_write_to_an_unlicensed_stream_is_refused_and_changes_nothing() {
        let state = DeviceState::from_yaml_str(BASE_UNIT).expect("loads");

        assert_eq!(
            state.handle(Request::Configure(Setting::Stream2Name, "Hall B".into())),
            Some("E13\r\n".into())
        );
        assert_eq!(state.get("STREAM_2_NAME"), None);
    }

    /// A refusal and silence are different replies, and the difference is the
    /// reason the license is not modelled as an absent field: one feeds
    /// `auto_disable_after`, the other `exchange_timeout`.
    #[test]
    fn a_refusal_is_not_the_same_as_an_unset_field() {
        let state = DeviceState::from_yaml_str(BASE_UNIT).expect("loads");

        // Licensed but unset -> silence.
        assert_eq!(state.handle(Request::Get(Query::Stream1State)), None);
        // Unlicensed -> a refusal.
        assert_eq!(
            state.handle(Request::Get(Query::Stream2State)),
            Some("E13\r\n".into())
        );
    }

    /// An unlicensed field needs no configured value: a base unit's file should
    /// not have to invent values for the eight fields it denies having.
    #[test]
    fn check_does_not_ask_for_values_it_will_never_serve() {
        let missing: BTreeSet<String> = DeviceState::from_yaml_str(BASE_UNIT)
            .expect("loads")
            .check()
            .into_iter()
            .filter_map(|i| match i {
                Issue::Missing { field } => Some(field),
                _ => None,
            })
            .collect();

        assert!(!missing.contains("STREAM_2_NAME"), "{missing:?}");
        assert!(!missing.contains("RTMP_2_URL"), "{missing:?}");
        // Stream 1 is licensed, so its unset fields are still reported.
        assert!(missing.contains("STREAM_1_STATE"), "{missing:?}");
    }

    /// The by-name veto, which is the device-side mirror of the client's
    /// `disabled_fields` — and it canonicalises like every other key, so an
    /// alias or a lowercase spelling vetoes the field it names.
    #[test]
    fn unlicensed_fields_are_refused_by_name_through_the_catalog() {
        let state = DeviceState::from_yaml_str(
            "\
username: admin
password: extron
unlicensed_fields: [snmp-state, STREAM_NAME_3]
fields:
  SNMP_STATE: true
  STREAM_3_NAME: Confidence
",
        )
        .expect("loads");

        // `snmp-state` reached `SNMP_STATE`, though it is not a stream at all.
        assert_eq!(
            state.handle(Request::Get(Query::SnmpState)),
            Some("E13\r\n".into())
        );
        // `STREAM_NAME_3` is an alias of `STREAM_3_NAME`.
        assert_eq!(
            state.handle(Request::Get(Query::Stream3Name)),
            Some("E13\r\n".into())
        );
    }

    /// A veto naming a field in no catalog is reported rather than silently
    /// vetoing nothing — the same treatment an unknown `fields:` key gets.
    #[test]
    fn an_unlicensed_field_outside_the_catalog_is_reported() {
        let state = DeviceState::from_yaml_str(
            "username: admin\npassword: extron\nunlicensed_fields: [NOT_A_FIELD]\nfields: {}\n",
        )
        .expect("loads");

        assert!(state.check().contains(&Issue::Unknown {
            key: "NOT_A_FIELD".into()
        }));
    }

    #[test]
    fn the_refusal_code_is_configurable() {
        let state = DeviceState::from_yaml_str(
            "\
username: admin
password: extron
licensed_streams: [1]
refusal_code: 14
fields: {}
",
        )
        .expect("loads");

        assert_eq!(
            state.handle(Request::Get(Query::Stream2Name)),
            Some("E14\r\n".into())
        );
    }

    /// Saying nothing about licensing describes a fully-loaded unit, which is
    /// what keeps every config written before licensing existed meaning what it
    /// meant.
    #[test]
    fn a_config_silent_about_licensing_licenses_everything() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        for field in catalog_fields() {
            assert!(
                !state.license().refuses(&field),
                "{field} was refused by a config that said nothing about licenses"
            );
        }
    }

    #[test]
    fn an_unset_field_stays_silent() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        assert_eq!(state.handle(Request::Get(Query::Firmware)), None);
    }
}
