// ---- Command enum ---------------------------------------------------------

use winnow::Parser;
use winnow::token::literal;

use crate::protocol::control_chars::{CR, ESC, RCDR, RCDR_LOWER};
use crate::protocol::instructions::Instruction;
use crate::protocol::{In, ParseFn, Value, parser_of};
use sismatic_catalog_macros::catalog;

/// A built-in recorder command.
#[catalog]
pub enum Command {
    /// Start recording.
    #[name("STARTRECORDING")]
    #[alias("START")]
    Start,
    /// Stop recording.
    #[name("STOPRECORDING")]
    #[alias("STOP")]
    Stop,
    /// Pause recording.
    #[name("PAUSERECORDING")]
    #[alias("PAUSE")]
    Pause,
}

impl Command {
    /// The SIS verb this command sends. Crate-visible so the simulator can echo
    /// it back rather than re-spelling the token in a second place.
    pub(crate) fn verb(self) -> &'static str {
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

/// Echo after a recording command: `Rcdr<verb> CR`.
fn command_echo(verb: &str) -> ParseFn {
    let token = format!("{RCDR_LOWER}{verb}");
    parser_of(
        move |i: &mut In| {
            literal(token.as_str()).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            Ok(token.clone())
        },
        Value::Ack,
    )
}
