// ---- Command enum ---------------------------------------------------------

use winnow::Parser;
use winnow::token::literal;

use crate::protocol::control_chars::{CR, ESC, RCDR, RCDR_LOWER};
use crate::protocol::instructions::{Instruction, Session};
use crate::protocol::payload_helpers::echoed;
use crate::protocol::{In, ParseFn, Value, parser_of};
use sismatic_catalog_macros::catalog;

/// A built-in valueless device command: the recorder verbs, plus `REBOOT`.
#[catalog]
pub enum Command {
    /// Start recording.
    #[name("STARTRECORDING")]
    #[alias("START_RECORDING")]
    #[alias("START")]
    Start,
    /// Stop recording.
    #[name("STOPRECORDING")]
    #[alias("STOP_RECORDING")]
    #[alias("STOP")]
    Stop,
    /// Pause recording.
    #[name("PAUSERECORDING")]
    #[alias("PAUSE_RECORDING")]
    #[alias("PAUSE")]
    Pause,
    /// Restart the device. Ends any recording in progress and the SSH session.
    #[alias("BOOT")]
    #[alias("RESTART")]
    Reboot,
}

/// How a command is framed on the wire. One arm per frame the device uses, so
/// the payload and its echo are derived from the same strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Frame {
    /// `ESC ⟨verb⟩ RCDR CR` → `Rcdr⟨verb⟩ CR LF`.
    Recorder(&'static str),
    /// `ESC ⟨arg⟩ ⟨word⟩ CR` → `echoed(word)⟨arg⟩ CR LF`, e.g. `1BOOT` → `Boot1`.
    Prefixed {
        arg: &'static str,
        word: &'static str,
    },
}

impl Command {
    /// The frame this command is sent in. Exhaustive on purpose: a new command
    /// cannot compile without choosing its framing.
    pub(crate) fn frame(self) -> Frame {
        match self {
            Command::Start => Frame::Recorder("Y1"),
            Command::Stop => Frame::Recorder("Y0"),
            Command::Pause => Frame::Recorder("Y2"),
            Command::Reboot => Frame::Prefixed {
                arg: "1",
                word: "BOOT",
            },
        }
    }

    /// What this command does to the session it is sent on.
    pub fn session(self) -> Session {
        match self {
            Command::Start | Command::Stop | Command::Pause => Session::Preserved,
            Command::Reboot => Session::Ends,
        }
    }

    /// The token the device answers with, without the trailing CR LF.
    /// Crate-visible so the simulator echoes it back rather than re-spelling
    /// the token in a second place.
    pub(crate) fn echo_token(self) -> String {
        match self.frame() {
            Frame::Recorder(verb) => format!("{RCDR_LOWER}{verb}"),
            Frame::Prefixed { arg, word } => format!("{}{arg}", echoed(word)),
        }
    }

    /// Build the wire instruction for this command.
    pub fn instruction(self) -> Instruction {
        let payload = match self.frame() {
            Frame::Recorder(verb) => format!("{ESC}{verb}{RCDR}{CR}"),
            Frame::Prefixed { arg, word } => format!("{ESC}{arg}{word}{CR}"),
        };
        Instruction {
            name: self.name().to_string(),
            payload,
            session: self.session(),
            parser: echo_ack(self.echo_token()),
        }
    }
}

/// Echo after a command: `⟨token⟩ CR LF`, surfaced as [`Value::Ack`].
fn echo_ack(token: String) -> ParseFn {
    parser_of(
        move |i: &mut In| {
            literal(token.as_str()).parse_next(i)?;
            literal("\r\n").parse_next(i)?;
            Ok(token.clone())
        },
        Value::Ack,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Step;

    #[test]
    fn the_recorder_echo_is_the_title_cased_verb_word() {
        assert_eq!(echoed(RCDR), RCDR_LOWER);
    }

    #[test]
    fn reboot_is_framed_as_esc_1boot_cr() {
        assert_eq!(Command::Reboot.instruction().payload, "\u{1b}1BOOT\r");
    }

    #[test]
    fn reboot_acknowledges_boot1() {
        let instr = Command::Reboot.instruction();
        assert_eq!(
            instr.parse_step("Boot1\r\n"),
            Step::Done(Value::Ack("Boot1".into()))
        );
        // A leading request echo is skipped by the sliding search.
        assert_eq!(
            instr.parse_step("\u{1b}1BOOT\rBoot1\r\n"),
            Step::Done(Value::Ack("Boot1".into()))
        );
        assert_eq!(instr.parse_step("Boot1\r"), Step::NeedMore);
    }

    #[test]
    fn only_reboot_ends_the_session() {
        for &c in Command::ALL {
            let expected = if c == Command::Reboot {
                Session::Ends
            } else {
                Session::Preserved
            };
            assert_eq!(c.instruction().session(), expected, "{c}");
        }
    }
}
