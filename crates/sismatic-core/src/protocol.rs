//! The SIS protocol, modelled with typed instructions and winnow parsers.
//!
//! Each instruction is one exchange: a `payload` to send plus a streaming
//! parser that turns the device's reply into a typed [`Value`]. The parser is
//! incremental — it reports [`Step::NeedMore`] until a complete, well-formed
//! response is present — so the transport can feed it bytes as they arrive and
//! stop as soon as the message is complete.
//!
//! The built-in catalog is expressed as the [`Query`](instructions::query::Query),
//! [`Register`](instructions::register::Register), and
//! [`Command`](instructions::commands::Command) enums. The protocol stays open:
//! build an [`Instruction`](instructions::Instruction) with
//! [`Instruction::custom`](instructions::Instruction::custom) (supplying your own
//! parser) to add instructions the catalog does not cover.
//!
//! One reply shape is handled outside that scheme: a device may refuse a verb
//! with an error token instead of answering it, and no instruction's parser
//! recognises one. See [`SisError`].

// `pub(crate)`, not private: the simulator is a sibling module and builds its
// replies from these same control characters and helpers rather than
// re-spelling them. Still not part of the public API.
pub(crate) mod control_chars;
pub mod instructions;
pub(crate) mod payload_helpers;
mod sis_error;
mod states;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use winnow::error::ErrMode;
use winnow::{ModalResult, Partial};

// Re-exported so consumers (e.g. the DTO conversion in `sismatic-sync`) can name
// the type for an *exhaustive* match — the private `states` module keeps its
// other items internal.
pub use crate::protocol::states::RecordingState;

// Re-exported because it travels out of the crate inside a `ControllerError`,
// so a caller matching on why a command failed has to be able to name it.
pub use crate::protocol::sis_error::SisError;

/// A decoded response value. The variant reflects what the field means, so a
/// caller can pattern-match instead of re-parsing a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
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
    /// Decoded recording state.
    Mac(MacAddr),
    /// A command acknowledgement token echoed by the device.
    Ack(String),
    /// The state of the recording.
    State(RecordingState),
    /// Active alarms as `(name, level)` pairs.
    Alarms(Vec<(String, String)>),
    /// Every recording event the device knows about.
    RecordingEvents(Vec<RecordingEvent>),
}

/// One recording event, as `RECORDING_EVENTS` reports it.
///
/// # Which fields are required
///
/// Only `db_id` is. The device's field set is not uniform across firmwares, so
/// everything else is optional in one of two ways, chosen by what absence would
/// *mean*:
///
/// - `start`, `state` and `media_present` are [`Option`], because a missing
///   value is not the same claim as `0` or `false` and defaulting them would
///   invent a fact the device did not state.
/// - `identifier`, `creator`, `title` and `package` default to `""`, because
///   the device already sends `""` for an unset one — empty string *is* its
///   encoding of absent, so a default agrees with the wire rather than guessing.
///
/// `db_id` stays required because it is the row's identity; a record without
/// one is not a record this type can usefully carry.
///
/// # `state`
///
/// Kept as the raw code. Only `5` and `11` have been observed and the
/// vocabulary is not documented, so there is nothing here to decode it into —
/// the same restraint `Alarm`'s free-text `level` is given in `api-types`.
///
/// # `extra`
///
/// Every key not named above, so a firmware that reports more than this one
/// does neither loses the extra data nor fails to parse.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordingEvent {
    /// The recorder database's primary key for this event.
    pub db_id: u64,
    /// Start time in epoch seconds. Note this is *not* the RFC 3339 form the
    /// rest of the API uses for timestamps; it is passed on as the device sends
    /// it, and converting is the caller's decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    #[serde(default)]
    pub identifier: String,
    #[serde(default)]
    pub creator: String,
    #[serde(default)]
    pub title: String,
    /// The device's raw state code; see the type docs on why it is not decoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<u8>,
    /// Whether the recording's media is still present.
    ///
    /// The device sends this as the *string* `"true"`, not a JSON boolean, so
    /// the deserializer accepts both — a firmware that corrects the encoding
    /// must not break the parse.
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_bool",
        skip_serializing_if = "Option::is_none"
    )]
    pub media_present: Option<bool>,
    #[serde(default)]
    pub package: String,
    /// Keys this type does not name, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Every recording event a device knows about, as one JSON document.
