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
    TooLong { max: usize, got: usize },
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
        }
    }

    /// The SIS verb this setting writes through. Crate-visible so the simulator
    /// can build its write echo from the same verb the client addressed, rather
    /// than re-spelling the token in a second place — as
    /// [`Register::reg`](super::register::Register::reg) and
    /// [`Command::verb`](super::commands::Command::verb) already are.
    ///
    /// Every verb below is the *read* verb attested in `query.rs`. The write
    /// form is the same verb with the value in front — verify per model against
    /// the SIS reference before shipping. See the design note's verification
    /// section.
    pub(crate) fn verb(self) -> &'static str {
        match self {
            Setting::UnitName => "CN",
            Setting::Timezone => "TZON",
            Setting::DhcpMode => "DH",
            Setting::SnmpState => "ESNMP",
            Setting::SnmpUnitLocation => "LSNMP",
            Setting::SnmpUnitContact => "CSNMP",
            Setting::TelnetPort => "MT",
            Setting::HttpPort => "MH",
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
        Ok(Instruction {
            name: self.name().to_string(),
            payload: format!("{ESC}{encoded}{verb}{CR}"),
            parser: setting_echo(verb),
        })
    }
}

/// Echo after writing a device setting: `<verb><value> CR LF`.
///
/// Shaped like `register::settable_echo` and different in one respect: a setting echo
/// carries no `RCDR` prefix and no `*` separator, because the write it answers
/// is addressed to the device rather than to the recorder subsystem. Confirm
/// the exact echo per verb against the SIS reference before shipping.
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
}
