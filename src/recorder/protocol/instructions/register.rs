// ---- Register (settable) enum ---------------------------------------------

use crate::recorder::protocol::payload_helpers::is_not_cr;
use std::{fmt, str::FromStr};

use winnow::{
    Parser,
    token::{literal, take_while},
};

use crate::recorder::protocol::{
    In, ParseFn, Value,
    control_chars::{CR, ESC, RCDR, RCDR_LOWER},
    instructions::{Instruction, UnknownInstruction},
    parser_of,
    payload_helpers::{normalize, shorten},
};

//
// ---- Register / token constants from the SIS protocol ---------------------

/// A built-in metadata register that can be written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Register {
    Contributor,
    Course,
    Coverage,
    Creator,
    Description,
    Format,
    Language,
    Presenter,
    Publisher,
    Relation,
    Rights,
    Source,
    Subject,
    SystemName,
    Title,
    Type,
}

impl Register {
    pub const ALL: &'static [Register] = &[
        Register::Contributor,
        Register::Course,
        Register::Coverage,
        Register::Creator,
        Register::Description,
        Register::Format,
        Register::Language,
        Register::Presenter,
        Register::Publisher,
        Register::Relation,
        Register::Rights,
        Register::Source,
        Register::Subject,
        Register::SystemName,
        Register::Title,
        Register::Type,
    ];

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
            _ => unimplemented!("confirm index"),
        }
    }

    fn name(self) -> String {
        format!("{self:?}").to_uppercase()
    }

    /// `M`-prefixed register address, derived from [`index`].
    fn reg(self) -> String {
        format!("M{}", self.index())
    }

    /// Build the wire instruction that writes `value` into this register. The
    /// value is truncated to [`MAX_VALUE_LEN`] characters, matching the device.
    pub fn instruction(self, value: &str) -> Instruction {
        let reg = self.reg();
        let value = shorten(value, MAX_VALUE_LEN);
        let payload = format!("{ESC}{reg}*{value}{RCDR}{CR}");
        Instruction {
            name: self.name().to_string(),
            payload,
            parser: settable_echo(&reg),
        }
    }
}

impl fmt::Display for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

impl FromStr for Register {
    type Err = UnknownInstruction;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match normalize(s).as_str() {
            "CONTRIBUTOR" => Ok(Self::Contributor),
            "COURSE" => Ok(Self::Course),
            "COVERAGE" => Ok(Self::Coverage),
            "CREATOR" => Ok(Self::Creator),
            "DESCRIPTION" => Ok(Self::Description),
            "FORMAT" => Ok(Self::Format),
            "LANGUAGE" => Ok(Self::Language),
            "PRESENTER" => Ok(Self::Presenter),
            "PUBLISHER" => Ok(Self::Publisher),
            "RELATION" => Ok(Self::Relation),
            "RIGHTS" => Ok(Self::Rights),
            "SOURCE" => Ok(Self::Source),
            "SUBJECT" => Ok(Self::Subject),
            "SYSTEMNAME" => Ok(Self::SystemName),
            "TITLE" => Ok(Self::Title),
            "TYPE" => Ok(Self::Type),
            _ => Err(UnknownInstruction(s.to_string())),
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
            literal("\r").parse_next(i)?;
            Ok(v.to_string())
        },
        Value::Text,
    )
}
