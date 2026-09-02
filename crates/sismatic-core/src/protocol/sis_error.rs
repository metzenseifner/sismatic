//! The one reply shape that is not an answer: a SIS error token.
//!
//! Every SIS verb can be refused, and a refusal looks nothing like the value it
//! replaces — the device sends `E` and two digits on a line of its own, with no
//! echo of what was asked. That makes it invisible to an instruction's parser:
//! a parser is written to recognise *its own* reply, so an error token is just
//! bytes that never complete, and the exchange reads until `command_timeout`.
//!
//! # Why this lives outside the instruction catalog
//!
//! A refusal is a property of the protocol, not of the instruction refused. The
//! token is identical whichever verb drew it, so teaching all ~50 catalog
//! parsers to recognise it would be the same clause written fifty times, and a
//! parser added later would silently not have it. [`Controller`] checks for it
//! once, before the instruction's own parser runs, which also means
//! [`Instruction::custom`] users get it without asking.
//!
//! [`Controller`]: crate::devices::controller::Controller
//! [`Instruction::custom`]: crate::protocol::instructions::Instruction::custom

use std::fmt;

/// A SIS error reply, as the two-digit code the device sent.
///
/// The code is kept as a number rather than resolved to an enum at parse time
/// so an undocumented code still round-trips: an operator who sees `E42` in a
/// log can look it up in the model's SIS reference, which is better than the
/// catalog having silently mapped it onto something it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SisError {
    /// The two digits following the `E`, e.g. `13` for `E13`.
    pub code: u8,
}

impl SisError {
    /// The documented meaning of this code, if it is one the device publishes.
    ///
    /// These ten are the error set the SMP 351 SIS reference documents, worded
    /// as it words them. Deliberately not a wider table copied from other
    /// Extron models: a code this device never sends would be a guess presented
    /// in the same voice as an attested fact, and the [`None`] arm below already
    /// handles anything outside the list without losing the code itself.
    pub const fn description(self) -> Option<&'static str> {
        Some(match self.code {
            10 => "unrecognized command",
            12 => "invalid port number",
            13 => "invalid parameter (number is out of range)",
            14 => "not valid for this configuration",
            17 => "invalid command for signal type",
            18 => "system timed out",
            22 => "busy",
            24 => "privilege violation",
            26 => "maximum connections exceeded",
            28 => "bad file name or file not found",
            _ => return None,
        })
    }

    /// Find an error token in the accumulated reply buffer.
    ///
    /// # Anchoring
    ///
    /// The token has to be a *whole* line, and the line has to be *complete*.
    /// Both halves matter and for different reasons:
    ///
    /// - Whole-line, because the buffer is searched at every line rather than
    ///   only the first (a device may echo before it answers, which is why
    ///   [`search`](crate::protocol) slides too). A substring match would then
    ///   read a stream named `HALL E13 CAMERA` as a refusal.
    /// - Complete, because this runs on a partial buffer after every read. A
    ///   reply of `SUITE E13\r\n` arriving in fragments passes through the state
    ///   `…\r\nE13` — a whole line by any measure except that its terminator has
    ///   not arrived, and treating it as one would turn a fragment boundary into
    ///   a refusal.
    ///
    /// # The ambiguity that remains
    ///
    /// A free-text field whose value is *exactly* `E13` is indistinguishable
    /// from a refusal, because SIS sends both as a bare line. That is the
    /// protocol's ambiguity, not this function's, and it is resolved the way
    /// that loses least: a stream nobody named `E13` beats a `command_timeout`
    /// on every poll of a field the device refuses.
    pub fn in_reply(buffer: &str) -> Option<Self> {
        // Everything up to and including the last terminator; a trailing
        // fragment is not a line yet.
        let complete = &buffer[..buffer.rfind("\r\n")? + 2];
        complete.split_terminator("\r\n").find_map(Self::of_line)
    }

    /// `E` followed by exactly two ASCII digits, and nothing else.
    fn of_line(line: &str) -> Option<Self> {
        let digits = line.strip_prefix('E')?;
        if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok().map(|code| SisError { code })
    }
}

