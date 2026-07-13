use winnow::combinator::separated;
use winnow::error::{ContextError, ErrMode};
use winnow::token::{literal, one_of, take_while};
use winnow::{ModalResult, Parser};

// ---- Query (gettable) enum ------------------------------------------------
use crate::protocol::control_chars::RCDR;
use crate::protocol::instructions::Instruction;
use crate::protocol::instructions::catalog::instruction_catalog;
use crate::protocol::payload_helpers::{esc_cr, esc_rcdr, is_not_cr};
use crate::protocol::states::RecordingState;
use crate::protocol::{In, MacAddr, ParseFn, Value, parser_of};

instruction_catalog! {
    /// A built-in field that can be queried.
    pub enum Query {
        Firmware { name: "FIRMWARE", aliases: [], doc: "Firmware/version string, e.g. 2.11." },
        RunningState { name: "RUNNING_STATE", aliases: [], doc: "Current recording state (stopped, started, or paused)." },
        UnitName { name: "UNIT_NAME", aliases: [], doc: "Configured unit name." },
        TelnetPort { name: "TELNET_PORT", aliases: [], doc: "Telnet service port." },
        SshPort { name: "SSH_PORT", aliases: [], doc: "SSH service port." },
        HttpPort { name: "HTTP_PORT", aliases: [], doc: "HTTP service port." },
        SnmpPort { name: "SNMP_PORT", aliases: [], doc: "SNMP service port." },
        HttpsPort { name: "HTTPS_PORT", aliases: [], doc: "HTTPS service port." },
        SnmpUnitLocation { name: "SNMP_UNIT_LOCATION", aliases: [], doc: "SNMP unit location string." },
        SnmpUnitContact { name: "SNMP_UNIT_CONTACT", aliases: [], doc: "SNMP unit contact string." },
        SnmpPrivateCommunityString { name: "SNMP_PRIVATE_COMMUNITY_STRING", aliases: [], doc: "SNMP private community string." },
        SnmpPublicCommunityString { name: "SNMP_PUBLIC_COMMUNITY_STRING", aliases: [], doc: "SNMP public community string." },
        SnmpState { name: "SNMP_STATE", aliases: [], doc: "Whether SNMP is enabled." },
        DhcpMode { name: "DHCP_MODE", aliases: [], doc: "Whether DHCP is enabled." },
        Timezone { name: "TIMEZONE", aliases: [], doc: "Configured timezone." },
        MacAddress { name: "MAC_ADDRESS", aliases: [], doc: "Hardware MAC address." },
        PortTimeout { name: "PORT_TIMEOUT", aliases: [], doc: "Per-port timeout in tens of seconds; 30 tens of seconds=300 seconds." },
        GlobalPortTimeout { name: "GLOBAL_PORT_TIMEOUT", aliases: [], doc: "Global port timeout in tens of seconds; 30 tens of seconds=300 seconds." },
        ModelName { name: "MODEL_NAME", aliases: [], doc: "Device model name." },
        ModelDescription { name: "MODEL_DESCRIPTION", aliases: [], doc: "Device model description." },
        ActiveAlarms { name: "ACTIVE_ALARMS", aliases: [], doc: "Active alarms." },
        PartNumber { name: "PART_NUMBER", aliases: [], doc: "Device part number." },
        Contributor { name: "CONTRIBUTOR", aliases:[], doc:"Dublin Core Contributor (read)"},
        Course {name:"COURSE", aliases:[], doc:"Course"},
        Coverage { name: "COVERAGE", aliases: [], doc: "Dublin Core 'coverage' metadata register (read)." },
        Date { name: "DATE", aliases: [], doc: "Dublin Core Date (read-only) (read)"},
        Description { name:"DESCRIPTION", aliases:[], doc: "Dublin Core Description (read)"},
        Format {name: "FORMAT", aliases: [], doc:"Dublin Core Format (read)"},
        Identifier {name:"IDENTIFIER", aliases:[], doc: "Dublin Core Identifier (read)"},
        Presenter { name: "PRESENTER", aliases: [], doc: "Presenter metadata register (read)." },
        Relation { name: "RELATION", aliases: [], doc: "Dublin Core 'relation' metadata register (read)." },
        Rights {name: "RIGHTS", aliases:[], doc: "Dublin Core Rights (read)"},
        Source { name: "SOURCE", aliases: [], doc: "Dublin Core 'source' metadata register (read)." },
        Subject { name: "SUBJECT", aliases: [], doc: "Dublin Core 'subject' metadata register (read)." },
        SystemName {name:"SYSTEMNAME", aliases:[], doc: "System Name"},
        Title { name: "TITLE", aliases: [], doc: "Recording title metadata register (read)." },
        Type {name: "TYPE", aliases:[], doc: "Dublin Core Type (read)"},
    }
}