///
/// The reply itself is a *list*: one JSON object per line. A list is not a
/// value a config file cell or a `NAME=VALUE` flag can hold, so this folds it
/// into a single object under one key, which both of those can:
///
/// ```json
/// {"recording_events": [{"db_id": 58, "title": "Lecture 1"}]}
/// ```
///
/// It carries the same [`RecordingEvent`] the parser produces, so a document
/// cannot describe an event a client could not have read, and
/// [`wire_body`](Self::wire_body) renders it back to the device's own framing.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingEvents {
    /// The events, in the order the device would list them.
    #[serde(default)]
    pub recording_events: Vec<RecordingEvent>,
}

impl From<Vec<RecordingEvent>> for RecordingEvents {
    fn from(recording_events: Vec<RecordingEvent>) -> Self {
        RecordingEvents { recording_events }
    }
}

impl RecordingEvents {
    /// The reply body: each event as one JSON object, each terminated by
    /// `CR LF` — the `RECORDING_EVENTS` frame *minus* its final `CR LF`.
    ///
    /// The terminator is left to the caller because that makes the body the
    /// same shape as every other query's: a simulated device renders any field
    /// as `format!("{value}\r\n")`, so an empty list is the empty string, which
    /// that rendering turns into the bare terminator a device with no
    /// recordings sends, and `n` events become `n` lines plus the terminator.
    ///
    /// The framing lives here rather than at the call site for the reason the
    /// rest of this module gives: the layer that parses a frame is the layer
    /// that publishes it, so the two cannot drift apart silently.
    pub fn wire_body(&self) -> String {
        let mut out = String::new();
        for event in &self.recording_events {
            // Infallible: every key is a `String` and every value is already
            // `serde_json`'s own, so there is no non-string map key and no
            // custom `Serialize` to fail.
            let line = serde_json::to_string(event)
                .expect("a RecordingEvent serializes: its keys are all strings");
            out.push_str(&line);
            out.push_str("\r\n");
        }
        out
    }
}

/// Accept a boolean written either as a JSON boolean or as a string.
///
/// The accepted spellings are the ones `Setting`'s encoder folds onto `1`/`0`,
/// so a value this reads and a value that writes agree with each other.
fn deserialize_lenient_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Bool(bool),
        Text(String),
    }

    match <Option<Raw> as serde::Deserialize>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Raw::Bool(b)) => Ok(Some(b)),
        Some(Raw::Text(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "on" | "yes" => Ok(Some(true)),
            "0" | "false" | "off" | "no" => Ok(Some(false)),
            other => Err(serde::de::Error::custom(format!(
                "not a boolean: {other:?}"
            ))),
        },
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Text(s) | Value::Version(s) | Value::Ack(s) => f.write_str(s),
            Value::Port(p) => write!(f, "{p}"),
            Value::Number(n) => write!(f, "{n}"),
            Value::Flag(b) => f.write_str(if *b { "1" } else { "0" }),
            Value::State(s) => write!(f, "{s}"),
            Value::Mac(m) => write!(f, "{m}"),
            Value::Alarms(a) => {
                let joined = a
                    .iter()
                    .map(|(n, l)| format!("{n}:{l}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                f.write_str(&joined)
            }
            // One JSON object per line — the device's own framing minus the
            // terminator. There is no shorter form that does not throw data
            // away, and this is what a CLI printing a `Value` should show.
            Value::RecordingEvents(events) => {
                for event in events {
                    let line = serde_json::to_string(event).map_err(|_| fmt::Error)?;
                    writeln!(f, "{line}")?;
                }
                Ok(())
            }
        }
    }
}

impl Value {
    /// The decoded running state, if this value carries one.
    pub fn as_state(&self) -> Option<RecordingState> {
        match self {
            Value::State(s) => Some(*s),
            _ => None,
        }
    }

    /// The port number, if this value is a port.
    pub fn as_port(&self) -> Option<u16> {
        match self {
            Value::Port(p) => Some(*p),
            _ => None,
        }
    }

    /// The MAC address, if this value is one.
    pub fn as_mac(&self) -> Option<MacAddr> {
        match self {
            Value::Mac(m) => Some(*m),
            _ => None,
        }
    }
}
///
/// A hardware MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacAddr(pub [u8; 6]);

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02X}-{:02X}-{:02X}-{:02X}-{:02X}-{:02X}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

// ---- Streaming parse step -------------------------------------------------

/// The result of feeding the accumulated response buffer to a parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step<T> {
    /// The buffer does not yet contain a complete response; read more bytes.
    NeedMore,
    /// A complete response was parsed.
    Done(T),
}

