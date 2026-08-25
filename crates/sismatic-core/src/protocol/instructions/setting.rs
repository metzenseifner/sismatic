// ---- Setting (writable device configuration) ------------------------------

use winnow::Parser;
use winnow::token::{literal, take_while};

use crate::protocol::control_chars::{CR, ESC};
use crate::protocol::instructions::Instruction;
use crate::protocol::instructions::catalog::instruction_catalog;
use crate::protocol::payload_helpers::is_not_cr;
use crate::protocol::{In, ParseFn, Value, parser_of};

instruction_catalog! {
    /// A built-in device setting that can be written.
    ///
    /// Names are the same canonical spellings [`Query`](super::query::Query)
    /// uses for the read of the same field, so `GET .../fields/TIMEZONE` and
    /// `PUT .../settings/TIMEZONE` name one thing.
    pub enum Setting {
        UnitName { name: "UNIT_NAME", aliases: [], doc: "Configured unit name." },
        Timezone { name: "TIMEZONE", aliases: [], doc: "Configured timezone." },
        DhcpMode { name: "DHCP_MODE", aliases: [], doc: "Whether DHCP is enabled." },
        SnmpState { name: "SNMP_STATE", aliases: [], doc: "Whether SNMP is enabled." },
        SnmpUnitLocation { name: "SNMP_UNIT_LOCATION", aliases: [], doc: "SNMP unit location string." },
        SnmpUnitContact { name: "SNMP_UNIT_CONTACT", aliases: [], doc: "SNMP unit contact string." },
        TelnetPort { name: "TELNET_PORT", aliases: [], doc: "Telnet service port." },
        HttpPort { name: "HTTP_PORT", aliases: [], doc: "HTTP service port." },
        // One variant per stream rather than one name plus an index argument:
        // `instruction` is reached by name from `Intent::SetSetting`, and the
        // whole write path — the `{field}` route, the outbox, the command log —
        // addresses a setting by that name alone. Names and aliases match
        // `Query`'s, so the read and the write of a stream are one field.
        Stream1Name { name: "STREAM_1_NAME", aliases: ["STREAM_NAME_1"], doc: "Name of stream 1." },
        Stream2Name { name: "STREAM_2_NAME", aliases: ["STREAM_NAME_2"], doc: "Name of stream 2." },
        Stream3Name { name: "STREAM_3_NAME", aliases: ["STREAM_NAME_3"], doc: "Name of stream 3." },
        Stream1State { name: "STREAM_1_STATE", aliases: ["STREAM_1_ENABLED", "STREAM_1_STATUS"], doc: "Whether stream 1 is enabled." },
        Stream2State { name: "STREAM_2_STATE", aliases: ["STREAM_2_ENABLED", "STREAM_2_STATUS"], doc: "Whether stream 2 is enabled." },
        Stream3State { name: "STREAM_3_STATE", aliases: ["STREAM_3_ENABLED", "STREAM_3_STATUS"], doc: "Whether stream 3 is enabled." },
    }
}

/// What a setting's value must look like on the wire.
///
/// One shape per *kind* of value rather than one validator per field, so eight
/// settings need three encoders. A ninth setting of an existing kind adds a
/// catalog line and a match arm and no new code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `0` or `1`. Accepts the same spellings a human writes.
    Flag,
    /// A TCP port, 1..=65535.
    Port,
    /// Free text with a device-imposed ceiling.
    Text { max: usize },
    /// Free text that must also survive an `*`-delimited frame — see
    /// [`Form::Addressed`]. Same ceiling as [`Shape::Text`], plus the
    /// characters that would re-cut the payload the device receives.
    Token { max: usize },
}

/// Where the value sits in the payload, and so what the device echoes back.
///
/// Orthogonal to [`Shape`]: `Shape` constrains the value, `Form` frames it. A
/// setting needs an address when the device has several like instances of one
/// thing — three streams behind one `STRC` verb — and the address is what tells
/// them apart on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    /// `ESC <value><verb> CR`, echoed as `<verb><value> CR LF`.
    Prefixed,
    /// `ESC <addr>*<value><verb> CR`, echoed as `<Verb><addr>*<value> CR LF`.
    Addressed(&'static str),
}

