use winnow::combinator::separated;
use winnow::error::{ContextError, ErrMode};
use winnow::token::{literal, one_of, take_while};
use winnow::{ModalResult, Parser};

// ---- Query (gettable) enum ------------------------------------------------
use crate::protocol::instructions::{Instruction, Session};
use crate::protocol::payload_helpers::{esc_cr, esc_rcdr, is_not_cr};
use crate::protocol::states::RecordingState;
use crate::protocol::{In, MacAddr, ParseFn, RecordingEvent, Step, Value, anchored, parser_of};
use sismatic_catalog_macros::catalog;

/// A built-in field that can be queried.
#[catalog]
pub enum Query {
    /// Read all recording events
    RecordingEvents,
    /// Name of stream 1.
    #[name("STREAM_1_NAME")]
    #[alias("STREAM_NAME_1")]
    Stream1Name,
    /// Name of stream 2.
    #[name("STREAM_2_NAME")]
    #[alias("STREAM_NAME_2")]
    Stream2Name,
    /// Name of stream 3.
    #[name("STREAM_3_NAME")]
    #[alias("STREAM_NAME_3")]
    Stream3Name,
    /// Firmware/version string, e.g. 2.11.
    Firmware,
    /// Current recording state (stopped, started, or paused).
    RunningState,
    /// Configured unit name.
    UnitName,
    /// Telnet service port.
    TelnetPort,
    /// SSH service port.
    SshPort,
    /// HTTP service port.
    HttpPort,
    /// SNMP service port.
    SnmpPort,
    /// HTTPS service port.
    HttpsPort,
    /// SNMP unit location string.
    SnmpUnitLocation,
    /// SNMP unit contact string.
    SnmpUnitContact,
    /// SNMP private community string.
    SnmpPrivateCommunityString,
    /// SNMP public community string.
    SnmpPublicCommunityString,
    /// Whether SNMP is enabled.
    SnmpState,
    /// Whether DHCP is enabled.
    DhcpMode,
    /// Configured timezone.
    Timezone,
    /// Hardware MAC address.
    MacAddress,
    /// Per-port timeout in tens of seconds; 30 tens of seconds=300 seconds.
    PortTimeout,
    /// Global port timeout in tens of seconds; 30 tens of seconds=300 seconds.
    GlobalPortTimeout,
    /// Device model name.
    ModelName,
    /// Device model description.
    ModelDescription,
    /// Active alarms.
    ActiveAlarms,
    /// Device part number.
    PartNumber,
    /// Dublin Core Contributor (read)
    Contributor,
    /// Course
    Course,
    /// Dublin Core 'coverage' metadata register (read).
    Coverage,
    /// Dublin Core Date (read-only) (read)
    Date,
    /// Dublin Core Description (read)
    Description,
    /// Dublin Core Format (read)
    Format,
    /// Dublin Core Identifier (read-only) (read)
    Identifier,
    /// Dublin Core 'language' metadata register (read).
    Language,
    /// Presenter metadata register (read).
    Presenter,
    /// Dublin Core 'publisher' metadata register (read).
    Publisher,
    /// Dublin Core 'relation' metadata register (read).
    Relation,
    /// Dublin Core Rights (read)
    Rights,
    /// Dublin Core 'source' metadata register (read).
    Source,
    /// Dublin Core 'subject' metadata register (read).
    Subject,
    /// System Name
    #[name("SYSTEMNAME")]
    SystemName,
    /// Recording title metadata register (read).
    Title,
    /// Dublin Core Type (read)
    Type,
    // Enable state per stream. The index is part of the name for the same
    // reason it is in the three entries above: a `Query` carries no
    // argument, so the address has to live in the variant. `_STATE` follows
    // `SNMP_STATE`/`DHCP_MODE`; `_ENABLED` and `_STATUS` are accepted too.
    /// Whether stream 1 is enabled.
    #[name("STREAM_1_STATE")]
    #[alias("STREAM_1_ENABLED", "STREAM_1_STATUS")]
    Stream1State,
    /// Whether stream 2 is enabled.
    #[name("STREAM_2_STATE")]
    #[alias("STREAM_2_ENABLED", "STREAM_2_STATUS")]
    Stream2State,
    /// Whether stream 3 is enabled.
    #[name("STREAM_3_STATE")]
    #[alias("STREAM_3_ENABLED", "STREAM_3_STATUS")]
    Stream3State,
    // Whether each RTMP push target is actually publishing, as opposed to
    // merely armed — the enable state is `Setting::RTMPStream1State`, and is
    // not readable here yet for the reason given on `instruction` below.
    //
    // Read-only, and not for want of a wire form: SIS has no write for a
    // live state. What puts a push on air is a scheduled session, which is
    // outside this protocol entirely, so these are the one part of the RTMP
    // catalog with no counterpart in `Setting`. A device with scheduling off
    // or unsupported answers the read with an error code rather than a
    // flag — see `SisError`, which is what keeps that from costing a
    // `exchange_timeout` per poll.
    //
    // Per target rather than per stream, because the wire addresses them
    // that way: a stream's primary push can be live while its backup is not,
    // so three streams give six live states.
    //
    // `LIVE_STATE` is load-bearing, not decoration. Without it these read
    // `RTMP_STREAM_<n>_STATE`, which is an accepted spelling of
    // `Setting::RTMPStream<n>State` — the *enable* state, a different field
    // with a write behind it. One name for two fields means a caller can
    // read whether a push is on air and write whether it is armed through
    // the same string and never be told they are not the same thing.
    //
    // The trailing alias is the spelling these shipped under before the
    // index moved behind `STREAM_`; a `sync.fields` config naming fields
    // explicitly would otherwise stop polling them, silently, at the next
    // deploy.
    /// Whether the primary RTMP push for stream 1 is live.
    #[name("RTMP_STREAM_1_LIVE_STATE")]
    #[alias("RTMP_STREAM_LIVE_STATE_1", "RTMP_1_LIVE_STATE")]
    RtmpStream1LiveState,
    /// Whether the primary RTMP push for stream 2 is live.
    #[name("RTMP_STREAM_2_LIVE_STATE")]
    #[alias("RTMP_STREAM_LIVE_STATE_2", "RTMP_2_LIVE_STATE")]
    RtmpStream2LiveState,
    /// Whether the primary RTMP push for stream 3 is live.
    #[name("RTMP_STREAM_3_LIVE_STATE")]
    #[alias("RTMP_STREAM_LIVE_STATE_3", "RTMP_3_LIVE_STATE")]
    RtmpStream3LiveState,
    /// Whether the backup RTMP push for stream 1 is live.
    #[name("RTMP_BACKUP_STREAM_1_LIVE_STATE")]
    #[alias("RTMP_BACKUP_STREAM_LIVE_STATE_1", "RTMP_1_BACKUP_LIVE_STATE")]
    RtmpBackupStream1LiveState,
    /// Whether the backup RTMP push for stream 2 is live.
    #[name("RTMP_BACKUP_STREAM_2_LIVE_STATE")]
    #[alias("RTMP_BACKUP_STREAM_LIVE_STATE_2", "RTMP_2_BACKUP_LIVE_STATE")]
    RtmpBackupStream2LiveState,
    /// Whether the backup RTMP push for stream 3 is live.
    #[name("RTMP_BACKUP_STREAM_3_LIVE_STATE")]
    #[alias("RTMP_BACKUP_STREAM_LIVE_STATE_3", "RTMP_3_BACKUP_LIVE_STATE")]
    RtmpBackupStream3LiveState,
}

