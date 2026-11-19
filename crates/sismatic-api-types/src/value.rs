//! The wire value model: a serde mirror of `sismatic-core`'s decoded `Value`.
//!
//! `sismatic-core::protocol::Value` is the *internal* decoded reply. It is a
//! fine in-process type, but it is deliberately **not** reused here: doing so
//! would make `api-types` depend on `core`, and since every client depends on
//! `api-types`, that single edge would hand every frontend a compile path back
//! to the device library — the exact coupling the workspace layout forbids
//! (design note §2, "dependency direction as a partial order").
//!
//! So we re-declare the shape as plain serde DTOs. The translation
//! `core::Value -> ReadValue` lives wherever both crates already meet —
//! `sismatic-sync` (which writes reads) or `sismatic-db` — never here. This
//! crate stays a pure description of bytes on the wire.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Recording state reported by an SMP, mirroring
/// `sismatic-core::protocol::states::RecordingState`.
///
/// Serialized in `snake_case` (`"stopped"`, `"started"`, …) so the JSON reads
/// the same as the core `Display` impl and is trivial to match on in a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Stopped,
    Started,
    Paused,
    Unknown,
}

/// A hardware MAC address rendered in the SMP's canonical hyphenated hex form,
/// e.g. `"00-05-A6-1B-2C-3D"`.
///
/// Carried as a string rather than `[u8; 6]` because the wire is text and a
/// dashboard displays it verbatim; a client that needs the raw octets parses
/// this at its own boundary. `#[serde(transparent)]` makes it serialize as a
/// bare string, not `{ "0": [...] }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(as = "String"))]
#[cfg_attr(
    feature = "openapi",
    derive(utoipa::ToSchema),
    schema(value_type = String, example = "00-05-A6-1B-2C-3D")
)]
#[serde(transparent)]
pub struct MacAddr(pub String);

/// One active alarm as reported by `ACTIVE_ALARMS`.
///
/// `level` is left as free text (`"critical"`, `"warning"`, …) rather than an
/// enum: the device is the authority on the vocabulary, and pinning it to a
/// closed set here would turn an unrecognized-but-valid level into a
/// deserialization failure. Model it as an enum in a client if you want, at the
/// client's own risk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Alarm {
    pub name: String,
    pub level: String,
}

/// One recording event, as `RECORDING_EVENTS` reports it.
///
/// Mirrors `sismatic-core::protocol::RecordingEvent`, re-declared here for the
/// reason the module docs give: `api-types` must not depend on `core`.
///
/// # Optionality is load-bearing
///
/// The device's field set is not uniform across firmwares. Only `db_id` is
/// required; the rest is optional in one of two ways, chosen by what absence
/// would *mean*. `start`, `state` and `media_present` are nullable, because a
/// missing value is a different claim from `0` or `false`. The text fields
/// default to `""`, because the device already sends `""` for an unset one.
///
/// `state` is the device's raw code, left undecoded: only `5` and `11` have
/// been observed and the vocabulary is not documented — the same restraint
/// [`Alarm`]'s free-text `level` is given above.
///
/// `start` is epoch seconds, which is *not* the RFC 3339 form `Read.at` uses.
/// It is passed through as the device sends it; converting is the client's
/// decision, taken at the client's own boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RecordingEvent {
    pub db_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    #[serde(default)]
    pub identifier: String,
    #[serde(default)]
    pub creator: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_present: Option<bool>,
    #[serde(default)]
    pub package: String,
    /// Keys the device sent that this type does not name, preserved verbatim so
    /// a richer firmware loses nothing on the way through.
    ///
    /// Each value is the **raw JSON text** of that key — `"1200"` for a number,
    /// `"\"B\""` for a string, `"{\"a\":1}"` for an object. A client that wants
    /// one parses it at its own boundary, exactly as [`MacAddr`] above expects
    /// of a caller that wants the raw octets.
    ///
    /// Text rather than a JSON value type because this crate takes `serde` and
    /// nothing else (see the module docs): a `serde_json::Value` here would put
    /// `serde_json` in the dependency cone of every client of the API. Text
    /// costs the client a `JSON.parse` and loses nothing — JSON text round-trips.
    ///
    /// Nested under its own key rather than flattened back into the object,
    /// which is the one place this DTO's shape departs from the device's. That
    /// is deliberate: it puts the stability contract *in the type*. Everything
    /// above this field is attested against real hardware; everything inside it
    /// is whatever a firmware happened to add. Flattening the two together
    /// would hide exactly the distinction a client needs in order to know what
    /// it can rely on.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[cfg_attr(feature = "ts", ts(type = "Record<string, string>"))]
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub extra: BTreeMap<String, String>,
}

/// A decoded read value. The variant reflects what the field *means*, so a
/// client pattern-matches instead of re-parsing a string — the same benefit the
/// core `Value` enum gives in-process, preserved across the network boundary.
///
/// Adjacently tagged as `{ "type": <variant>, "value": <payload> }`, e.g.
/// `{"type":"port","value":22023}`, `{"type":"state","value":"started"}`,
/// `{"type":"alarms","value":[{"name":"video_loss","level":"critical"}]}`. The
/// adjacent form keeps every payload — including the primitives — in one stable
/// `value` slot, which is friendlier to a statically typed client than serde's
/// externally tagged default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ReadValue {
    /// Free-form text (names, metadata registers, SNMP strings, model info).
    Text(String),
    /// A firmware/version string such as `2.11`.
    Version(String),
    /// A network port.
    Port(u16),
    /// A numeric value that may exceed a port range (e.g. port timeouts).
    Number(u32),
    /// A boolean flag (DHCP mode, SNMP enabled).
    Flag(bool),
    /// A hardware MAC address.
    Mac(MacAddr),
    /// A command acknowledgement token echoed by the device.
    Ack(String),
    /// Decoded recording state.
    State(RecordingState),
    /// Active alarms.
    Alarms(Vec<Alarm>),
    /// Every recording event the device knows about.
    RecordingEvents(Vec<RecordingEvent>),
}