impl fmt::Display for SisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `E13: …` rather than `E13 (…)`, because two of the documented
        // meanings carry parentheses of their own and nesting them reads as a
        // typo.
        write!(f, "E{:02}: ", self.code)?;
        match self.description() {
            Some(what) => f.write_str(what),
            None => f.write_str("undocumented error code"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_complete_error_line() {
        assert_eq!(SisError::in_reply("E13\r\n"), Some(SisError { code: 13 }));
        // Well-formed but outside the documented set: the grammar decides what
        // is a token, the table only decides what it is *called*.
        assert_eq!(SisError::in_reply("E01\r\n"), Some(SisError { code: 1 }));
    }

    /// The documented set, pinned. A device that answers one of these must not
    /// reach an operator as a bare number, and the wording is the reference's
    /// so a log line can be searched for in it.
    #[test]
    fn every_documented_code_resolves_to_its_published_meaning() {
        let documented = [
            (10, "unrecognized command"),
            (12, "invalid port number"),
            (13, "invalid parameter (number is out of range)"),
            (14, "not valid for this configuration"),
            (17, "invalid command for signal type"),
            (18, "system timed out"),
            (22, "busy"),
            (24, "privilege violation"),
            (26, "maximum connections exceeded"),
            (28, "bad file name or file not found"),
        ];
        for (code, meaning) in documented {
            let wire = format!("E{code:02}\r\n");
            let found = SisError::in_reply(&wire).expect("a well-formed token");
            assert_eq!(found, SisError { code }, "parsing {wire:?}");
            assert_eq!(found.description(), Some(meaning), "describing E{code:02}");
            assert_eq!(found.to_string(), format!("E{code:02}: {meaning}"));
        }
    }

    /// The buffer is fed in after every read, so every prefix of a real reply
    /// gets asked. Only the last one may answer.
    #[test]
    fn waits_for_the_terminator() {
        for partial in ["", "E", "E1", "E13", "E13\r"] {
            assert_eq!(SisError::in_reply(partial), None, "matched on {partial:?}");
        }
    }

    /// The device may echo, so later lines are searched too.
    #[test]
    fn finds_the_token_after_an_echo() {
        assert_eq!(
            SisError::in_reply("\u{1b}CN\r\nE13\r\n"),
            Some(SisError { code: 13 })
        );
    }

    /// The anchor's whole job: a value that merely *contains* the token is a
    /// value.
    #[test]
    fn does_not_match_a_token_inside_a_longer_line() {
        assert_eq!(SisError::in_reply("HALL E13 CAMERA\r\n"), None);
        assert_eq!(SisError::in_reply("E133\r\n"), None);
        assert_eq!(SisError::in_reply("E1\r\n"), None);
        assert_eq!(SisError::in_reply("XE13\r\n"), None);
    }

    /// A fragment boundary must not manufacture a line. `SUITE E13\r\n` passes
    /// through `SUITE E13` on its way in, and neither state is a refusal.
    #[test]
    fn a_split_reply_is_not_a_refusal_at_any_prefix() {
        let reply = "SUITE E13\r\n";
        for end in 0..reply.len() {
            assert_eq!(
                SisError::in_reply(&reply[..end]),
                None,
                "matched on prefix {:?}",
                &reply[..end]
            );
        }
        assert_eq!(SisError::in_reply(reply), None);
    }

    #[test]
    fn an_undocumented_code_still_parses_and_prints() {
        let err = SisError::in_reply("E42\r\n").expect("a well-formed token");
        assert_eq!(err.code, 42);
        assert_eq!(err.description(), None);
        assert_eq!(err.to_string(), "E42: undocumented error code");
    }

    #[test]
    fn displays_the_code_and_its_meaning() {
        assert_eq!(
            SisError { code: 13 }.to_string(),
            "E13: invalid parameter (number is out of range)"
        );
        // Zero-padded back to the wire spelling, not `E1`, so the log line and
        // the device's own output are the same string.
        assert_eq!(
            SisError { code: 1 }.to_string(),
            "E01: undocumented error code"
        );
    }
}
