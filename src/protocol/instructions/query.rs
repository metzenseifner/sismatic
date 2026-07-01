use winnow::error::{ContextError, ErrMode};
use winnow::token::{literal, one_of, take_while};
use winnow::{ModalResult, Parser};

// ---- Query (gettable) enum ------------------------------------------------
use crate::protocol::control_chars::RCDR;
use crate::protocol::instructions::{Instruction, UnknownInstruction};
use crate::protocol::payload_helpers::{esc_cr, esc_rcdr, is_not_cr, normalize};
use crate::protocol::states::RecordingState;
use crate::protocol::{In, MacAddr, ParseFn, Value, parser_of};
use std::fmt;
use std::str::FromStr;

macro_rules! query_enum {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        /// A built-in field that can be queried.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Query {
            $($variant),+
        }

        impl Query {
            /// Every built-in query, in catalog order.
            pub const ALL: &'static [Query] = &[$(Query::$variant),+];

            /// The canonical uppercase name (e.g. `RUNNING_STATE`).
            pub fn name(self) -> &'static str {
                match self { $(Query::$variant => $name),+ }
            }
        }

        impl FromStr for Query {
            type Err = UnknownInstruction;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let norm = normalize(s);
                match norm.as_str() {
                    $($name => Ok(Query::$variant),)+
                    _ => Err(UnknownInstruction(s.to_string())),
                }
            }
        }
    };
}