/// Why a value was refused before any byte was sent.
///
/// Refused here rather than at the device, because the device answers a bad
/// write by echoing an error token this catalog does not model, which surfaces
/// to a caller as a command timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueError {
    NotAFlag(String),
    NotAPort(String),
    TooLong {
        max: usize,
        got: usize,
    },
    /// A character that would re-cut the payload the device parses: the `*`
    /// that separates address from value, or a control character such as the
    /// CR that terminates the whole message.
    IllegalCharacter(char),
}

impl std::fmt::Display for ValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValueError::NotAFlag(v) => {
                write!(f, "'{v}' is not a flag; use 1/0, true/false, on/off")
            }
            ValueError::NotAPort(v) => write!(f, "'{v}' is not a TCP port in 1..=65535"),
            ValueError::TooLong { max, got } => {
                write!(f, "value is {got} characters; this setting accepts {max}")
            }
            ValueError::IllegalCharacter(c) => write!(
                f,
                "'{}' cannot appear in this value; it delimits the field on the wire",
                c.escape_debug()
            ),
        }
    }
}

impl std::error::Error for ValueError {}

/// Render `value` in the wire form `shape` demands. Pure and total.
fn encode(shape: Shape, value: &str) -> Result<String, ValueError> {
    match shape {
        Shape::Flag => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "on" | "yes" => Ok("1".to_owned()),
            "0" | "false" | "off" | "no" => Ok("0".to_owned()),
            _ => Err(ValueError::NotAFlag(value.to_owned())),
        },
        Shape::Port => value
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .map(|p| p.to_string())
            .ok_or_else(|| ValueError::NotAPort(value.to_owned())),
        Shape::Text { max } => {
            let got = value.chars().count();
            // Rejected rather than truncated, unlike `Register`. A truncated
            // hostname is a different hostname, and the caller is in a position
            // to shorten it deliberately.
            if got > max {
                Err(ValueError::TooLong { max, got })
            } else {
                Ok(value.to_owned())
            }
        }
        // Delegates the ceiling to `Text` so the length rule lives in one
        // place, then adds what the frame itself demands. Refused rather than
        // stripped: a `*` in the middle of a stream name means the device would
        // store a *prefix* of what was asked for and report success, which is
        // the same failure mode `Text` refuses to truncate its way into.
        Shape::Token { max } => {
            let text = encode(Shape::Text { max }, value)?;
            match text.chars().find(|c| *c == '*' || c.is_control()) {
                Some(c) => Err(ValueError::IllegalCharacter(c)),
                None => Ok(text),
            }
        }
    }
}

impl Setting {
    fn shape(self) -> Shape {
        match self {
            Setting::DhcpMode | Setting::SnmpState => Shape::Flag,
            Setting::TelnetPort | Setting::HttpPort => Shape::Port,
            // Ceilings carried over from the TODOs already recorded beside the
            // matching queries in `query.rs`.
            Setting::SnmpUnitLocation | Setting::SnmpUnitContact => Shape::Text { max: 64 },
            Setting::UnitName => Shape::Text { max: 63 },
            Setting::Timezone => Shape::Text { max: 64 },
            Setting::Stream1State | Setting::Stream2State | Setting::Stream3State => Shape::Flag,
            // 127 is the longest value attested for a SIS write on this model
            // (`register::MAX_VALUE_LEN`), used here as a ceiling that will not
            // refuse a name the device would have accepted. Tighten it once the
            // stream-name limit is confirmed against the SIS guide — the
            // encoder rejects rather than truncates, so a ceiling set too low
            // turns a valid name into an error.
            Setting::Stream1Name | Setting::Stream2Name | Setting::Stream3Name => {
                Shape::Token { max: 127 }
            }
        }
    }

    /// How this setting's payload is framed. Exhaustive rather than
    /// `_ => Form::Prefixed`, for the reason [`shape`](Self::shape) and
    /// [`verb`](Self::verb) are: a setting added to the catalog must not
    /// acquire a framing by default.
    fn form(self) -> Form {
        match self {
            Setting::Stream1Name => Form::Addressed("N1"),
            Setting::Stream2Name => Form::Addressed("N2"),
            Setting::Stream3Name => Form::Addressed("N3"),
            Setting::Stream1State => Form::Addressed("1"),
            Setting::Stream2State => Form::Addressed("2"),
            Setting::Stream3State => Form::Addressed("3"),
            Setting::UnitName
            | Setting::Timezone
            | Setting::DhcpMode
            | Setting::SnmpState
            | Setting::SnmpUnitLocation
            | Setting::SnmpUnitContact
            | Setting::TelnetPort
            | Setting::HttpPort => Form::Prefixed,
        }
    }