/// The parser stored inside an [`Instruction`]: given the accumulated buffer it
/// returns whether a complete [`Value`] is present.
type ParseFn = Arc<dyn Fn(&str) -> Step<Value> + Send + Sync>;

// ---- parser plumbing ------------------------------------------------------

type In<'a> = Partial<&'a str>;

/// Search the accumulated buffer for a position where `core` parses a complete
/// response. Mirrors a regex `find`: the device often echoes the request before
/// the framed reply, so we try every offset. If nothing matches yet we ask for
/// more bytes (the transport stops on its read timeout).
fn search<O>(buf: &str, core: &(impl Fn(&mut In) -> ModalResult<O> + ?Sized)) -> Step<O> {
    for (i, _) in buf.char_indices() {
        let mut input = Partial::new(&buf[i..]);
        match core(&mut input) {
            Ok(value) => return Step::Done(value),
            // A committed (`Cut`) error means this offset matched the reply's
            // shape but the value itself was invalid (e.g. digits that overflow
            // the target integer). Sliding to a later offset would only match a
            // shorter, truncated token and report a wrong value, so stop here
            // and ask for more bytes instead. `Backtrack`/`Incomplete` keep
            // scanning — that is how a leading request echo gets skipped.
            Err(ErrMode::Cut(_)) => return Step::NeedMore,
            Err(_) => {}
        }
    }
    Step::NeedMore
}

/// Wrap a *whole-buffer* parser into the `ParseFn` an [`Instruction`] stores,
/// deliberately bypassing [`search`].
///
/// # Why a reply would refuse `parser_of`
///
/// [`search`] slides to every offset and returns the first one that parses,
/// which is what lets a leading request echo be skipped. That is sound only
/// while the reply's grammar cannot match the *empty* input — otherwise some
/// offset inside the reply matches a zero-length response and the search
/// reports it, having read almost nothing.
///
/// `RECORDING_EVENTS` is exactly that shape: its frame is `item* CRLF`, so an
/// empty list is a bare CRLF, and a bare CRLF is also what sits at the end of
/// every single item. Under `search`, a list whose first item had arrived but
/// whose terminator had not would match "zero items, then the terminator" at
/// that item's own CRLF and report an *empty* list for a device holding a
/// hundred recordings. Since the transport reads in 1 KiB chunks, every read
/// of a long list passes through precisely that state.
///
/// A parser reached through this function sees the accumulated buffer from byte
/// zero and decides for itself, so it can require the terminator before
/// committing. The cost is that it must handle any leading echo itself.
fn anchored(parse: impl Fn(&str) -> Step<Value> + Send + Sync + 'static) -> ParseFn {
    Arc::new(parse)
}

