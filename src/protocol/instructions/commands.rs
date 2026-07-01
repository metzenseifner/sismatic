// ---- Command enum ---------------------------------------------------------

use std::{fmt, str::FromStr};

use winnow::Parser;
use winnow::token::literal;

use crate::protocol::control_chars::{CR, ESC, RCDR, RCDR_LOWER};
use crate::protocol::instructions::{Instruction, UnknownInstruction};
use crate::protocol::payload_helpers::normalize;
use crate::protocol::{In, ParseFn, Value, parser_of};

/// A built-in recorder command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Start,
    Stop,
    Pause,
}

impl Command {
    pub const ALL: &'static [Command] = &[Command::Start, Command::Stop, Command::Pause];

    pub fn name(self) -> &'static str {
        match self {
            Command::Start => "STARTRECORDING",
            Command::Stop => "STOPRECORDING",
            Command::Pause => "PAUSERECORDING",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Command::Start => "Y1",
            Command::Stop => "Y0",
            Command::Pause => "Y2",
        }
    }
    /// Build the wire instruction for this command.
    pub fn instruction(self) -> Instruction {
        let verb = self.verb();
        Instruction {
            name: self.name().to_string(),
            payload: format!("{ESC}{verb}{RCDR}{CR}"),
            parser: command_echo(verb),
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Command {
    type Err = UnknownInstruction;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match normalize(s).as_str() {
            "START" | "STARTRECORDING" => Ok(Command::Start),
            "STOP" | "STOPRECORDING" => Ok(Command::Stop),
            "PAUSE" | "PAUSERECORDING" => Ok(Command::Pause),
            _ => Err(UnknownInstruction(s.to_string())),
        }
    }
}
///
/// Echo after a recording command: `Rcdr<verb> CR`.
fn command_echo(verb: &str) -> ParseFn {
    let token = format!("{RCDR_LOWER}{verb}");
    parser_of(
        move |i: &mut In| {
            literal(token.as_str()).parse_next(i)?;
            literal("\r").parse_next(i)?;
            Ok(token.clone())
        },
        Value::Ack,
    )
}
