// ---- Setting (writable device configuration) ------------------------------

use winnow::Parser;
use winnow::token::{literal, take_while};

use crate::protocol::control_chars::{CR, ESC};
use crate::protocol::instructions::Instruction;
use crate::protocol::instructions::catalog::instruction_catalog;
use crate::protocol::payload_helpers::{echoed, is_not_cr};
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
        // The RTMP push targets. Named `RTMP_<n>_<FIELD>` with a
        // `RTMP_<FIELD>_<n>` alias, which is the `STREAM_1_NAME` /
        // `STREAM_NAME_1` pattern above with `RTMP` in place of `STREAM` — the
        // index sits in the same place across the whole catalog, so a caller
        // that guesses one name from another guesses right.
        //
        // `<n>` is the stream throughout: 1 is Archive Channel A, 2 is Archive
        // Channel B (Dual Mode only), 3 is Confidence. Each stream publishes to
        // a primary and a backup target, which the wire selects with a separate
        // index — hence a `BACKUP` variant of every field that has one, rather
        // than six streams.
        RTMPStream1PublishURL { name: "RTMP_1_URL", aliases: ["RTMP_URL_1"], doc: "Primary RTMP push target URL for stream 1." },
        RTMPStream1BackupPublishURL { name: "RTMP_1_BACKUP_URL", aliases: ["RTMP_BACKUP_URL_1"], doc: "Backup RTMP push target URL for stream 1." },
        RTMPStream2PublishURL { name: "RTMP_2_URL", aliases: ["RTMP_URL_2"], doc: "Primary RTMP push target URL for stream 2." },
        RTMPStream2BackupPublishURL { name: "RTMP_2_BACKUP_URL", aliases: ["RTMP_BACKUP_URL_2"], doc: "Backup RTMP push target URL for stream 2." },
        RTMPStream3PublishURL { name: "RTMP_3_URL", aliases: ["RTMP_URL_3"], doc: "Primary RTMP push target URL for stream 3." },
        RTMPStream3BackupPublishURL { name: "RTMP_3_BACKUP_URL", aliases: ["RTMP_BACKUP_URL_3"], doc: "Backup RTMP push target URL for stream 3." },
        // Enable state per push stream, aliased like `STREAM_1_STATE` is. The
        // `RTMP_STREAM_<n>_STATE` spelling is carried as an alias rather than as
        // the canonical name: a name literal must be normalized (uppercase, `_`
        // for `-`) or `FromStr` — which normalizes its *input* before matching —
        // can never reach it.
        RTMPStream1State { name: "RTMP_1_STATE", aliases: ["RTMP_1_ENABLED", "RTMP_1_STATUS", "RTMP_STREAM_1_STATE"], doc: "Whether the Archive Channel A RTMP push stream is enabled." },
        RTMPStream2State { name: "RTMP_2_STATE", aliases: ["RTMP_2_ENABLED", "RTMP_2_STATUS", "RTMP_STREAM_2_STATE"], doc: "Whether the Archive Channel B RTMP push stream is enabled." },
        RTMPStream3State { name: "RTMP_3_STATE", aliases: ["RTMP_3_ENABLED", "RTMP_3_STATUS", "RTMP_STREAM_3_STATE"], doc: "Whether the Confidence RTMP push stream is enabled." },
        // Whether a push is actually *live* is not here, and not because its
        // wire form is unknown: SIS offers no write for it. Enabling a stream
        // arms it; what puts it on air is a scheduled session, which this
        // protocol does not reach. So live state is a reading, and lives in
        // `Query` alone — the one RTMP field where the read and the write of a
        // name do not pair up, because the write does not exist.
    }
}