query_enum! {
    Firmware => "FIRMWARE",
    RunningState => "RUNNING_STATE",
    UnitName => "UNIT_NAME",
    TelnetPort => "TELNET_PORT",
    SshPort => "SSH_PORT",
    HttpPort => "HTTP_PORT",
    SnmpPort => "SNMP_PORT",
    HttpsPort => "HTTPS_PORT",
    SnmpUnitLocation => "SNMP_UNIT_LOCATION",
    SnmpUnitContact => "SNMP_UNIT_CONTACT",
    SnmpPrivateCommunityString => "SNMP_PRIVATE_COMMUNITY_STRING",
    SnmpPublicCommunityString => "SNMP_PUBLIC_COMMUNITY_STRING",
    SnmpState => "SNMP_STATE",
    DhcpMode => "DHCP_MODE",
    Timezone => "TIMEZONE",
    MacAddress => "MAC_ADDRESS",
    PortTimeout => "PORT_TIMEOUT",
    GlobalPortTimeout => "GLOBAL_PORT_TIMEOUT",
    ModelName => "MODEL_NAME",
    ModelDescription => "MODEL_DESCRIPTION",
    ActiveAlarms => "ACTIVE_ALARMS",
    PartNumber => "PART_NUMBER",
    Coverage => "COVERAGE",
    Presenter => "PRESENTER",
    Relation => "RELATION",
    Source => "SOURCE",
    Subject => "SUBJECT",
    Title => "TITLE",
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Query {
    /// Build the wire instruction for this query.
    pub fn instruction(self) -> Instruction {
        use Query::*;
        let (payload, parser): (String, ParseFn) = match self {
            Firmware => ("Q".into(), prefixed("Q", is_version, "\r", Value::Version)),
            RunningState => (esc_rcdr("Y"), parse_state()),
            UnitName => (esc_cr("CN"), framed_text("CN")),
            TelnetPort => (esc_cr("MT"), framed_port("MT")),
            SshPort => (esc_cr("BPMAP"), framed_port("BPMAP")),
            HttpPort => (esc_cr("MH"), framed_port("MH")),
            SnmpPort => (esc_cr("APMAP"), framed_port("APMAP")),
            HttpsPort => (esc_cr("SPMAP"), framed_port("SPMAP")),
            SnmpUnitLocation => (esc_cr("LSNMP"), framed_text("LSNMP")),
            SnmpUnitContact => (esc_cr("CSNMP"), {
                let verb = "CSNMP".to_string();
                parser_of(
                    move |i: &mut In| {
                        literal(verb.as_str()).parse_next(i)?;
                        literal("\r\n").parse_next(i)?;
                        let v: &str = take_while(0.., is_not_cr).parse_next(i)?;
                        literal("\r\r").parse_next(i)?;
                        Ok(v.to_string())
                    },
                    Value::Text,
                )
            }),
            SnmpPrivateCommunityString => (esc_cr("XSNMP"), framed_text("XSNMP")),
            SnmpPublicCommunityString => (esc_cr("PSNMP"), framed_text("PSNMP")),
            SnmpState => (esc_cr("ESNMP"), framed_flag("ESNMP")),
            DhcpMode => (esc_cr("DH"), framed_flag("DH")),
            Timezone => (esc_cr("TZON"), framed_text("TZON")),
            MacAddress => (esc_cr("CH"), framed_mac("CH")),
            PortTimeout => (esc_cr("0TC"), framed_number("0TC")),
            GlobalPortTimeout => (esc_cr("1TC"), framed_number("1TC")),
            ModelName => ("1I".into(), prefixed("1I", is_not_cr, "\r\r", Value::Text)),
            ModelDescription => ("2I".into(), prefixed("2I", is_not_cr, "\r\r", Value::Text)),
            ActiveAlarms => (
                "39I".into(),
                prefixed("39I", is_not_cr, "\r\r", Value::Text),
            ),
            PartNumber => ("N".into(), prefixed("N", is_part, "\r\r", Value::Text)),
            Coverage => (esc_rcdr("M1"), register_query("M1")),
            Presenter => (esc_rcdr("M2"), register_query("M2")),
            Relation => (esc_rcdr("M9"), register_query("M9")),
            Source => (esc_rcdr("M11"), register_query("M11")),
            Subject => (esc_rcdr("M12"), register_query("M12")),
            Title => (esc_rcdr("M13"), register_query("M13")),
        };
        Instruction {
            name: self.name().to_string(),
            payload,
            parser,
        }
    }
}

/// Read-back of a Dublin-Core metadata register: `<reg>RCDR CR LF <value?> CR CR`.
fn register_query(reg: &str) -> ParseFn {
    let head = format!("{reg}{RCDR}");
    parser_of(
        move |i: &mut In| {
            literal(head.as_str()).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            let v: &str = take_while(0.., is_not_cr).parse_next(i)?;
            literal("\r\r").parse_next(i)?;
            Ok(v.to_string())
        },
        Value::Text,
    )
}

/// `YRCDR CR LF (0|1|2) CR CR` decoded to [`RecordingState`].
fn parse_state() -> ParseFn {
    parser_of(
        move |i: &mut In| {
            literal("YRCDR\r\n").parse_next(i)?;
            let d = one_of(['0', '1', '2']).parse_next(i)?;
            literal("\r\r").parse_next(i)?;
            Ok(RecordingState::from_code(d as i32 - '0' as i32))
        },
        Value::State,
    )
}

/// `CH CR LF <xx-xx-xx-xx-xx-xx> CR CR`.
fn framed_mac(verb: &str) -> ParseFn {
    let verb = verb.to_string();
    parser_of(
        move |i: &mut In| {
            literal(verb.as_str()).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            let mac = parse_mac(i)?;
            literal("\r\r").parse_next(i)?;
            Ok(mac)
        },
        Value::Mac,
    )
}

fn parse_mac(i: &mut In) -> ModalResult<MacAddr> {
    let mut bytes = [0u8; 6];
    for (k, byte) in bytes.iter_mut().enumerate() {
        if k > 0 {
            literal("-").parse_next(i)?;
        }
        let h: &str = take_while(2..=2, |c: char| c.is_ascii_hexdigit()).parse_next(i)?;
        *byte = u8::from_str_radix(h, 16).or_else(|_| backtrack())?;
    }
    Ok(MacAddr(bytes))
}

/// `<verb> CR LF <value> CR CR`, value = text up to CR.
fn framed_text(verb: &str) -> ParseFn {
    let verb = verb.to_string();
    parser_of(
        move |i: &mut In| {
            literal(verb.as_str()).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            let v: &str = take_while(0.., is_not_cr).parse_next(i)?;
            literal("\r\r").parse_next(i)?;
            Ok(v.to_string())
        },
        Value::Text,
    )
}

/// `<verb> CR LF <digits> CR CR` as a `u16` port.
fn framed_port(verb: &str) -> ParseFn {
    let verb = verb.to_string();
    parser_of(
        move |i: &mut In| {
            literal(verb.as_str()).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            let d: &str = take_while(1.., |c: char| c.is_ascii_digit()).parse_next(i)?;
            literal("\r\r").parse_next(i)?;
            d.parse::<u16>().or_else(|_| backtrack())
        },
        Value::Port,
    )
}

/// `<verb> CR LF <digits> CR CR` as a `u32` number.
fn framed_number(verb: &str) -> ParseFn {
    let verb = verb.to_string();
    parser_of(
        move |i: &mut In| {
            literal(verb.as_str()).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            let d: &str = take_while(1.., |c: char| c.is_ascii_digit()).parse_next(i)?;
            literal("\r\r").parse_next(i)?;
            d.parse::<u32>().or_else(|_| backtrack())
        },
        Value::Number,
    )
}

/// `<verb> CR LF (0|1) CR CR` as a boolean flag.
fn framed_flag(verb: &str) -> ParseFn {
    let verb = verb.to_string();
    parser_of(
        move |i: &mut In| {
            literal(verb.as_str()).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            let b = one_of(['0', '1']).parse_next(i)?;
            literal("\r\r").parse_next(i)?;
            Ok(b == '1')
        },
        Value::Flag,
    )
}

fn backtrack<O>() -> ModalResult<O> {
    Err(ErrMode::Backtrack(ContextError::new()))
}

// character-class predicates for winnow's `take_while`
fn is_version(c: char) -> bool {
    c.is_ascii_digit() || c == '.'
}

fn is_part(c: char) -> bool {
    c.is_ascii_digit() || c == '-'
}

/// `<prefix> <value: pred*> <terminator>` where the value class is `pred`.
fn prefixed(
    prefix: &str,
    pred: fn(char) -> bool,
    terminator: &str,
    wrap: fn(String) -> Value,
) -> ParseFn {
    let prefix = prefix.to_string();
    let terminator = terminator.to_string();
    parser_of(
        move |i: &mut In| {
            literal(prefix.as_str()).parse_next(i)?;
            let v: &str = take_while(1.., pred).parse_next(i)?;
            literal(terminator.as_str()).parse_next(i)?;
            Ok(v.to_string())
        },
        wrap,
    )
}
