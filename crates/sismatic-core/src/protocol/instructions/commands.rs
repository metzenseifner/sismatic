// ---- Command enum ---------------------------------------------------------

use winnow::Parser;
use winnow::token::literal;

use crate::protocol::control_chars::{CR, ESC, RCDR, RCDR_LOWER};
use crate::protocol::instructions::Instruction;
use crate::protocol::instructions::catalog::instruction_catalog;
use crate::protocol::{In, ParseFn, Value, parser_of};

instruction_catalog! {
    /// A built-in recorder command.
    pub enum Command {
        Start { name: "STARTRECORDING", aliases: ["START"], doc: "Start recording." },
        Stop { name: "STOPRECORDING", aliases: ["STOP"], doc: "Stop recording." },
        Pause { name: "PAUSERECORDING", aliases: ["PAUSE"], doc: "Pause recording." },
    }
}

impl Command {
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