/// Wrap a typed winnow parser into the `ParseFn` an [`Instruction`] stores.
fn parser_of<O: 'static>(
    core: impl Fn(&mut In) -> ModalResult<O> + Send + Sync + 'static,
    wrap: impl Fn(O) -> Value + Send + Sync + 'static,
) -> ParseFn {
    Arc::new(move |buf: &str| match search(buf, &core) {
        Step::Done(o) => Step::Done(wrap(o)),
        Step::NeedMore => Step::NeedMore,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::protocol::instructions::{
        Instruction,
        commands::Command,
        query::Query,
        register::{MAX_VALUE_LEN, Register},
        setting::Setting,
    };

    use super::*;

    /// Drive a parser the way the transport does: one byte at a time.
    fn drive(instr: &Instruction, response: &str) -> Step<Value> {
        let mut buf = String::new();
        for c in response.chars() {
            buf.push(c);
            if let Step::Done(v) = instr.parse_step(&buf) {
                return Step::Done(v);
            }
        }
        Step::NeedMore
    }

    /// Pull the events out of a reply, failing the test on any other shape.
    fn events_of(step: Step<Value>) -> Vec<RecordingEvent> {
        match step {
            Step::Done(Value::RecordingEvents(events)) => events,
            other => panic!("expected a recording-event list, got {other:?}"),
        }
    }

    const EVENT_A: &str = r#"{"db_id":58,"start":1767879511,"identifier":"A_1","creator":"c900011","title":"Mechanics I","state":11,"media_present":"true","package":"/var/internal/A_1"}"#;
    const EVENT_B: &str = r#"{"db_id":59,"start":1767883456,"identifier":"A_2","creator":"","title":"","state":5,"media_present":"true","package":"/var/internal/A_2"}"#;

    #[test]
    fn an_empty_recording_list_is_a_bare_terminator() {
        let instr = Query::RecordingEvents.instruction();
        assert_eq!(events_of(drive(&instr, "\r\n")), vec![]);
    }

    #[test]
    fn parses_one_recording_event() {
        let instr = Query::RecordingEvents.instruction();
        let events = events_of(drive(&instr, &format!("{EVENT_A}\r\n\r\n")));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].db_id, 58);
        assert_eq!(events[0].state, Some(11));
        assert_eq!(events[0].start, Some(1767879511));
        assert_eq!(events[0].title, "Mechanics I");
        // The device sends this as the string `"true"`, not a JSON boolean.
        assert_eq!(events[0].media_present, Some(true));
    }

    /// The regression test for the whole reason this parser bypasses `search`.
    ///
    /// `drive` feeds one character at a time, so the parser is asked to decide
    /// on every prefix of the reply — including the prefix that ends just after
    /// the first event's own CRLF. Under `parser_of` that prefix matches "zero
    /// items, then the terminator" and the exchange completes with an **empty**
    /// list while two events are on the wire. Anchored, it has to wait.
    #[test]
    fn a_partially_arrived_list_never_completes_early() {
        let instr = Query::RecordingEvents.instruction();
        let events = events_of(drive(&instr, &format!("{EVENT_A}\r\n{EVENT_B}\r\n\r\n")));
        assert_eq!(
            events.iter().map(|e| e.db_id).collect::<Vec<_>>(),
            vec![58, 59],
            "the list completed before every event had arrived"
        );
    }

    #[test]
    fn a_list_without_its_terminator_asks_for_more() {
        let instr = Query::RecordingEvents.instruction();
        // Every prefix up to and including the last event's own CRLF.
        assert_eq!(
            drive(&instr, &format!("{EVENT_A}\r\n{EVENT_B}\r\n")),
            Step::NeedMore
        );
    }

    /// A CR LF inside a string value is JSON-escaped as the two characters
    /// `\r`, so it cannot be mistaken for the frame's terminator.
    #[test]
    fn an_escaped_crlf_inside_a_title_does_not_split_the_list() {
        let instr = Query::RecordingEvents.instruction();
        let line = r#"{"db_id":7,"title":"two\r\n\r\nlines"}"#;
        let events = events_of(drive(&instr, &format!("{line}\r\n\r\n")));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].title, "two\r\n\r\nlines");
    }

    /// The firmware-variation contract: absent optional fields stay absent,
    /// absent text fields read as the empty string the device would have sent.
    #[test]
    fn a_sparse_event_keeps_absence_distinct_from_zero() {
        let instr = Query::RecordingEvents.instruction();
        let events = events_of(drive(&instr, "{\"db_id\":42}\r\n\r\n"));
        let event = &events[0];
        assert_eq!(event.db_id, 42);
        assert_eq!(event.start, None);
        assert_eq!(event.state, None);
        assert_eq!(event.media_present, None);
        assert_eq!(event.title, "");
        assert_eq!(event.package, "");
        assert!(event.extra.is_empty());
    }

    /// A firmware that reports more than this one must neither lose the extra
    /// keys nor fail to parse.
    #[test]
    fn unknown_keys_are_preserved_rather_than_dropped() {
        let instr = Query::RecordingEvents.instruction();
        let line = r#"{"db_id":9,"duration":1200,"channel":"B"}"#;
        let events = events_of(drive(&instr, &format!("{line}\r\n\r\n")));
        assert_eq!(events[0].extra["duration"], serde_json::json!(1200));
        assert_eq!(events[0].extra["channel"], serde_json::json!("B"));
    }

    /// Both encodings of the flag, so a firmware that corrects the stringly
    /// boolean does not break the parse.
    #[test]
    fn media_present_accepts_a_string_or_a_boolean() {
        let instr = Query::RecordingEvents.instruction();
        for (written, expected) in [
            ("\"true\"", Some(true)),
            ("\"false\"", Some(false)),
            ("true", Some(true)),
            ("false", Some(false)),
        ] {
            let line = format!(r#"{{"db_id":1,"media_present":{written}}}"#);
            let events = events_of(drive(&instr, &format!("{line}\r\n\r\n")));
            assert_eq!(events[0].media_present, expected, "for {written}");
        }
    }

    /// A malformed line asks for more bytes rather than reporting a short list:
    /// a wrong recording count is indistinguishable from a true one downstream.
    #[test]
    fn a_malformed_line_never_yields_a_shorter_list() {
        let instr = Query::RecordingEvents.instruction();
        assert_eq!(
            drive(&instr, &format!("{EVENT_A}\r\nnot json\r\n\r\n")),
            Step::NeedMore
        );
    }

    /// `Display` round-trips through the device's own framing, minus the
    /// terminator — the property that makes it safe for the CLI to print.
    #[test]
    fn display_renders_one_json_object_per_line() {
        let instr = Query::RecordingEvents.instruction();
        let Step::Done(value) = drive(&instr, &format!("{EVENT_A}\r\n{EVENT_B}\r\n\r\n")) else {
            panic!("expected a complete list");
        };
        let rendered = value.to_string();
        assert_eq!(rendered.lines().count(), 2);
        for line in rendered.lines() {
            serde_json::from_str::<RecordingEvent>(line).expect("each line re-parses");
        }
    }

    /// An excerpt of a real `LRCDR` reply, framed as the device frames it,
    /// with identifiers, staff ids and titles replaced: the shape is the
    /// device's, the data is not. Guards the shape against a "simplification" that happens to satisfy
    /// the synthetic cases above: mixed empty and populated metadata, the
    /// stringly boolean, and both observed state codes, all in one reply.
    #[test]
    fn parses_a_captured_device_reply() {
        const CAPTURED: &str = concat!(
            r#"{"db_id":58,"start":1767879511,"identifier":"Sim-Studio-A_20260108-143831","creator":"c900011","title":"2026-01-08 - Mechanics I","state":11,"media_present":"true","package":"/var/internal/Sim-Studio-A_20260108-143831"}"#,
            "\r\n",
            r#"{"db_id":67,"start":1774626149,"identifier":"Sim-Studio-A_20260327-164229","creator":"","title":"","state":11,"media_present":"true","package":"/var/internal/Sim-Studio-A_20260327-164229"}"#,
            "\r\n",
            r#"{"db_id":163,"start":1792494612,"identifier":"Sim-Studio-A_20261020-131012","creator":"c900022","title":"2026-10-20 - Optics II","state":5,"media_present":"true","package":"/var/internal/Sim-Studio-A_20261020-131012"}"#,
            "\r\n",
            "\r\n",
        );

        let instr = Query::RecordingEvents.instruction();
        let events = events_of(drive(&instr, CAPTURED));

        assert_eq!(
            events.iter().map(|e| e.db_id).collect::<Vec<_>>(),
            vec![58, 67, 163]
        );
        assert_eq!(
            events.iter().map(|e| e.state).collect::<Vec<_>>(),
            vec![Some(11), Some(11), Some(5)]
        );
        // Every row in the capture carries the flag as the string `"true"`.
        assert!(events.iter().all(|e| e.media_present == Some(true)));
        // Empty metadata is the device's own encoding of "unset", so it reads
        // back as the empty string rather than becoming an absence.
        assert_eq!(events[1].creator, "");
        assert_eq!(events[1].title, "");
        assert_eq!(events[2].creator, "c900022");
        // Nothing in this firmware's field set lands outside the named fields.
        assert!(events.iter().all(|e| e.extra.is_empty()));
    }

    /// The drift net for the document form: the body a config renders is fed
    /// to the *production* parser, framed the way a simulated device frames
    /// every field. A change to either side fails here rather than later, as a
    /// simulated recorder that answers bytes no client can read.
    #[test]
    fn a_document_renders_a_body_the_parser_reads_back() {
        let document = RecordingEvents {
            recording_events: events_of(drive(
                &Query::RecordingEvents.instruction(),
                &format!("{EVENT_A}\r\n{EVENT_B}\r\n\r\n"),
            )),
        };

        let instr = Query::RecordingEvents.instruction();
        let rendered = format!("{}\r\n", document.wire_body());
        assert_eq!(
            events_of(drive(&instr, &rendered)),
            document.recording_events
        );
    }

    /// The empty document is the empty body, which the uniform
    /// `format!("{value}\r\n")` rendering turns into the bare terminator a
    /// device with no recordings sends. Separate from the test above because
    /// this is the case the frame is ambiguous about, and the one a `Vec`
    /// joined on `\r\n` would get wrong.
    #[test]
    fn an_empty_document_renders_the_bare_terminator() {
        let document = RecordingEvents::default();
        assert_eq!(document.wire_body(), "");

        let rendered = format!("{}\r\n", document.wire_body());
        assert_eq!(rendered, "\r\n");
        assert_eq!(
            events_of(drive(&Query::RecordingEvents.instruction(), &rendered)),
            vec![]
        );
    }

    /// A single event is the other end of that ambiguity: one line plus the
    /// terminator is `\r\n\r\n` only because the body carries its own line
    /// ending.
    #[test]
    fn a_one_event_document_still_carries_the_terminator() {
        let document = RecordingEvents::from(events_of(drive(
            &Query::RecordingEvents.instruction(),
            &format!("{EVENT_A}\r\n\r\n"),
        )));
        let rendered = format!("{}\r\n", document.wire_body());
        assert!(rendered.ends_with("\r\n\r\n"), "{rendered:?}");
        assert_eq!(
            events_of(drive(&Query::RecordingEvents.instruction(), &rendered)).len(),
            1
        );
    }

    /// The document is also the file format, so it must survive JSON — and an
    /// unattested key must survive it too, or a config could not describe the
    /// firmware that sends one.
    #[test]
    fn a_document_round_trips_through_json_with_unattested_keys() {
        let text = r#"{"recording_events":[{"db_id":7,"title":"Lecture 7","chapters":3}]}"#;
        let document: RecordingEvents = serde_json::from_str(text).expect("parses");
        assert_eq!(document.recording_events[0].db_id, 7);
        assert_eq!(
            document.recording_events[0].extra.get("chapters"),
            Some(&serde_json::json!(3))
        );

        let again: RecordingEvents =
            serde_json::from_str(&serde_json::to_string(&document).expect("serializes"))
                .expect("re-parses");
        assert_eq!(again, document);
    }

    /// A misspelled key at the document's own level is an error, not an event
    /// list that silently came back empty. `RecordingEvent` is lenient about
    /// unknown keys because firmwares differ; the wrapper is not, because its
    /// one key is ours.
    #[test]
    fn a_misspelled_document_key_is_rejected() {
        assert!(serde_json::from_str::<RecordingEvents>(r#"{"recordings":[]}"#).is_err());
    }

    #[test]
    fn parses_running_state() {
        let instr = Query::RunningState.instruction();
        // Includes a leading echo to prove the search skips it.
        let resp = "1\r\n";
        assert_eq!(
            drive(&instr, resp),
            Step::Done(Value::State(RecordingState::Started))
        );
    }

    #[test]
    fn parses_stream_1_name() {
        let instr = Query::Stream1Name.instruction();
        assert_eq!(instr.payload, "\u{1b}N1STRC\r");
        assert_eq!(
            drive(&instr, "Lecture Hall A\r\n"),
            Step::Done(Value::Text("Lecture Hall A".into()))
        );
    }

    #[test]
    fn parses_stream_2_name() {
        let instr = Query::Stream2Name.instruction();
        assert_eq!(instr.payload, "\u{1b}N2STRC\r");
        assert_eq!(
            drive(&instr, "Lecture Hall B\r\n"),
            Step::Done(Value::Text("Lecture Hall B".into()))
        );
    }

    #[test]
    fn parses_stream_3_name() {
        let instr = Query::Stream3Name.instruction();
        assert_eq!(instr.payload, "\u{1b}N3STRC\r");
        assert_eq!(
            drive(&instr, "Lecture Hall C\r\n"),
            Step::Done(Value::Text("Lecture Hall C".into()))
        );
    }

    /// The enable-state read is the name read's verb without the `N`, so the
    /// two payloads are pinned together — a typo in either would silently make
    /// one of them the other.
    #[test]
    fn parses_stream_state_as_a_flag() {
        let instr = Query::Stream2State.instruction();
        assert_eq!(instr.payload, "\u{1b}2STRC\r");
        assert_eq!(drive(&instr, "1\r\n"), Step::Done(Value::Flag(true)));
        assert_eq!(
            drive(&Query::Stream3State.instruction(), "0\r\n"),
            Step::Done(Value::Flag(false))
        );
    }

    /// `ESC S<i>*<n>RTMP CR` -> `(0|1) CR LF`. A bare flag, exactly as the
    /// `STRC` reads above — the address is *not* echoed back, which is the
    /// whole correction this test exists to pin.
    ///
    /// The payload assertions matter as much as the parse ones: `i` and `n` are
    /// two indices in one address, and transposing them is a mistake that reads
    /// a real value off the wrong target rather than failing.
    #[test]
    fn parses_rtmp_live_state_as_a_bare_flag() {
        let instr = Query::RtmpStream2LiveState.instruction();
        assert_eq!(instr.payload, "\u{1b}S1*2RTMP\r");
        assert_eq!(drive(&instr, "1\r\n"), Step::Done(Value::Flag(true)));

        let backup = Query::RtmpBackupStream2LiveState.instruction();
        assert_eq!(backup.payload, "\u{1b}S2*2RTMP\r");
        assert_eq!(drive(&backup, "0\r\n"), Step::Done(Value::Flag(false)));
    }

    /// Every one of the six, so a wrong index in the table cannot hide behind a
    /// sibling that happens to be spelled right.
    #[test]
    fn every_rtmp_live_state_addresses_its_own_target() {
        let addressed = [
            (Query::RtmpStream1LiveState, "\u{1b}S1*1RTMP\r"),
            (Query::RtmpStream2LiveState, "\u{1b}S1*2RTMP\r"),
            (Query::RtmpStream3LiveState, "\u{1b}S1*3RTMP\r"),
            (Query::RtmpBackupStream1LiveState, "\u{1b}S2*1RTMP\r"),
            (Query::RtmpBackupStream2LiveState, "\u{1b}S2*2RTMP\r"),
            (Query::RtmpBackupStream3LiveState, "\u{1b}S2*3RTMP\r"),
        ];
        for (query, payload) in addressed {
            let instr = query.instruction();
            assert_eq!(instr.payload, payload, "payload for {}", instr.name);
            assert_eq!(
                drive(&instr, "1\r\n"),
                Step::Done(Value::Flag(true)),
                "reply shape for {}",
                instr.name
            );
        }
    }

    /// A firmware that answers the live-state read with the address echoed back
    /// — the padded form the enable *write* really does use — still decodes to
    /// the flag at the end of the line, so the bare-flag parser is not merely
    /// correct for the attested reply but safe against the other shape.
    #[test]
    fn a_live_state_read_survives_an_echoed_address() {
        let instr = Query::RtmpStream1LiveState.instruction();
        for reply in ["1\r\n", "RtmpS1*1*1\r\n", "RtmpS01*01*1\r\n"] {
            assert_eq!(
                drive(&instr, reply),
                Step::Done(Value::Flag(true)),
                "{reply:?}"
            );
        }
        let off = Query::RtmpBackupStream3LiveState.instruction();
        for reply in ["0\r\n", "RtmpS02*03*0\r\n"] {
            assert_eq!(
                drive(&off, reply),
                Step::Done(Value::Flag(false)),
                "{reply:?}"
            );
        }
    }

    #[test]
    fn parses_port_as_u16() {
        let instr = Query::SshPort.instruction();
        assert_eq!(drive(&instr, "22023\r\n"), Step::Done(Value::Port(22023)));
    }

    #[test]
    fn parses_telnet_port_bare_with_leading_zeros() {
        let instr = Query::TelnetPort.instruction();
        assert_eq!(drive(&instr, "00023\r\n"), Step::Done(Value::Port(23)));
    }

    #[test]
    fn out_of_range_port_does_not_silently_truncate() {
        // `99999` overflows u16. The offset search must not slide forward and
        // decode the truncated `9999` — the u16 conversion emits `Cut`, which
        // stops the search, so this reports NeedMore rather than Port(9999).
        let instr = Query::TelnetPort.instruction();
        assert_eq!(drive(&instr, "99999\r\n"), Step::NeedMore);
    }

    #[test]
    fn parses_active_alarms_list() {
        let instr = Query::ActiveAlarms.instruction();
        let resp = "<name:virtual_input,level:critical>*<name:video_loss,level:critical>*<name:publish_failure,level:warning>\r\n";
        assert_eq!(
            drive(&instr, resp),
            Step::Done(Value::Alarms(vec![
                ("virtual_input".to_string(), "critical".to_string()),
                ("video_loss".to_string(), "critical".to_string()),
                ("publish_failure".to_string(), "warning".to_string()),
            ]))
        );
    }

    #[test]
    fn parses_flag() {
        let instr = Query::DhcpMode.instruction();
        assert_eq!(drive(&instr, "1\r\n"), Step::Done(Value::Flag(true)));
    }

    #[test]
    fn parses_firmware_version_skipping_echo() {
        let instr = Query::Firmware.instruction();
        assert_eq!(
            drive(&instr, "2.11\r\n"),
            Step::Done(Value::Text("2.11".into())) // TODO parse as Value::Version
        );
    }

    #[test]
    fn parses_mac() {
        let instr = Query::MacAddress.instruction();
        let got = drive(&instr, "00-05-A6-1B-2C-3D\r\n");
        assert_eq!(
            got,
            Step::Done(Value::Mac(MacAddr([0x00, 0x05, 0xA6, 0x1B, 0x2C, 0x3D])))
        );
    }

    #[test]
    fn parses_empty_register() {
        let instr = Query::Title.instruction();
        assert_eq!(
            drive(&instr, "\r\n"),
            Step::Done(Value::Text(String::new()))
        );
    }

    #[test]
    fn parses_register_value() {
        let instr = Query::Title.instruction();
        assert_eq!(
            drive(&instr, "Lecture 1\r\n"),
            Step::Done(Value::Text("Lecture 1".into()))
        );
    }

    #[test]
    fn parses_settable_echo() {
        let instr = Register::Title.instruction("Hello");
        assert!(instr.payload.contains("M13*Hello"));
        assert_eq!(
            drive(&instr, "RcdrM13*Hello\r\n"),
            Step::Done(Value::Text("Hello".into()))
        );
    }

    #[test]
    fn parses_command_ack() {
        let instr = Command::Start.instruction();
        assert_eq!(
            drive(&instr, "RcdrY1\r\n"),
            Step::Done(Value::Ack("RcdrY1".into()))
        );
    }

    #[test]
    fn incomplete_buffer_needs_more() {
        let instr = Query::SshPort.instruction();
        assert_eq!(drive(&instr, "BPMAP\r\n220"), Step::NeedMore);
    }

    #[test]
    fn settable_truncates_to_127() {
        let long = "a".repeat(300);
        let instr = Register::Title.instruction(&long);
        let value_len = instr.payload.chars().count() - 1 - 3 - 1 - 4 - 1;
        assert_eq!(value_len, MAX_VALUE_LEN);
    }

    #[test]
    fn enums_round_trip_names() {
        for q in Query::ALL {
            assert_eq!(Query::from_str(q.name()).unwrap(), *q);
        }
        assert_eq!(
            Query::from_str("running-state").unwrap(),
            Query::RunningState
        );
        assert_eq!(Command::from_str("start").unwrap(), Command::Start);
        assert!(Query::from_str("nope").is_err());
    }

    /// Every spelling a catalog advertises must be one `FromStr` accepts —
    /// across all four catalogs, and for aliases as well as canonical names.
    ///
    /// The hole this closes: `FromStr` normalizes its *input* and matches it
    /// against the name literals as written, so a literal that is not itself
    /// normalized — `RTMP_Stream_1_State`, say — is matched by nothing at all.
    /// The variant is still in `ALL`, still rendered into the generated docs and
    /// the Python stub, and still unreachable by name, which is a failure no
    /// caller can distinguish from a typo of their own. `accepted()` is the same
    /// list the stub and the docs are built from, so this pins the whole
    /// advertised surface rather than one sample of it.
    #[test]
    fn every_advertised_spelling_resolves_in_every_catalog() {
        fn check<T>(all: &'static [T], accepted: fn(T) -> &'static [&'static str])
        where
            T: Copy + PartialEq + fmt::Debug + FromStr,
            <T as FromStr>::Err: fmt::Debug,
        {
            for &variant in all {
                for spelling in accepted(variant) {
                    // The macro's rule, asserted directly rather than inferred
                    // from the lookup below: a *canonical* name that is not
                    // normalized can still resolve, if some alias happens to
                    // normalize to the same string and rescues it. It would
                    // nonetheless be advertised in mixed case by `name()` — in
                    // the generated docs, the stub, and the `{field}` routes —
                    // which is the inconsistency the rule exists to prevent.
                    assert_eq!(
                        *spelling,
                        crate::protocol::payload_helpers::normalize(spelling),
                        "{variant:?} advertises {spelling:?}, which is not in normalized form"
                    );
                    let got = T::from_str(spelling)
                        .unwrap_or_else(|e| panic!("{variant:?} advertises {spelling:?}: {e:?}"));
                    assert_eq!(got, variant, "{spelling:?} resolves to the wrong variant");
                    // The lowercase form is what the Python stub publishes.
                    assert_eq!(
                        T::from_str(&spelling.to_ascii_lowercase()).unwrap(),
                        variant,
                        "{spelling:?} does not resolve in the lowercase form the stub publishes"
                    );
                }
            }
        }

        check(Query::ALL, Query::accepted);
        check(Command::ALL, Command::accepted);
        check(Register::ALL, Register::accepted);
        check(Setting::ALL, Setting::accepted);
    }
}