impl Query {
    /// Build the wire instruction for this query.
    ///
    /// The RTMP entries read the live state only — the field that exists
    /// *nowhere else*, since SIS offers no write for it. The other RTMP fields —
    /// the enable state and the publish URLs — are writable through
    /// [`Setting`](super::setting::Setting) but deliberately absent here until
    /// their read forms are attested, because a `Query` is not free to guess at:
    /// a wildcard `sync.fields` config polls every variant in
    /// [`Query::ALL`](Self::ALL) on every cycle, so a payload the device does
    /// not answer costs one command timeout per field per device per interval,
    /// forever. A `Setting` with a wrong payload costs one timeout when someone
    /// writes it.
    pub fn instruction(self) -> Instruction {
        use Query::*;
        let (payload, parser): (String, ParseFn) = match self {
            RecordingEvents => (esc_cr("LRCDR"), recording_events()),
            Stream1Name => (esc_cr("N1STRC"), plain_text()),
            Stream2Name => (esc_cr("N2STRC"), plain_text()),
            Stream3Name => (esc_cr("N3STRC"), plain_text()),
            // `ESC <n> STRC CR` -> `(0|1) CR LF`. The name read one line up is
            // the same verb with an `N` in front of the index, so the two
            // differ on the wire by exactly that letter.
            Stream1State => (esc_cr("1STRC"), boolean_flag()),
            Stream2State => (esc_cr("2STRC"), boolean_flag()),
            Stream3State => (esc_cr("3STRC"), boolean_flag()),
            // `ESC S<i>*<n>RTMP CR` -> `(0|1) CR LF`, where `i` selects the
            // primary (1) or backup (2) target and `n` the stream. All six
            // answer with a bare flag, exactly as the `STRC` reads above do —
            // attested against an SMP 351 on V3.06 (60-1324-01), which never
            // echoes the address back on any of the six.
            RtmpStream1LiveState => (esc_cr("S1*1RTMP"), boolean_flag()),
            RtmpStream2LiveState => (esc_cr("S1*2RTMP"), boolean_flag()),
            RtmpStream3LiveState => (esc_cr("S1*3RTMP"), boolean_flag()),
            RtmpBackupStream1LiveState => (esc_cr("S2*1RTMP"), boolean_flag()),
            RtmpBackupStream2LiveState => (esc_cr("S2*2RTMP"), boolean_flag()),
            RtmpBackupStream3LiveState => (esc_cr("S2*3RTMP"), boolean_flag()),
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
            Language => (esc_rcdr("M7"), plain_text()),
            Presenter => (esc_rcdr("M2"), plain_text()),
            Publisher => (esc_rcdr("M8"), plain_text()),
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
            session: Session::Preserved,
            parser,
        }
    }
}

