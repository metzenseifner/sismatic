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

/// Where one recording event is in its schedule's lifecycle. Mirrors
/// `sismatic-core::protocol::ScheduleState`.
///
/// A listed code is serialized as the device's `SCHEDULE_*` name without the
/// prefix, in `snake_case`: `5` becomes `"archived"` and `200` becomes
/// `"package_resched"`. That matches how [`RecordingState`] reads, and a
/// browser can `switch` on it without a lookup table.
///
/// A code the device does not document (the list skips `7`, and a later
/// firmware may add more) is serialized as the **bare integer**, so a
/// client's type is `"pending" | … | number`. Falling back to a number keeps
/// the value: a client can still show it, and nothing on the wire changes
/// when the list grows. A single `"unknown"` string would drop the code, and
/// rejecting the row would drop the whole event list.
///
/// The OpenAPI schema is written by hand (below) because utoipa does not
/// support a variant-level `untagged`: it would document the fallback as
/// `{"unrecognized": 7}`, which is not what serde sends. ts-rs supports it, so
/// the TypeScript type is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ScheduleState {
    /// `0` — Recording Scheduled.
    Pending,
    /// `1` — Recording Now.
    Current,
    /// `2` — Recording Finalized.
    Processed,
    /// `3` — Ready to Upload.
    Finished,
    /// `4` — Uploading to Server.
    Archiving,
    /// `5` — Uploading to Server Complete.
    Archived,
    /// `6` — Uploading to Server Failed (retry pending).
    ArchiveFailed,
    /// `8` — Recording Deleted (to recover disk space).
    PackageDeleted,
    /// `9` — Recording Failed (package corrupted).
    RecordFailed,
    /// `10` — No Transfer Method Defined.
    ArchiveNomethod,
    /// `11` — Transfer Skipped.
    RecordSkipped,
    /// `200` — Transfer Rescheduled (one time).
    PackageResched,
    /// A code not listed above, as the device sent it.
    #[serde(untagged)]
    Unrecognized(u8),
}

impl ScheduleState {
    /// Every listed state with its device code, in code order. The wire
    /// names are written out once here and nowhere else.
    pub const KNOWN: [(ScheduleState, u8, &'static str); 12] = [
        (ScheduleState::Pending, 0, "pending"),
        (ScheduleState::Current, 1, "current"),
        (ScheduleState::Processed, 2, "processed"),
        (ScheduleState::Finished, 3, "finished"),
        (ScheduleState::Archiving, 4, "archiving"),
        (ScheduleState::Archived, 5, "archived"),
        (ScheduleState::ArchiveFailed, 6, "archive_failed"),
        (ScheduleState::PackageDeleted, 8, "package_deleted"),
        (ScheduleState::RecordFailed, 9, "record_failed"),
        (ScheduleState::ArchiveNomethod, 10, "archive_nomethod"),
        (ScheduleState::RecordSkipped, 11, "record_skipped"),
        (ScheduleState::PackageResched, 200, "package_resched"),
    ];

    /// The device's integer code.
    pub fn code(self) -> u8 {
        match self {
            ScheduleState::Unrecognized(code) => code,
            known => Self::KNOWN
                .iter()
                .find(|(state, _, _)| *state == known)
                .map(|(_, code, _)| *code)
                .expect("every listed variant is in KNOWN"),
        }
    }

    /// The device's integer code mapped to a variant. Total: an unlisted
    /// code is [`Unrecognized`](Self::Unrecognized).
    pub fn from_code(code: u8) -> Self {
        Self::KNOWN
            .iter()
            .find(|(_, c, _)| *c == code)
            .map_or(ScheduleState::Unrecognized(code), |(state, _, _)| *state)
    }

    /// Read a state as a caller types it in a filter. Three spellings are
    /// accepted, all case-insensitive: the wire name (`archive_failed`), the
    /// device's name (`SCHEDULE_ARCHIVE_FAILED`), and the integer code (`6`).
    /// The code is the only way to name an unrecognized state.
    ///
    /// `None` means the text names no state. That is not an error here,
    /// because a misspelled state and a state no event is in both select no
    /// device.
    pub fn from_text(text: &str) -> Option<Self> {
        let text = text.trim();
        if let Ok(code) = text.parse::<u8>() {
            return Some(Self::from_code(code));
        }
        let lower = text.to_ascii_lowercase();
        let name = lower.strip_prefix("schedule_").unwrap_or(&lower);
        Self::KNOWN
            .iter()
            .find(|(_, _, wire)| *wire == name)
            .map(|(state, _, _)| *state)
    }
}

#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for ScheduleState {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{
            KnownFormat, ObjectBuilder, OneOfBuilder, SchemaFormat, Type,
        };
        let names = ObjectBuilder::new()
            .schema_type(Type::String)
            .enum_values(Some(Self::KNOWN.iter().map(|(_, _, name)| *name)))
            .description(Some(
                "A listed state: the device's `SCHEDULE_*` name without the prefix, in \
                 `snake_case`. Codes: pending 0, current 1, processed 2, finished 3, \
                 archiving 4, archived 5, archive_failed 6, package_deleted 8, \
                 record_failed 9, archive_nomethod 10, record_skipped 11, package_resched 200.",
            ));
        let code = ObjectBuilder::new()
            .schema_type(Type::Integer)
            .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int32)))
            .minimum(Some(0))
            .maximum(Some(255))
            .description(Some("A code the device does not document, as it sent it."));
        OneOfBuilder::new()
            .item(names)
            .item(code)
            .description(Some(
                "Where one recording event is in its schedule's lifecycle: a state name, or \
                 the raw integer code for a state the device does not document.",
            ))
            .into()
    }
}