impl Query {
    /// Build the wire instruction for this query.
    pub fn instruction(self) -> Instruction {
        use Query::*;
        let (payload, parser): (String, ParseFn) = match self {
            Firmware => ("Q".into(), plain_text()),
            RunningState => (esc_rcdr("Y"), parse_state()),
            UnitName => (esc_cr("CN"), plain_text()),
            TelnetPort => (esc_cr("MT"), plain_port()),
            SshPort => (esc_cr("BPMAP"), plain_port()),
            HttpPort => (esc_cr("MH"), plain_port()),
            SnmpPort => (esc_cr("APMAP"), plain_port()),
            HttpsPort => (esc_cr("SPMAP"), plain_port()),
            SnmpUnitLocation => (esc_cr("LSNMP"), plain_text()), // TODO: limit to 64 chars
            SnmpUnitContact => (esc_cr("CSNMP"), plain_text()),  // TODO limit to 64 chars
            SnmpPrivateCommunityString => (esc_cr("XSNMP"), plain_text()), // TODO limit to 64 chars
            SnmpPublicCommunityString => (esc_cr("PSNMP"), plain_text()), // TODO limit to 64 chars
            SnmpState => (esc_cr("ESNMP"), boolean_flag()),
            DhcpMode => (esc_cr("DH"), boolean_flag()),
            Timezone => (esc_cr("TZON"), plain_text()),
            MacAddress => (esc_cr("CH"), mac_address()),
            PortTimeout => (esc_cr("0TC"), plain_number()),
            GlobalPortTimeout => (esc_cr("1TC"), plain_number()),
            ModelName => ("1I".into(), plain_text()),
            ModelDescription => ("2I".into(), plain_text()), // TODO Device name (63 characters, max); must comply with internet host name
            ActiveAlarms => ("39I".into(), active_alarms()),
            PartNumber => ("N".into(), plain_text()), // TODO parser specifically for 60-1324-01\r\n
            Contributor => (esc_rcdr("M0"), plain_text()),
            Course => (esc_rcdr("M16"), plain_text()),
            Coverage => (esc_rcdr("M1"), plain_text()),
            Date => (esc_rcdr("M3"), plain_text()),
            Description => (esc_rcdr("M4"), plain_text()),
            Format => (esc_rcdr("M5"), plain_text()),
            Identifier => (esc_rcdr("M6"), plain_text()),
            Presenter => (esc_rcdr("M2"), plain_text()),
            Relation => (esc_rcdr("M9"), plain_text()),
            Rights => (esc_rcdr("M10"), plain_text()),
            Source => (esc_rcdr("M11"), plain_text()),
            Subject => (esc_rcdr("M12"), plain_text()),
            SystemName => (esc_rcdr("M15"), plain_text()),
            Title => (esc_rcdr("M13"), plain_text()),
            Type => (esc_rcdr("M14"), plain_text()),
        };
        Instruction {
            name: self.name().to_string(),
            payload,
            parser,
        }
    }
}

/// Parse version string into a Value::Version
//fn parse_version() -> ParseFn {
//    parser_of(
//        |i: &mut In| {
//            literal("\r\n").parse_next(i)?;
//            Ok(version.to_string())
//        },
//        Value::Version,
//    )
//}

/// Active-alarm list: `<name:NAME,level:LEVEL>` records joined by `*` and
/// terminated by CR LF, decoded to `(name, level)` pairs. Example:
/// `<name:video_loss,level:critical>*<name:publish_failure,level:warning>\r\n`.
/// An empty list (bare CR LF) yields no pairs.
fn active_alarms() -> ParseFn {
    parser_of(
        |i: &mut In| {
            let alarms: Vec<(String, String)> = separated(0.., alarm_entry, '*').parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            Ok(alarms)
        },
        Value::Alarms,
    )
}

/// One `<name:NAME,level:LEVEL>` alarm record.
fn alarm_entry(i: &mut In) -> ModalResult<(String, String)> {
    literal("<name:").parse_next(i)?;
    let name: &str = take_while(0.., |c: char| c != ',' && c != '>').parse_next(i)?;
    literal(",level:").parse_next(i)?;
    let level: &str = take_while(0.., |c: char| c != '>').parse_next(i)?;
    literal(">").parse_next(i)?;
    Ok((name.to_string(), level.to_string()))
}

/// `YRCDR CR LF (0|1|2) CR CR` decoded to [`RecordingState`].
fn parse_state() -> ParseFn {
    parser_of(
        move |i: &mut In| {
            let d = one_of(['0', '1', '2']).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            Ok(RecordingState::from_code(d as i32 - '0' as i32))
        },
        Value::State,
    )
}

