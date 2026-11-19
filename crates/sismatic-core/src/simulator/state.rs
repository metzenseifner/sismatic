//! The device's mutable field store, seeded from a YAML config.
//!
//! # Why values are wire text
//!
//! A field's type is not introspectable from the catalog — it lives implicitly
//! in which parser [`Query::instruction`] picks. Rather than keep a second
//! table mapping each field to its type (a fresh source of drift), the config
//! stores each field's **wire representation** as a string, and "well-typed"
//! means *the production parser accepts it*. That works because every query
//! reply in this protocol has the same shape — an untagged value terminated by
//! `CR LF` — so rendering is uniformly `format!("{value}\r\n")` for every
//! query, with no per-field knowledge at all.
//!
//! The cost is that a bad value is caught at check time rather than parse time.
//! [`DeviceState::check`] closes that gap by running every field through the
//! real parser, and `survives_the_read_path` does the same for a runtime
//! write, so the store's invariant — every cell holds text the client can
//! decode — holds for a running device and not only for its starting state.
//!
//! The map is mutable: writes overwrite a field and recording commands rewrite
//! `RUNNING_STATE`. The license is the exception, and the module docs say why
//! it sits beside the store rather than in it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;

use serde::Deserialize;

use crate::protocol::RecordingEvents;
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

/// A YAML scalar accepted as a field value, normalised to wire text.
///
/// Requiring the file to quote every number would read badly, so the three
/// scalar forms YAML resolves naturally are all accepted: `22023` → `"22023"`,
/// and `true`/`false` → `"1"`/`"0"`, which is what `boolean_flag` expects for
/// `SNMP_STATE` and `DHCP_MODE`.
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

/// A field value as the config file writes it.
///
/// Three forms, because one of them has to carry a *list*. Nearly every reply
/// in this protocol is one line, which [`WireScalar`] covers; `RECORDING_EVENTS`
/// is a list of JSON objects, and a list is not something a YAML scalar or a
/// `--field NAME=VALUE` flag can hold. So the file may also give a
/// [`RecordingEvents`] document inline, or name a file holding one:
///
/// ```yaml
/// fields:
///   UNIT_NAME: Main Hall SMP            # a scalar, as before
///   RECORDING_EVENTS:
///     recording_events:                 # the document, inline
///       - db_id: 1
///         title: Lecture 1
/// ```
///
/// ```yaml
///   RECORDING_EVENTS:
///     file: ./recordings.json           # the same document, from a file
/// ```
///
/// All three end up as the one thing the store holds — wire text — so nothing
/// downstream of the loader knows which form was used.
///
/// The mapping forms are read key-first rather than through an untagged enum:
/// a mapping that is neither is then an error naming the key the file actually
/// wrote, instead of serde's "data did not match any variant".
#[derive(Debug, Clone)]
enum RawFieldValue {
    /// A single-line value, used verbatim as wire text.
    Scalar(WireScalar),
    /// A recording-event document written out in the config itself.
    Events(RecordingEvents),
    /// A path to a file holding that document as JSON (or YAML — the loader
    /// reads both, so `.json` and `.yaml` are equally valid).
    ///
    /// Relative to the **process working directory**, not to the config: a
    /// config may be a compiled-in string (`simulated-device` with no
    /// `--config`) or a `--device-config-for` template, so there is not always
    /// a config directory to anchor to, and one rule everywhere beats a rule
    /// that depends on where the YAML came from.
    File(PathBuf),
}

/// The keys [`RawFieldValue`] accepts in its mapping form, for error messages.
const DOCUMENT_KEYS: &str = "`recording_events` or `file`";