#[cfg(feature = "openapi")]
impl utoipa::ToSchema for ScheduleState {}

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
/// `state` is decoded into a [`ScheduleState`]; see that type for how a code
/// the device does not document is carried.
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
    pub state: Option<ScheduleState>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listed_state_is_its_name_and_an_unlisted_one_its_code() {
        for (state, _, name) in ScheduleState::KNOWN {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(name)
            );
            assert_eq!(
                serde_json::from_value::<ScheduleState>(name.into()).unwrap(),
                state
            );
        }
        assert_eq!(
            serde_json::to_string(&ScheduleState::Unrecognized(7)).unwrap(),
            "7"
        );
        assert_eq!(
            serde_json::from_str::<ScheduleState>("7").unwrap(),
            ScheduleState::Unrecognized(7)
        );
    }

    #[test]
    fn an_event_carries_the_decoded_state() {
        let event: RecordingEvent =
            serde_json::from_str(r#"{"db_id":58,"state":"record_skipped"}"#).unwrap();
        assert_eq!(event.state, Some(ScheduleState::RecordSkipped));
    }

    #[test]
    fn a_state_is_read_from_any_of_its_three_spellings() {
        for text in [
            "archive_failed",
            "ARCHIVE_FAILED",
            "SCHEDULE_ARCHIVE_FAILED",
            "6",
            " 6 ",
        ] {
            assert_eq!(
                ScheduleState::from_text(text),
                Some(ScheduleState::ArchiveFailed),
                "{text}"
            );
        }
        assert_eq!(
            ScheduleState::from_text("7"),
            Some(ScheduleState::Unrecognized(7))
        );
        assert_eq!(ScheduleState::from_text("SCHEDULE_7"), None);
        assert_eq!(ScheduleState::from_text("archive-failed"), None);
        assert_eq!(ScheduleState::from_text("schedule_"), None);
    }

    #[test]
    fn codes_round_trip() {
        for code in 0..=u8::MAX {
            assert_eq!(ScheduleState::from_code(code).code(), code);
        }
    }
}