///// Parse version string into a Value::Version
//fn parse_version() -> ParseFn {
//    parser_of(
//        |i: &mut In| {
//            literal("\r\n").parse_next(i)?;
//            Ok(version.to_string())
//        },
//        Value::Version,
//    )
//}

/// The recording-event list: `(<json> CRLF)* CRLF`.
///
/// # The frame
///
/// Each event is one non-empty JSON object on its own CRLF-terminated line, and
/// a final bare CRLF closes the list. So an empty list is exactly `\r\n`, and a
/// non-empty one ends `\r\n\r\n`. Attested against a real SMP.
///
/// Items being *non-empty* is what makes that terminator unambiguous: `\r\n\r\n`
/// cannot occur between two events, only after the last one. JSON escapes a
/// carriage return inside a string as the two characters `\r`, never as a raw
/// byte, so no event can forge a terminator from inside one of its values
/// either.
///
/// # Why this is `anchored` and not `parser_of`
///
/// Because the frame matches the empty input, which `search` cannot be trusted
/// with — see [`anchored`] for the full account and the wrong answer it would
/// otherwise produce.
///
/// # Why a bad line asks for more bytes
///
/// A line that is not valid JSON returns [`Step::NeedMore`] rather than a
/// shorter list. The alternative — skip it, or stop early — reports a
/// recording count that is quietly wrong, and a wrong count is worse here than
/// a visible `exchange_timeout`: nothing downstream can tell a short list from
/// a true one. This mirrors the `Cut` discipline the numeric parsers use.
fn recording_events() -> ParseFn {
    anchored(|buf: &str| {
        // Before the terminator search, because an empty list *is* the bare
        // terminator and contains no `\r\n\r\n` to find.
        if buf.starts_with("\r\n") {
            return Step::Done(Value::RecordingEvents(Vec::new()));
        }

        let Some(end) = buf.find("\r\n\r\n") else {
            return Step::NeedMore;
        };

        let mut events = Vec::new();
        for line in buf[..end].split("\r\n") {
            match serde_json::from_str::<RecordingEvent>(line) {
                Ok(event) => events.push(event),
                Err(_) => return Step::NeedMore,
            }
        }
        Step::Done(Value::RecordingEvents(events))
    })
}

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
            d.parse::<u16>().or_else(|_| cut())
        },
        Value::Port,
    )
}

fn plain_number() -> ParseFn {
    parser_of(
        |i: &mut In| {
            let d: &str = take_while(1.., |c: char| c.is_ascii_digit()).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            d.parse::<u32>().or_else(|_| cut())
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
///
/// The RTMP live states used to read through an `addressed_flag` here, on the
/// belief that they answer `RtmpS1*1*0` — the address echoed back — and were
/// anchored on that head so the six could not be confused for one another. The
/// device does not do this: it answers all six with a bare flag, the same as
/// `STRC`. The anchor therefore never matched, and since a parser that cannot
/// match simply asks for more bytes, each read spent the full `exchange_timeout`
/// waiting for a reply that had already arrived.
///
/// Nothing takes over the "keeps the six apart" job the anchor was doing,
/// because on this wire there is nothing to do it with — a bare `0` carries no
/// address. What keeps a reply attached to its request is that a device holds
/// one connection under one lock and answers one instruction at a time.
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

/// A *committed* failure: the reply matched this parser's shape but carried an
/// invalid value (e.g. digits that overflow the target integer). Unlike
/// [`backtrack`], this returns `ErrMode::Cut`, which stops the offset `search`
/// in `protocol.rs` from sliding forward and matching a shorter, truncated
/// token — otherwise `99999\r\n` would silently decode to `9999`.
fn cut<O>() -> ModalResult<O> {
    Err(ErrMode::Cut(ContextError::new()))
}

// // character-class predicates for winnow's `take_while`
// fn is_version(c: char) -> bool {
//     c.is_ascii_digit() || c == '.'
// }

// fn is_part(c: char) -> bool {
//     c.is_ascii_digit() || c == '-'
// }

// /// `<prefix> <value: pred*> <terminator>` where the value class is `pred`.
// fn prefixed(
//     prefix: &str,
//     pred: fn(char) -> bool,
//     terminator: &str,
//     wrap: fn(String) -> Value,
// ) -> ParseFn {
//     let prefix = prefix.to_string();
//     let terminator = terminator.to_string();
//     parser_of(
//         move |i: &mut In| {
//             literal(prefix.as_str()).parse_next(i)?;
//             let v: &str = take_while(1.., pred).parse_next(i)?;
//             literal(terminator.as_str()).parse_next(i)?;
//             Ok(v.to_string())
//         },
//         wrap,
//     )
// }