impl<'de> Deserialize<'de> for RawFieldValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = RawFieldValue;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(
                    f,
                    "a string, integer, or boolean field value, or a mapping with {DOCUMENT_KEYS}"
                )
            }

            fn visit_str<E>(self, v: &str) -> Result<RawFieldValue, E> {
                Ok(RawFieldValue::Scalar(WireScalar(v.to_string())))
            }
            fn visit_i64<E>(self, v: i64) -> Result<RawFieldValue, E> {
                Ok(RawFieldValue::Scalar(WireScalar(v.to_string())))
            }
            fn visit_u64<E>(self, v: u64) -> Result<RawFieldValue, E> {
                Ok(RawFieldValue::Scalar(WireScalar(v.to_string())))
            }
            fn visit_bool<E>(self, v: bool) -> Result<RawFieldValue, E> {
                Ok(RawFieldValue::Scalar(WireScalar(
                    if v { "1" } else { "0" }.to_string(),
                )))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<RawFieldValue, A::Error> {
                use serde::de::Error as _;

                let Some(key) = map.next_key::<String>()? else {
                    return Err(A::Error::custom(format!(
                        "an empty mapping is not a field value; expected {DOCUMENT_KEYS}"
                    )));
                };
                let value = match key.as_str() {
                    "recording_events" => RawFieldValue::Events(RecordingEvents {
                        recording_events: map.next_value()?,
                    }),
                    "file" => RawFieldValue::File(map.next_value::<PathBuf>()?),
                    other => {
                        return Err(A::Error::custom(format!(
                            "unknown field-value key `{other}`; expected {DOCUMENT_KEYS}"
                        )));
                    }
                };
                // Both forms are the whole value, so a second key is a config
                // that means two things at once rather than one to merge.
                if let Some(extra) = map.next_key::<String>()? {
                    return Err(A::Error::custom(format!(
                        "a field value takes one of {DOCUMENT_KEYS}, not both: found `{key}` and `{extra}`"
                    )));
                }
                Ok(value)
            }
        }
        d.deserialize_any(V)
    }
}

impl RawFieldValue {
    /// The wire text this value seeds its field with.
    ///
    /// The document forms render through [`RecordingEvents::wire_body`], so the
    /// framing comes from the protocol layer that parses it rather than from
    /// here — the rule the module docs set out, applied to a value that happens
    /// to span several lines.
    fn into_wire_text(self) -> Result<String, LoadError> {
        match self {
            RawFieldValue::Scalar(WireScalar(text)) => Ok(text),
            RawFieldValue::Events(document) => Ok(document.wire_body()),
            RawFieldValue::File(path) => {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| LoadError(format!("cannot read {}: {e}", path.display())))?;
                // One reader for both extensions: JSON is a subset of YAML, so
                // a `{"recording_events": [...]}` document parses either way
                // and an operator need not think about which they wrote.
                let document: RecordingEvents = serde_saphyr::from_str(&text).map_err(|e| {
                    LoadError(format!(
                        "{} is not a recording-event document: {e}",
                        path.display()
                    ))
                })?;
                Ok(document.wire_body())
            }
        }
    }
}

