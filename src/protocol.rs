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

mod control_chars;
pub mod instructions;
mod payload_helpers;
mod states;

use std::fmt;
use std::sync::Arc;
use winnow::{ModalResult, Partial};

use crate::protocol::states::RecordingState;

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
        if let Ok(value) = core(&mut input) {
            return Step::Done(value);
        }
    }
    Step::NeedMore
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
    };

    use super::*;

    /// Drive a parser the way the transport does: one byte at a time.
    fn drive(instr: &Instruction, response: &str) -> Step<Value> {
        let mut buf = String::new();
        for ch in response.chars() {
            buf.push(ch);
            if let Step::Done(v) = instr.parse_step(&buf) {
                return Step::Done(v);
            }
        }
        Step::NeedMore
    }

    #[test]
    fn parses_running_state() {
        let instr = Query::RunningState.instruction();
        // Includes a leading echo to prove the search skips it.
        let resp = "\u{1b}YRCDR\rYRCDR\r\n1\r\r";
        assert_eq!(
            drive(&instr, resp),
            Step::Done(Value::State(RecordingState::Started))
        );
    }

    #[test]
    fn parses_port_as_u16() {
        let instr = Query::SshPort.instruction();
        assert_eq!(
            drive(&instr, "BPMAP\r\n22023\r\r"),
            Step::Done(Value::Port(22023))
        );
    }

    #[test]
    fn parses_flag() {
        let instr = Query::DhcpMode.instruction();
        assert_eq!(drive(&instr, "DH\r\n1\r\r"), Step::Done(Value::Flag(true)));
    }

    #[test]
    fn parses_firmware_version_skipping_echo() {
        let instr = Query::Firmware.instruction();
        assert_eq!(
            drive(&instr, "QQ2.11\r"),
            Step::Done(Value::Version("2.11".into()))
        );
    }

    #[test]
    fn parses_mac() {
        let instr = Query::MacAddress.instruction();
        let got = drive(&instr, "CH\r\n00-05-A6-1B-2C-3D\r\r");
        assert_eq!(
            got,
            Step::Done(Value::Mac(MacAddr([0x00, 0x05, 0xA6, 0x1B, 0x2C, 0x3D])))
        );
    }

    #[test]
    fn parses_empty_register() {
        let instr = Query::Title.instruction();
        assert_eq!(
            drive(&instr, "M13RCDR\r\n\r\r"),
            Step::Done(Value::Text(String::new()))
        );
    }

    #[test]
    fn parses_register_value() {
        let instr = Query::Title.instruction();
        assert_eq!(
            drive(&instr, "M13RCDR\r\nLecture 1\r\r"),
            Step::Done(Value::Text("Lecture 1".into()))
        );
    }

    #[test]
    fn parses_settable_echo() {
        let instr = Register::Title.instruction("Hello");
        assert!(instr.payload.contains("M13*Hello"));
        assert_eq!(
            drive(&instr, "M13*HelloRCDR\r\nRcdrM13*Hello\r\r"),
            Step::Done(Value::Text("Hello".into()))
        );
    }

    #[test]
    fn parses_command_ack() {
        let instr = Command::Start.instruction();
        assert_eq!(
            drive(&instr, "Y1RCDR\r\nRcdrY1\r\r"),
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
}
