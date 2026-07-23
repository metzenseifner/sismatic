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
//! `core::Value -> ReadingValue` lives wherever both crates already meet —
//! `sismatic-sync` (which writes readings) or `sismatic-db` — never here. This
//! crate stays a pure description of bytes on the wire.

use serde::{Deserialize, Serialize};

/// Recording state reported by an SMP, mirroring
/// `sismatic-core::protocol::states::RecordingState`.
///
/// Serialized in `snake_case` (`"stopped"`, `"started"`, …) so the JSON reads
/// the same as the core `Display` impl and is trivial to match on in a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
pub struct Alarm {
    pub name: String,
    pub level: String,
}

/// A decoded reading value. The variant reflects what the field *means*, so a
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
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ReadingValue {
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
}
