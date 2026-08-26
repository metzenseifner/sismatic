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
use winnow::error::ErrMode;
use winnow::{ModalResult, Partial};

// Re-exported so consumers (e.g. the DTO conversion in `sismatic-sync`) can name
// the type for an *exhaustive* match — the private `states` module keeps its
// other items internal.
pub use crate::protocol::states::RecordingState;

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

    /// `ESC S<i>*<n>RTMP CR` -> `RtmpS<i>*<n>*(0|1) CR LF`. Unlike the `STRC`
    /// reads above, the reply repeats its own address, so the flag is anchored
    /// rather than bare.
    #[test]
    fn parses_rtmp_live_state_as_an_addressed_flag() {
        let instr = Query::Rtmp2LiveState.instruction();
        assert_eq!(instr.payload, "\u{1b}S1*2RTMP\r");
        assert_eq!(
            drive(&instr, "RtmpS1*2*1\r\n"),
            Step::Done(Value::Flag(true))
        );

        let backup = Query::Rtmp2BackupLiveState.instruction();
        assert_eq!(backup.payload, "\u{1b}S2*2RTMP\r");
        assert_eq!(
            drive(&backup, "RtmpS2*2*0\r\n"),
            Step::Done(Value::Flag(false))
        );
    }

    /// The six live states differ only by address, and `search` tries a parser
    /// at every offset in a shared buffer. Without the anchor, whichever reply
    /// arrived first would answer all six reads — reporting stream 2's primary
    /// push as live because stream 1's is.
    #[test]
    fn an_rtmp_live_state_does_not_accept_another_targets_reply() {
        let instr = Query::Rtmp1LiveState.instruction();
        assert_eq!(drive(&instr, "RtmpS1*2*1\r\n"), Step::NeedMore);
        assert_eq!(drive(&instr, "RtmpS2*1*1\r\n"), Step::NeedMore);
        // The echoed request must not satisfy it either: it contains `S1*1`,
        // but the anchor demands the title-cased verb in front.
        assert_eq!(drive(&instr, "\u{1b}S1*1RTMP\r\n"), Step::NeedMore);
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
