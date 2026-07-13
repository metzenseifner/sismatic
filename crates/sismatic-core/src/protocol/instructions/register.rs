// ---- Register (settable) enum ---------------------------------------------

use crate::protocol::payload_helpers::is_not_cr;

use winnow::{
    Parser,
    token::{literal, take_while},
};

use crate::protocol::instructions::Instruction;
use crate::protocol::instructions::catalog::instruction_catalog;
use crate::protocol::{
    In, ParseFn, Value,
    control_chars::{CR, ESC, RCDR, RCDR_LOWER},
    parser_of,
    payload_helpers::shorten,
};

//
// ---- Register / token constants from the SIS protocol ---------------------

instruction_catalog! {
    /// A built-in metadata register that can be written to.
    pub enum Register {
        Contributor { name: "CONTRIBUTOR", aliases: [], doc: "Dublin Core 'contributor'." },
        Course { name: "COURSE", aliases: [], doc: "Course name." },
        Coverage { name: "COVERAGE", aliases: [], doc: "Dublin Core 'coverage'." },
        Description { name: "DESCRIPTION", aliases: [], doc: "Dublin Core 'description'." },
        Format { name: "FORMAT", aliases: [], doc: "Dublin Core 'format'." },
        Language { name: "LANGUAGE", aliases: [], doc: "Dublin Core 'language'." },
        Presenter { name: "PRESENTER", aliases: [], doc: "Presenter name." },
        Publisher { name: "PUBLISHER", aliases: [], doc: "Dublin Core 'publisher'." },
        Relation { name: "RELATION", aliases: [], doc: "Dublin Core 'relation'." },
        Rights { name: "RIGHTS", aliases: [], doc: "Dublin Core 'rights'." },
        Source { name: "SOURCE", aliases: [], doc: "Dublin Core 'source'." },
        Subject { name: "SUBJECT", aliases: [], doc: "Dublin Core 'subject'." },
        SystemName { name: "SYSTEMNAME", aliases: [], doc: "System name." },
        Title { name: "TITLE", aliases: [], doc: "Recording title." },
        Type { name: "TYPE", aliases: [], doc: "Dublin Core 'type'." },
    }
}

impl Register {
    pub fn index(self) -> u8 {
        match self {
            Register::Contributor => 0,
            Register::Coverage => 1,
            Register::Presenter => 2,
            // Register::Date => 3 // read-only exclusion
            Register::Description => 4,
            Register::Format => 5,
            // Register::Identifier => 6 // read-only exclusion
            Register::Language => 7,
            Register::Publisher => 8,
            Register::Relation => 9,
            Register::Rights => 10,
            Register::Source => 11,
            Register::Subject => 12,
            Register::Title => 13,
            Register::Type => 14,
            Register::SystemName => 15,
            Register::Course => 16,
        }
    }

    /// `M`-prefixed register address, derived from [`index`].
    fn reg(self) -> String {
        format!("M{}", self.index())
    }

    /// Build the wire instruction that writes `value` into this register. The
    /// value is truncated to [`MAX_VALUE_LEN`] characters, matching the device.
    #[tracing::instrument(
        name = "Building the wire instruction for writing value into register",
        level = "debug"
    )]
    pub fn instruction(self, value: &str) -> Instruction {
        let reg = self.reg();
        if value.len() > MAX_VALUE_LEN {
            tracing::warn!("Value exceeded 127 characters and will be curtailed to 127.")
        }
        let value = shorten(value, MAX_VALUE_LEN);
        let payload = format!("{ESC}{reg}*{value}{RCDR}{CR}");
        Instruction {
            name: self.name().to_string(),
            payload,
            parser: settable_echo(&reg),
        }
    }
}

/// Maximum value length the SMP 351 accepts for a settable register.
pub const MAX_VALUE_LEN: usize = 127;

/// Echo after writing a register: `Rcdr<reg>*<value> CR`.
fn settable_echo(reg: &str) -> ParseFn {
    let head = format!("{RCDR_LOWER}{reg}*");
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