/// The on-disk shape of a device config file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(flatten)]
    credentials: Credentials,
    fields: BTreeMap<String, RawFieldValue>,
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
/// Two ways to say it. `streams` matches how Extron sells the hardware — stream
/// 2 is Dual Mode, stream 3 is Confidence — and vetoes every field addressing a
/// stream the unit did not buy. `fields` names anything else this particular
/// unit refuses, one field at a time; it is the device-side mirror of the
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
/// unlicensed stream *is* on the wire — a request addressing stream 2 on a unit
/// whose streams stop at 1 — and the code the field-veto design note names.
/// Configurable because the scheduler treats every code alike, so a unit that
/// answers `E14` is worth being able to reproduce.
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
/// Deliberately not `RawConfig` with everything optional: a file states what
/// the device *is*, so a missing `fields:` key is an error the drift net
/// reports, while an override states what to *change*, so saying nothing is the
/// normal case. One type could not tell those apart.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    /// Replace the configured SSH username.
    pub username: Option<String>,
    /// Replace the configured SSH password.
    pub password: Option<String>,
    /// Replace the licensed stream set wholesale: `1` means "this unit has one
    /// stream", not "one more than the file's". Merging would make a narrower
    /// command line impossible to express.
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
    /// The one loader; [`from_yaml_str`](Self::from_yaml_str) is it with
    /// nothing overridden. The merge lives here rather than in the binary, so
    /// the integration tests exercise the precedence an operator gets.
    pub fn from_yaml_str_with(text: &str, overrides: &Overrides) -> Result<Self, LoadError> {
        let raw: RawConfig = serde_saphyr::from_str(text).map_err(|e| LoadError(format!("{e}")))?;

        let mut fields = BTreeMap::new();
        let mut unknown = Vec::new();
        for (key, raw_value) in raw.fields {
            match canonical_name(&key) {
                Some(name) => {
                    // Resolved here rather than at use time so a config naming
                    // an unreadable file fails the load, where the path is
                    // still in hand to name, instead of answering a query with
                    // an empty list later.
                    fields.insert(name, raw_value.into_wire_text()?);
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

        // Absent from both means a fully-loaded unit, not a unit with no
        // streams: the fallback is the whole catalog, and only an explicit list
        // narrows it. An explicit empty list is still a unit with no streams.
        let streams = overrides
            .licensed_streams
            .clone()
            .or(raw.licensed_streams)
            .unwrap_or_else(catalog_streams);
        let refused = overrides
            .unlicensed_fields
            .clone()
            .unwrap_or(raw.unlicensed_fields);

        // Canonicalised like a field key, so `unlicensed_fields:
        // [stream-2-name]` vetoes what `STREAM_2_NAME` names. A name in no
        // catalog is reported rather than silently vetoing nothing.
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
        // Ahead of every lookup: a refusal is the device declining to have the
        // field, not an answer about it. So it holds whether or not the config
        // left a value in the cell, and on writes as well as reads — a unit
        // without Dual Mode will not be talked into naming its second stream.
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

            // The echo comes from the catalog, not from here: a setting's
            // reply shape depends on its `Form`, and the addressed ones repeat
            // the address back with the verb capitalised. `Setting::echo` is
            // built from the same `form`/`verb` its parser is.
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
    ///    and every catalog field has a value. Adding a catalog variant fails
    ///    this until the config follows.
    /// 2. **Decodability** — each query's rendered reply is fed to that query's
    ///    *production* parser, so a divergence between what the simulator
    ///    writes and what the client reads fails here rather than later as a
    ///    mysterious timeout.
    /// 3. **Fidelity**, text fields only — the decoded text must equal the
    ///    configured text, catching embedded control characters that silently
    ///    truncate a reply.
    ///
    /// Checks 2 and 3 need a read path, so the write-only settings
    /// (`names(Setting) \ names(Query)`, the nine RTMP push fields) get only
    /// check 1; `write_only_settings_are_exactly_the_attested_rtmp_hole` keeps
    /// that set from growing unnoticed. A refused field skips all three.
    pub fn check(&self) -> Vec<Issue> {
        let mut issues: Vec<Issue> = self
            .unknown
            .iter()
            .map(|key| Issue::Unknown { key: key.clone() })
            .collect();

        let fields = self.lock().clone();
        for field in catalog_fields() {
            // A refused field is never served, so requiring a value would
            // make a base unit's config invent fourteen of them to satisfy a
            // check about fields the device denies having.
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
/// A name in two catalogs (`TITLE`) is one field, which is what makes a write
/// visible to a later query without a pairing table.
///
/// `Setting::ALL` is chained in for the nine RTMP push fields it alone names:
/// without it they would escape coverage rather than being reported as missing
/// from the config.
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
/// Derived, not tabulated: the index is already *in* the name —
/// `STREAM_2_NAME`, `RTMP_2_URL`, `RTMP_BACKUP_STREAM_2_LIVE_STATE` — so a
/// stream field added later is classified the moment it is named.
///
/// The prefix guard keeps the digit from being over-read: only the two
/// stream-addressed families are considered, so `HTTPS_PORT` and
/// `SNMP_PUBLIC_COMMUNITY_STRING` can never be read as streams.
/// `stream_fields_are_exactly_the_stream_addressed_catalog` pins the partition
/// against the catalog, because a rename that moves the index out of reach
/// would silently *license* a field that should be refused.
pub fn stream_index(field: &str) -> Option<u8> {
    if !(field.starts_with("STREAM_") || field.starts_with("RTMP_")) {
        return None;
    }
    // The first `_`-delimited component that is a number. No upper bound, so a
    // stream 4 would be licensed like the other three rather than exempt.
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
/// [`DeviceState::check`]'s [`Unparsable`](Issue::Unparsable) and
/// [`Mismatch`](Issue::Mismatch) conditions, applied to a runtime write instead
/// of the config file. Without it one hand-crafted write could poison a cell
/// and turn every later read of that field into a mysterious timeout.
///
/// Refusing is also what the device does: a value it will not take is answered
/// with an error token this catalog does not model (see
/// [`Setting::instruction`]), which reaches the caller as the same command
/// timeout the simulator's silence produces.
///
/// A value from [`Setting::instruction`] always passes — a flag is already
/// `0`/`1`, a port already in range, free text already under its ceiling — so
/// what this catches is a request no client of ours built.
fn survives_the_read_path(field: &str, value: &str) -> bool {
    let Ok(query) = Query::from_str(field) else {
        // A write-only field (the nine RTMP push fields) has no read path to
        // disagree with, so there is nothing to check and the write stands.
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

/// The `RUNNING_STATE` wire code a command leaves behind. An explicit match, so
/// a new [`Command`] cannot compile without deciding what it does to the
/// recording state; `commands_mutate_the_running_state` pins these codes
/// against [`Query::RunningState`]'s parser over a real connection.
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
        // `ssh-port` normalised to `SSH_PORT`, the YAML integer to wire text.
        assert_eq!(state.get("SSH_PORT").as_deref(), Some("22023"));
    }

    #[test]
    fn booleans_become_flag_codes() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        assert_eq!(state.get("SNMP_STATE").as_deref(), Some("1"));
        assert_eq!(state.get("DHCP_MODE").as_deref(), Some("0"));
    }

    /// The document form renders the device's framing, not YAML: one JSON
    /// object per line, each line CR LF terminated, with the frame's final
    /// terminator left to the uniform `format!("{value}\r\n")` rendering.
    #[test]
    fn a_document_field_becomes_the_wire_frame() {
        let state = DeviceState::from_yaml_str(
            "\
username: admin
password: extron
fields:
  RECORDING_EVENTS:
    recording_events:
      - db_id: 7
        title: Lecture 7
",
        )
        .expect("loads");

        let body = state.get("RECORDING_EVENTS").expect("set");
        assert!(body.ends_with("\r\n"), "{body:?}");
        assert_eq!(body.lines().count(), 1);

        // And the device answers the full frame, which the client's own parser
        // reads back as one event.
        let reply = state
            .handle(Request::Get(Query::RecordingEvents))
            .expect("answered");
        assert!(reply.ends_with("\r\n\r\n"), "{reply:?}");
        assert!(matches!(
            Query::RecordingEvents.instruction().parse_step(&reply),
            Step::Done(Value::RecordingEvents(ref events)) if events.len() == 1
        ));
    }

    /// An empty document is a recorder that has never recorded, and must answer
    /// the bare terminator rather than going silent — silence is a timeout, and
    /// "no recordings" is an answer.
    #[test]
    fn an_empty_document_answers_the_bare_terminator() {
        let state = DeviceState::from_yaml_str(
            "username: admin\npassword: extron\nfields:\n  RECORDING_EVENTS:\n    recording_events: []\n",
        )
        .expect("loads");
        assert_eq!(state.get("RECORDING_EVENTS").as_deref(), Some(""));
        assert_eq!(
            state
                .handle(Request::Get(Query::RecordingEvents))
                .as_deref(),
            Some("\r\n")
        );
    }

    /// A mapping that is neither form names the key the file wrote. Serde's own
    /// untagged error ("data did not match any variant") would not, which is
    /// the reason the value is read key-first.
    #[test]
    fn an_unknown_document_key_names_itself() {
        let err = DeviceState::from_yaml_str(
            "username: admin\npassword: extron\nfields:\n  RECORDING_EVENTS:\n    recordings: []\n",
        )
        .expect_err("rejected");
        assert!(err.to_string().contains("recordings"), "{err}");
    }

    /// Both forms are the whole value, so giving both is a config that means
    /// two things at once rather than one to merge.
    #[test]
    fn a_document_cannot_be_inline_and_a_file_at_once() {
        let err = DeviceState::from_yaml_str(
            "\
username: admin
password: extron
fields:
  RECORDING_EVENTS:
    recording_events: []
    file: ./recordings.json
",
        )
        .expect_err("rejected");
        assert!(err.to_string().contains("file"), "{err}");
    }

    /// A file that cannot be read fails the *load*, naming the path, rather
    /// than leaving a cell empty and answering "no recordings" later.
    #[test]
    fn an_unreadable_document_file_fails_the_load() {
        let err = DeviceState::from_yaml_str(
            "username: admin\npassword: extron\nfields:\n  RECORDING_EVENTS:\n    file: ./no-such-recordings.json\n",
        )
        .expect_err("rejected");
        assert!(err.to_string().contains("no-such-recordings.json"), "{err}");
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
        // The configured fields must not be reported.
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
        // Both catalogs canonicalise to `TITLE`, so the write lands where the
        // read looks, with no pairing table involved.
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

    /// The settings half of
    /// `a_register_write_is_visible_to_the_matching_query`: `Setting` and
    /// `Query` spell `TIMEZONE` the same way, so one cell serves both.
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
    /// simulator answers with are fed to that setting's own production parser.
    /// The echo shape necessarily lives in two places — `setting_echo` parses
    /// it, `handle` renders it — and this is what holds them together.
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

    /// A value no `Setting::instruction` would produce, arriving anyway.
    /// Storing it would leave `DHCP_MODE` holding text the flag parser cannot
    /// read, so every later read of the field would time out.
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
    /// name must partition the catalog the way the catalog is shaped.
    ///
    /// A set comparison rather than spot checks, because both directions fail
    /// differently. A stream field that stops being recognised is silently
    /// *licensed* on a unit that should refuse it — the dangerous direction,
    /// invisible without this. One that starts being recognised gets refused by
    /// a `licensed_streams` list that never meant to name it.
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

    /// Each index claims only its own fields, so a one-stream license cannot
    /// refuse stream 1 by association.
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

    /// Ports and community strings carry digits of their own, and must never be
    /// read as streams.
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

    /// A one-stream unit refuses streams 2 and 3 and still answers stream 1 —
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
    /// merely a line that looks like one — which is what lets the simulator
    /// drive `auto_disable_after`, since the client counts refusals by running
    /// exactly this function over what arrives.
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
    /// refused rather than merely unanswered, and the cell stays empty.
    #[test]
    fn a_write_to_an_unlicensed_stream_is_refused_and_changes_nothing() {
        let state = DeviceState::from_yaml_str(BASE_UNIT).expect("loads");

        assert_eq!(
            state.handle(Request::Configure(Setting::Stream2Name, "Hall B".into())),
            Some("E13\r\n".into())
        );
        assert_eq!(state.get("STREAM_2_NAME"), None);
    }

    /// Refusal and silence are different replies — one feeds
    /// `auto_disable_after`, the other `exchange_timeout` — which is why the
    /// license is not modelled as an absent field.
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
    /// not have to invent values for fields it denies having.
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

    /// The by-name veto canonicalises like every other key, so an alias or a
    /// lowercase spelling vetoes the field it names.
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

    /// A veto naming no catalog field is reported rather than silently vetoing
    /// nothing — the treatment an unknown `fields:` key gets.
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
    /// what keeps a config written before licensing existed meaning what it
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
