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
//! The map is mutable at runtime: register writes overwrite a field and
//! recording commands rewrite `RUNNING_STATE`. The file is a starting point,
//! not a fixed answer table.

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
}

/// A way the config disagrees with the instruction catalog. Reported together
/// so `--check-config` can show every problem in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// A config key that is in neither [`Query`] nor [`Register`].
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
}

impl DeviceState {
    /// Parse a YAML config into a device.
    ///
    /// Keys are canonicalised through the catalog's own `FromStr`, so the file
    /// inherits its case- and `-`/`_`-insensitivity and its aliases for free:
    /// `ssh-port`, `SSH_PORT` and `Ssh_Port` all name the same field.
    /// Unrecognised keys are retained and reported by [`check`](Self::check).
    pub fn from_yaml_str(text: &str) -> Result<Self, LoadError> {
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

        Ok(DeviceState {
            credentials: raw.credentials,
            fields: Mutex::new(fields),
            unknown,
        })
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
    /// the device would stay silent (an unset field — which surfaces to the
    /// client as a command timeout, exactly like an unmodelled request).
    pub fn handle(&self, request: Request) -> Option<String> {
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
            if !fields.contains_key(&field) {
                issues.push(Issue::Missing { field });
            }
        }

        for &q in Query::ALL {
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

/// Every field the protocol can read or write: `Query::ALL ∪ Register::ALL`,
/// joined on the canonical name. `TITLE` appears in both catalogs and is one
/// field — which is what makes a register write visible to a later query
/// without any pairing table.
pub fn catalog_fields() -> BTreeSet<String> {
    Query::ALL
        .iter()
        .map(|q| q.name().to_string())
        .chain(Register::ALL.iter().map(|r| r.name().to_string()))
        .collect()
}

/// Resolve a config key to its canonical catalog name, trying both catalogs.
fn canonical_name(key: &str) -> Option<String> {
    Query::from_str(key)
        .map(|q| q.name().to_string())
        .or_else(|_| Register::from_str(key).map(|r| r.name().to_string()))
        .ok()
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

    #[test]
    fn an_unset_field_stays_silent() {
        let state = DeviceState::from_yaml_str(MINIMAL).expect("loads");
        assert_eq!(state.handle(Request::Get(Query::Firmware)), None);
    }
}