///// `CH CR LF <xx-xx-xx-xx-xx-xx> CR CR`.
//fn framed_mac(verb: &str) -> ParseFn {
//    let verb = verb.to_string();
//    parser_of(
//        move |i: &mut In| {
//            literal(verb.as_str()).parse_next(i)?;
//            literal("\r\n").parse_next(i)?;
//            let mac = parse_mac(i)?;
//            literal("\r\r").parse_next(i)?;
//            Ok(mac)
//        },
//        Value::Mac,
//    )
//}

/// `xx-xx-xx-xx-xx-xx CR LF`.
fn mac_address() -> ParseFn {
    parser_of(
        move |i: &mut In| {
            let mac = parse_mac(i)?;
            literal("\r\n").parse_next(i)?;
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

/// `<value> CR LF` — the SMP's plain, untagged reply in its default verbose
/// mode. Unlike [`framed_text`] there is no verb tag and the terminator is a
/// single CR LF, so the caller must ensure the login banner has been drained
/// first (see the ssh transport) or a banner line would parse as the value.
fn plain_text() -> ParseFn {
    parser_of(
        move |i: &mut In| {
            let v: &str = take_while(0.., is_not_cr).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            Ok(v.to_string())
        },
        Value::Text,
    )
}

// /// `<verb> CR LF <value> CR CR`, value = text up to CR.
// fn framed_text(verb: &str) -> ParseFn {
//     let verb = verb.to_string();
//     parser_of(
//         move |i: &mut In| {
//             literal(verb.as_str()).parse_next(i)?;
//             literal("\r\n").parse_next(i)?;
//             let v: &str = take_while(0.., is_not_cr).parse_next(i)?;
//             literal("\r\r").parse_next(i)?;
//             Ok(v.to_string())
//         },
//         Value::Text,
//     )
// }

/// `<digits> CR LF` as a `u16` port, e.g. `00023\r\n` → 23. The SMP's telnet
/// port reply is bare digits with no verb tag, so leading zeros are stripped by
/// the numeric parse.
fn plain_port() -> ParseFn {
    parser_of(
        |i: &mut In| {
            let d: &str = take_while(1.., |c: char| c.is_ascii_digit()).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            d.parse::<u16>().or_else(|_| backtrack())
        },
        Value::Port,
    )
}

fn plain_number() -> ParseFn {
    parser_of(
        |i: &mut In| {
            let d: &str = take_while(1.., |c: char| c.is_ascii_digit()).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            d.parse::<u32>().or_else(|_| backtrack())
        },
        Value::Number,
    )
}

// /// `<verb> CR LF <digits> CR CR` as a `u16` port.
// fn framed_port(verb: &str) -> ParseFn {
//     let verb = verb.to_string();
//     parser_of(
//         move |i: &mut In| {
//             literal(verb.as_str()).parse_next(i)?;
//             literal("\r\n").parse_next(i)?;
//             let d: &str = take_while(1.., |c: char| c.is_ascii_digit()).parse_next(i)?;
//             literal("\r\r").parse_next(i)?;
//             d.parse::<u16>().or_else(|_| backtrack())
//         },
//         Value::Port,
//     )
// }

// /// `<verb> CR LF <digits> CR CR` as a `u32` number.
// fn framed_number(verb: &str) -> ParseFn {
//     let verb = verb.to_string();
//     parser_of(
//         move |i: &mut In| {
//             literal(verb.as_str()).parse_next(i)?;
//             literal("\r\n").parse_next(i)?;
//             let d: &str = take_while(1.., |c: char| c.is_ascii_digit()).parse_next(i)?;
//             literal("\r\r").parse_next(i)?;
//             d.parse::<u32>().or_else(|_| backtrack())
//         },
//         Value::Number,
//     )
// }

/// `(0|1) CR LF` as a boolean flag.
fn boolean_flag() -> ParseFn {
    parser_of(
        move |i: &mut In| {
            let b = one_of(['0', '1']).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            Ok(b == '1')
        },
        Value::Flag,
    )
}

// /// `<verb> CR LF (0|1) CR CR` as a boolean flag.
// fn framed_flag(verb: &str) -> ParseFn {
//     let verb = verb.to_string();
//     parser_of(
//         move |i: &mut In| {
//             literal(verb.as_str()).parse_next(i)?;
//             literal("\r\n").parse_next(i)?;
//             let b = one_of(['0', '1']).parse_next(i)?;
//             literal("\r\r").parse_next(i)?;
//             Ok(b == '1')
//         },
//         Value::Flag,
//     )
// }

fn backtrack<O>() -> ModalResult<O> {
    Err(ErrMode::Backtrack(ContextError::new()))
}

// character-class predicates for winnow's `take_while`
fn is_version(c: char) -> bool {
    c.is_ascii_digit() || c == '.'
}

// fn is_part(c: char) -> bool {
//     c.is_ascii_digit() || c == '-'
// }

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