/// What a setting's value must look like on the wire.
///
/// One shape per *kind* of value rather than one validator per field, so a
/// catalog of twenty-three settings needs five encoders. A setting of a kind
/// that already exists adds a catalog line and a match arm and no new code; only
/// a genuinely new kind of value — `Url`, most recently — adds a variant here.
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
    /// An RTMP publish target: everything [`Shape::Token`] demands, plus an
    /// `rtmp://` or `rtmps://` scheme.
    ///
    /// The scheme is checked here rather than left to the device because of how
    /// a bad push target fails. A malformed URL is not refused at write time —
    /// the device stores the string and reports success, and the mistake only
    /// surfaces later as a stream that never reaches the CDN, by which point
    /// the lecture is already running. That is the same class of silent-success
    /// failure [`Shape::Text`] refuses to truncate its way into.
    ///
    /// Deliberately shallow: scheme and non-empty host only. Host, path and
    /// stream key are the CDN's grammar, not the device's, and a validator that
    /// second-guesses them would refuse targets the device would have published
    /// to. The empty string is accepted and clears the target.
    Url { max: usize },
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
    ///
    /// The address is whatever selects the field, and it may itself be
    /// compound: `STRC` needs one component (`N1`, the name of stream 1), while
    /// an RTMP publish URL needs two (`U1*1`, the *primary* target of stream 1)
    /// and so carries the separator inside the address. Nothing here has to
    /// change for that — `<addr>*<value>` reads the same either way, and the
    /// echo anchor is built from the whole address — but it is why the value's
    /// own `*` ban lives in [`Shape::Token`] rather than in this frame: the
    /// separator is legal in the address and illegal in the value.
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
    /// Not an RTMP publish target: a push URL must carry an `rtmp://` or
    /// `rtmps://` scheme and name a host. The empty string is not this error —
    /// it clears the target.
    NotAnRtmpUrl(String),
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
            ValueError::NotAnRtmpUrl(v) => write!(
                f,
                "'{v}' is not an RTMP publish target; expected rtmp://host/... or rtmps://host/... \
                 (empty clears the target)"
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
        // Layered on `Token` for the same reason `Token` layers on `Text`: the
        // length rule and the framing rule each live in exactly one place, and
        // this arm adds only what a URL itself demands.
        Shape::Url { max } => {
            let text = encode(Shape::Token { max }, value)?;
            if text.is_empty() || is_rtmp_url(&text) {
                Ok(text)
            } else {
                Err(ValueError::NotAnRtmpUrl(value.to_owned()))
            }
        }
    }
}