    /// The SIS verb this setting writes through.
    ///
    /// Every verb below is the *read* verb attested in `query.rs`. The write
    /// form is the same verb with the value in front — verify per model against
    /// the SIS reference before shipping. See the design note's verification
    /// section.
    fn verb(self) -> &'static str {
        match self {
            Setting::UnitName => "CN",
            Setting::Timezone => "TZON",
            Setting::DhcpMode => "DH",
            Setting::SnmpState => "ESNMP",
            Setting::SnmpUnitLocation => "LSNMP",
            Setting::SnmpUnitContact => "CSNMP",
            Setting::TelnetPort => "MT",
            Setting::HttpPort => "MH",
            // One verb for all six stream settings; the address in `form`
            // selects the stream, and the `N` in a name's address selects the
            // name rather than the enable state.
            Setting::Stream1Name
            | Setting::Stream2Name
            | Setting::Stream3Name
            | Setting::Stream1State
            | Setting::Stream2State
            | Setting::Stream3State => "STRC",
        }
    }

    /// Build the wire instruction that writes `value` into this setting.
    ///
    /// Fallible where [`Register::instruction`](super::register::Register::instruction)
    /// is not, because a setting's value has a shape and a metadata register's
    /// does not.
    ///
    /// Built as a struct literal rather than through
    /// [`Instruction::custom`], for the same reason `Register::instruction` is:
    /// `custom` takes a bare `impl Fn(&str) -> Step<Value>` and wraps it in an
    /// `Arc`, while `parser_of` has already produced that `Arc`. `Arc<F>`
    /// does not implement `Fn`, so the only way to route one through `custom`
    /// is a closure that calls through it — a second allocation and a second
    /// dynamic dispatch on every parse step. `custom` remains the extension
    /// point for callers outside this module, which have no `parser_of`.
    pub fn instruction(self, value: &str) -> Result<Instruction, ValueError> {
        let encoded = encode(self.shape(), value)?;
        let verb = self.verb();
        let (payload, parser) = match self.form() {
            Form::Prefixed => (format!("{ESC}{encoded}{verb}{CR}"), setting_echo(verb)),
            Form::Addressed(addr) => (
                format!("{ESC}{addr}*{encoded}{verb}{CR}"),
                addressed_echo(verb, addr),
            ),
        };
        Ok(Instruction {
            name: self.name().to_string(),
            payload,
            parser,
        })
    }
}

/// Echo after writing a device setting: `<verb><value> CR LF`.
///
/// Shaped like `register::settable_echo` and different in one respect: a setting echo
/// carries no `RCDR` prefix and no `*` separator, because the write it answers
/// is addressed to the device rather than to the recorder subsystem. Confirm
/// the exact echo per verb against the SIS reference before shipping.
/// Echo after an addressed write: `<Verb><addr>*<value> CR LF` — `Strc1*1` for
/// a stream-enable write, `StrcN1*<name>` for a stream-name write.
///
/// Anchored on the whole `<Verb><addr>*` head, and that is what makes it safe.
/// [`search`](crate::protocol) runs this parser at every offset in the
/// accumulated buffer, so an anchor the *request* also contains would match the
/// request's own bytes on a device that echoes them — and the request does
/// contain `1*`. It cannot contain `Strc1*`, because the request spells the
/// verb in upper case and puts it after the value.
/// [`register::settable_echo`](super::register) is anchored the same way and
/// survives for the same reason.
///
/// The captured value is [`Value::Text`] even for a flag write, matching
/// [`setting_echo`]: a write's result is the device's confirmation of what it
/// stored, and it is the *read* side that gives a field its type.
fn addressed_echo(verb: &'static str, addr: &'static str) -> ParseFn {
    let head = format!("{}{addr}*", echoed(verb));
    parser_of(
        move |i: &mut In| {
            literal(head.as_str()).parse_next(i)?;
            let v: &str = take_while(0.., is_not_cr).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            Ok(v.to_string())
        },
        Value::Text,
    )
}

