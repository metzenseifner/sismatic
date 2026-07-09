//! Control characters used by the SIS protocol.
//!
//! The SMP echoes the literal control bytes (not their printable escapes), so
//! both outgoing payloads and the regular expressions that match responses are
//! built from these raw characters.

/// Escape (0x1B). Prefixes most "settable"/"command" verbs in the SIS protocol.
pub const ESC: char = '\u{001b}';
/// Carriage return (0x0D). Terminates a command and appears throughout responses.
pub const CR: char = '\u{000d}';
/// Line feed (0x0A). The SMP appends an LF after the echoed CR of a message.
pub const LF: char = '\u{000a}';

pub const RCDR: &str = "RCDR";
pub const RCDR_LOWER: &str = "Rcdr";

/// Replace non-printable control characters with readable tokens for logging.
pub fn swap_non_printable(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            ESC => out.push_str("<ESC>"),
            CR => out.push_str("<CR>"),
            LF => out.push_str("<LF>"),
            _ => out.push(c),
        }
    }
    out
}