/// An `rtmp://` or `rtmps://` URL with a non-empty authority.
///
/// The scheme is matched case-insensitively (RFC 3986 §3.1) but the value is
/// stored as written: the authority and the stream key that follow are not the
/// device's to case-fold, and a lowercased stream key is a dead stream.
fn is_rtmp_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("rtmp://")
        .or_else(|| lower.strip_prefix("rtmps://"));
    // The leading-`/` check is what rejects `rtmp:///live/key`: that is a
    // well-formed URL with an *empty* authority, and so names no host to
    // publish to.
    rest.is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'))
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
            Setting::RTMPStream1State | Setting::RTMPStream2State | Setting::RTMPStream3State => {
                Shape::Flag
            }
            // Same 127 ceiling as the stream names, and provisional for the same
            // reason — it is the longest value attested for a SIS write on this
            // model, not a limit confirmed for this field. It is the one worth
            // confirming first: a push URL carries a CDN-issued stream key, so
            // it is the longest value anything here is likely to write.
            Setting::RTMPStream1PublishURL
            | Setting::RTMPStream2PublishURL
            | Setting::RTMPStream3PublishURL
            | Setting::RTMPStream1BackupPublishURL
            | Setting::RTMPStream2BackupPublishURL
            | Setting::RTMPStream3BackupPublishURL => Shape::Url { max: 127 },
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
            // RTMP addresses the field with a letter and the stream with a
            // digit, and where a field exists per target it takes a second
            // index ahead of the stream: `U1` is the primary URL and `U2` the
            // backup, `S1` the primary live state and `S2` the backup. So
            // `U2*3` is "backup publish URL of stream 3".
            //
            // The enable state is the odd one out at one component: a stream is
            // armed as a whole, not per target.
            Setting::RTMPStream1State => Form::Addressed("E1"),
            Setting::RTMPStream2State => Form::Addressed("E2"),
            Setting::RTMPStream3State => Form::Addressed("E3"),
            Setting::RTMPStream2PublishURL => Form::Addressed("U1*2"),
            Setting::RTMPStream1PublishURL => Form::Addressed("U1*1"),
            Setting::RTMPStream3PublishURL => Form::Addressed("U1*3"),
            Setting::RTMPStream1BackupPublishURL => Form::Addressed("U2*1"),
            Setting::RTMPStream2BackupPublishURL => Form::Addressed("U2*2"),
            Setting::RTMPStream3BackupPublishURL => Form::Addressed("U2*3"),
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
            Setting::RTMPStream1State
            | Setting::RTMPStream2State
            | Setting::RTMPStream3State
            | Setting::RTMPStream1PublishURL
            | Setting::RTMPStream2PublishURL
            | Setting::RTMPStream3PublishURL
            | Setting::RTMPStream1BackupPublishURL
            | Setting::RTMPStream2BackupPublishURL
            | Setting::RTMPStream3BackupPublishURL => "RTMP",
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

    /// `ESC U1*<n>*<url>RTMP CR` -> `RtmpU1*<n>*<url> CR LF`. The address is
    /// compound, so the payload carries two separators before the value and the
    /// value carries none.
    #[test]
    fn an_rtmp_url_write_frames_the_url_after_a_compound_address() {
        let instr = Setting::RTMPStream3PublishURL
            .instruction("rtmp://live.example.org/app/s3cret-key")
            .unwrap();
        assert_eq!(
            instr.payload,
            "\u{1b}U1*3*rtmp://live.example.org/app/s3cret-keyRTMP\r"
        );
        assert_eq!(
            drive(
                &instr,
                "RtmpU1*3*rtmp://live.example.org/app/s3cret-key\r\n"
            ),
            Step::Done(Value::Text("rtmp://live.example.org/app/s3cret-key".into()))
        );
    }

    /// Primary and backup differ only in the first index, which is exactly the
    /// kind of distinction that would go unnoticed: writing the primary target
    /// into the backup slot leaves a stream that publishes correctly and fails
    /// over to nothing.
    #[test]
    fn the_backup_target_is_a_different_address_from_the_primary() {
        let primary = Setting::RTMPStream1PublishURL
            .instruction("rtmp://a.example.org/app/key")
            .unwrap();
        let backup = Setting::RTMPStream1BackupPublishURL
            .instruction("rtmp://a.example.org/app/key")
            .unwrap();
        assert_eq!(
            primary.payload,
            "\u{1b}U1*1*rtmp://a.example.org/app/keyRTMP\r"
        );
        assert_eq!(
            backup.payload,
            "\u{1b}U2*1*rtmp://a.example.org/app/keyRTMP\r"
        );
        // ...and neither accepts the other's reply.
        assert_eq!(
            drive(&primary, "RtmpU2*1*rtmp://a.example.org/app/key\r\n"),
            Step::NeedMore
        );
    }

    /// A push target the device would have stored and never published to. The
    /// device reports success on a write like this, so refusing it here is the
    /// only place the mistake is visible before the stream is due to go live.
    #[test]
    fn a_target_that_is_not_an_rtmp_url_never_becomes_a_payload() {
        for bad in [
            "https://live.example.org/app/key",
            "live.example.org/app/key",
            // Well-formed, but with an empty authority: no host to publish to.
            "rtmp:///app/key",
            "rtmp://",
        ] {
            assert_eq!(
                Setting::RTMPStream1PublishURL.instruction(bad).unwrap_err(),
                ValueError::NotAnRtmpUrl(bad.into()),
                "{bad} should not have encoded"
            );
        }
    }

    /// `rtmps` is the same target over TLS, and the scheme is the only part of
    /// the value that may be spelled in any case — a case-folded stream key is a
    /// stream that authenticates against nothing.
    #[test]
    fn the_scheme_is_case_insensitive_and_the_stream_key_is_left_alone() {
        let instr = Setting::RTMPStream2BackupPublishURL
            .instruction("RTMPS://live.example.org/app/MixedCaseKey")
            .unwrap();
        assert!(
            instr
                .payload
                .contains("RTMPS://live.example.org/app/MixedCaseKey"),
            "payload rewrote the value: {:?}",
            instr.payload
        );
    }

    /// Clearing a push target is a normal operation — a stream that should no
    /// longer publish — so the empty value is the one non-URL the shape accepts.
    #[test]
    fn an_empty_value_clears_a_push_target() {
        let instr = Setting::RTMPStream3PublishURL.instruction("").unwrap();
        assert_eq!(instr.payload, "\u{1b}U1*3*RTMP\r");
    }

    /// A URL is still a framed value, so the `*` rule survives the extra layer.
    #[test]
    fn a_separator_cannot_enter_a_url_either() {
        assert_eq!(
            Setting::RTMPStream1PublishURL
                .instruction("rtmp://live.example.org/app/a*b")
                .unwrap_err(),
            ValueError::IllegalCharacter('*')
        );
    }

    #[test]
    fn an_rtmp_enable_write_folds_human_spellings_onto_the_wire_flag() {
        let instr = Setting::RTMPStream1State.instruction("on").unwrap();
        assert_eq!(instr.payload, "\u{1b}E1*1RTMP\r");
        assert_eq!(
            drive(&instr, "RtmpE1*1\r\n"),
            Step::Done(Value::Text("1".into()))
        );
        // A live-state reply is not an enable reply, even though both are a
        // flag under the same verb.
        assert_eq!(drive(&instr, "RtmpS1*1*1\r\n"), Step::NeedMore);
    }

    /// Arming a push stream is a write; putting it on air is not reachable over
    /// SIS at all — that is a scheduled session's doing. So `RTMP_1_STATE` is a
    /// setting and `RTMP_1_STREAM_STATE` is only ever a reading, and asking to
    /// write the latter has to fail as an unknown *setting* rather than
    /// half-succeed as a write of the former.
    #[test]
    fn a_live_state_is_not_writable() {
        use std::str::FromStr;

        for name in [
            "RTMP_1_STREAM_STATE",
            "RTMP_2_STREAM_STATE",
            "RTMP_3_STREAM_STATE",
            "RTMP_1_BACKUP_STREAM_STATE",
            "RTMP_2_BACKUP_STREAM_STATE",
            "RTMP_3_BACKUP_STREAM_STATE",
        ] {
            assert!(
                Setting::from_str(name).is_err(),
                "{name} is writable; SIS has no write for a live state"
            );
        }
    }

    /// The whole writable RTMP address table in one place, pinned against the
    /// SIS reference: `E<n>` enables a stream and `U<i>*<n>` is a publish URL,
    /// where `i` selects primary (1) or backup (2). The read-only `S<i>*<n>`
    /// live states are pinned beside their queries in `protocol.rs`.
    ///
    /// Worth pinning as a table rather than leaving to the per-field tests
    /// because the failure mode is a *swap* — `U2*1` where `U1*2` was meant is
    /// two valid addresses for two real fields, so it encodes cleanly, parses
    /// cleanly, and writes the right value to the wrong stream.
    #[test]
    fn rtmp_addresses_follow_the_field_stream_scheme() {
        for (setting, addr) in [
            (Setting::RTMPStream1State, "E1"),
            (Setting::RTMPStream2State, "E2"),
            (Setting::RTMPStream3State, "E3"),
            (Setting::RTMPStream1PublishURL, "U1*1"),
            (Setting::RTMPStream2PublishURL, "U1*2"),
            (Setting::RTMPStream3PublishURL, "U1*3"),
            (Setting::RTMPStream1BackupPublishURL, "U2*1"),
            (Setting::RTMPStream2BackupPublishURL, "U2*2"),
            (Setting::RTMPStream3BackupPublishURL, "U2*3"),
        ] {
            assert_eq!(
                setting.form(),
                Form::Addressed(addr),
                "{setting} is not addressed {addr}"
            );
            assert_eq!(
                setting.verb(),
                "RTMP",
                "{setting} does not write through RTMP"
            );
        }
    }

    /// The read and the write of a stream field are one name, which is what
    /// makes `GET .../fields/STREAM_1_NAME` and `PUT .../settings/STREAM_1_NAME`
    /// address the same thing.
    ///
    /// The RTMP settings are absent by design rather than by oversight: only the
    /// live-state read form is attested, and a `Query` added on a guess is
    /// polled on every sync cycle. See the note on
    /// [`Query::instruction`](super::query::Query::instruction).
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

    /// Aliases have to pair up too, not just canonical names: a caller who
    /// reads `GET .../fields/RTMP_STREAM_STATE_1` and then writes
    /// `PUT .../settings/RTMP_STREAM_STATE_1` must reach the same field, and a
    /// spelling accepted on one side only is a 404 that looks like a typo.
    #[test]
    fn a_paired_field_accepts_the_same_spellings_on_both_sides() {
        use crate::protocol::instructions::query::Query;
        use std::str::FromStr;

        for setting in Setting::ALL {
            let Ok(query) = Query::from_str(setting.name()) else {
                continue; // Write-only field; the test above governs the pairs.
            };
            assert_eq!(
                setting.accepted(),
                query.accepted(),
                "{setting} and its query disagree about accepted spellings"
            );
        }
    }
}