/// A verb as the device echoes it: title case, `STRC` -> `Strc`.
///
/// Derived rather than tabled because the device applies one rule to both verbs
/// attested so far — the recorder's `RCDR` comes back as `Rcdr` (see
/// `control_chars::RCDR_LOWER`) and stream control's `STRC` as `Strc`.
fn echoed(verb: &str) -> String {
    let mut chars = verb.chars();
    match chars.next() {
        Some(first) => format!(
            "{}{}",
            first.to_ascii_uppercase(),
            chars.as_str().to_ascii_lowercase()
        ),
        None => String::new(),
    }
}

fn setting_echo(verb: &'static str) -> ParseFn {
    parser_of(
        move |i: &mut In| {
            literal(verb).parse_next(i)?;
            let v: &str = take_while(0.., is_not_cr).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            Ok(v.to_string())
        },
        Value::Text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Step;

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

    /// The parser reaches the instruction. A `ParseFn` routed through
    /// `Instruction::custom` would not compile; one dropped on the floor would
    /// leave this returning `NeedMore` forever.
    #[test]
    fn a_setting_carries_the_parser_for_its_own_echo() {
        let instr = Setting::Timezone.instruction("Europe/Vienna").unwrap();
        assert!(instr.payload.contains("Europe/Vienna"));
        assert_eq!(
            drive(&instr, "TZONEurope/Vienna\r\n"),
            Step::Done(Value::Text("Europe/Vienna".into()))
        );
    }

    #[test]
    fn a_bad_flag_never_becomes_a_payload() {
        assert_eq!(
            Setting::DhcpMode.instruction("maybe").unwrap_err(),
            ValueError::NotAFlag("maybe".into())
        );
    }

    #[test]
    fn an_addressed_write_frames_the_value_between_address_and_verb() {
        let instr = Setting::Stream2Name.instruction("Hall B").unwrap();
        assert_eq!(instr.payload, "\u{1b}N2*Hall BSTRC\r");
        assert_eq!(
            drive(&instr, "StrcN2*Hall B\r\n"),
            Step::Done(Value::Text("Hall B".into()))
        );
    }

    #[test]
    fn a_stream_enable_write_folds_human_spellings_onto_the_wire_flag() {
        let instr = Setting::Stream1State.instruction("on").unwrap();
        assert_eq!(instr.payload, "\u{1b}1*1STRC\r");
        assert_eq!(
            drive(&instr, "Strc1*1\r\n"),
            Step::Done(Value::Text("1".into()))
        );
    }

    /// The hazard the title-case anchor exists to close. `search` tries the
    /// parser at every offset, so a device that echoes the request must not
    /// satisfy the reply parser — otherwise `1*1STRC` decodes as a value and
    /// the write reports success without the device ever having answered.
    #[test]
    fn an_echoed_request_does_not_satisfy_an_addressed_echo() {
        let instr = Setting::Stream1State.instruction("1").unwrap();
        assert_eq!(drive(&instr, "\u{1b}1*1STRC\r\n"), Step::NeedMore);
    }

    /// A name addressing the wrong stream is not this stream's reply.
    #[test]
    fn an_addressed_echo_is_specific_to_its_stream() {
        let instr = Setting::Stream1Name.instruction("Hall A").unwrap();
        assert_eq!(drive(&instr, "StrcN2*Hall A\r\n"), Step::NeedMore);
    }

    #[test]
    fn a_separator_cannot_enter_a_framed_value() {
        assert_eq!(
            Setting::Stream1Name.instruction("Hall A*B").unwrap_err(),
            ValueError::IllegalCharacter('*')
        );
        assert_eq!(
            Setting::Stream1Name.instruction("Hall\rA").unwrap_err(),
            ValueError::IllegalCharacter('\r')
        );
    }

    /// The read and the write of a stream field are one name, which is what
    /// makes `GET .../fields/STREAM_1_NAME` and `PUT .../settings/STREAM_1_NAME`
    /// address the same thing.
    #[test]
    fn every_stream_setting_names_a_query_of_the_same_name() {
        use crate::protocol::instructions::query::Query;
        use std::str::FromStr;

        for setting in [
            Setting::Stream1Name,
            Setting::Stream2Name,
            Setting::Stream3Name,
            Setting::Stream1State,
            Setting::Stream2State,
            Setting::Stream3State,
        ] {
            assert!(
                Query::from_str(setting.name()).is_ok(),
                "{setting} has no matching query"
            );
        }
    }
}
