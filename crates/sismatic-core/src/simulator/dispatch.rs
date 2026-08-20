//! Decoding inbound SIS requests back into catalog instructions.
//!
//! The simulator never spells a wire payload of its own. Every byte pattern it
//! recognises is produced by the same `instruction()` constructors the client
//! encodes with, so a change to a payload in
//! [`query`](crate::protocol::instructions::query),
//! [`commands`](crate::protocol::instructions::commands),
//! [`register`](crate::protocol::instructions::register) or
//! [`setting`](crate::protocol::instructions::setting) moves the client and
//! the simulator together by construction. There is no table here to forget to
//! update — only [`Query::ALL`], [`Command::ALL`], [`Register::ALL`] and
//! [`Setting::ALL`].

use std::sync::LazyLock;

use crate::protocol::control_chars::{CR, RCDR};
use crate::protocol::instructions::commands::Command;
use crate::protocol::instructions::query::Query;
use crate::protocol::instructions::register::Register;
use crate::protocol::instructions::setting::Setting;

/// A request the device understands, decoded from the raw channel bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Read a field.
    Get(Query),
    /// Write `value` into a metadata register.
    Set(Register, String),
    /// Run a recording command.
    Do(Command),
    /// Write `value` into a built-in device setting.
    ///
    /// Distinct from [`Set`](Request::Set) because the protocol keeps them
    /// distinct: a register write is addressed to the recorder subsystem
    /// (`RCDR`, an `M<i>` address, a `*` before the value), while a setting
    /// write is addressed to the device itself and carries none of those. They
    /// also differ in policy — the write path applies a recording freeze to
    /// metadata and not to settings — so collapsing them here would make the
    /// simulator unable to tell a caller which one it actually received.
    Configure(Setting, String),
}

/// The trailing `RCDR CR` shared by command and register-write payloads.
fn tail() -> String {
    format!("{RCDR}{CR}")
}

/// Every query's exact request payload, built once from the catalog.
static GETS: LazyLock<Vec<(String, Query)>> = LazyLock::new(|| {
    Query::ALL
        .iter()
        .map(|&q| (q.instruction().payload, q))
        .collect()
});

/// Every command's exact request payload, built once from the catalog.
static DOES: LazyLock<Vec<(String, Command)>> = LazyLock::new(|| {
    Command::ALL
        .iter()
        .map(|&c| (c.instruction().payload, c))
        .collect()
});

/// Every register's write *prefix*. A write embeds its value
/// (`ESC M<i> * <value> RCDR CR`), so these match by prefix, not equality.
static SETS: LazyLock<Vec<(String, Register)>> = LazyLock::new(|| {
    Register::ALL
        .iter()
        .map(|&r| (write_prefix(r), r))
        .collect()
});

/// Every setting's write *frame*: the fixed bytes before the value and the
/// fixed bytes after it. A setting embeds its value in the middle
/// (`ESC <value> <verb> CR`), so — unlike a register — a prefix alone cannot
/// identify one.
static CONFIGURES: LazyLock<Vec<(String, String, Setting)>> = LazyLock::new(|| {
    Setting::ALL
        .iter()
        .map(|&s| {
            let (head, tail) = write_frame(s);
            (head, tail, s)
        })
        .collect()
});

/// The fixed head of a register write, taken from the catalog's own encoder
/// rather than re-spelled here: `Register::instruction("")` yields
/// `ESC M<i> * RCDR CR`, and everything before the trailing `RCDR CR` is the
/// prefix. Re-formatting `M{index}` locally would be a second place that has to
/// stay in step with [`Register::index`].
fn write_prefix(r: Register) -> String {
    let payload = r.instruction("").payload;
    payload
        .strip_suffix(&tail())
        .expect("a register write payload ends with RCDR CR")
        .to_string()
}

/// The head and tail of a setting write, split out of the catalog's own encoder
/// rather than re-spelled here, for the same reason [`write_prefix`] is: the
/// verb and the `ESC`/`CR` framing stay in `setting.rs`, and this file cannot
/// drift from them.
///
/// The split is found by encoding a probe value and cutting the payload at it.
/// `1` is the probe because it is one of only two values *every*
/// `Shape` accepts — a flag rules out free text, a port rules out `on`/`off` —
/// which is also why `Setting::instruction("")` cannot be used the way
/// `Register::instruction("")` is: a port refuses an empty value.
///
/// Cutting at the *first* occurrence is what makes this sound: the value
/// precedes the verb, so even a verb that one day contains a `1` would be left
/// intact in the tail.
fn write_frame(s: Setting) -> (String, String) {
    const PROBE: &str = "1";
    let payload = s
        .instruction(PROBE)
        .expect("every setting shape accepts '1'; a new shape that does not must change this probe")
        .payload;
    let (head, tail) = payload
        .split_once(PROBE)
        .expect("a setting write payload carries its value verbatim");
    (head.to_string(), tail.to_string())
}

/// Decode one request. Returns `None` for anything outside the catalog, which
/// the caller answers with silence — the same way a real device ignores a
/// malformed request and the client sees a command timeout.
///
/// Assumes the request arrives as a single write, which holds over loopback.
///
/// The `*` delimiter keeps register prefixes unambiguous: `M1` cannot swallow a
/// write to `M16`, because the byte after `M1` is `6`, not `*`. Settings are
/// matched by their trailing verb instead, which is unambiguous for the
/// converse reason — no setting's frame ends the way another's does
/// (`no_setting_write_frame_ends_like_another`).
///
/// # Why queries are tried first
///
/// A setting write with an *empty* value is byte-for-byte the read of the same
/// field: both are `ESC <verb> CR`. That is the protocol's ambiguity, not this
/// function's — a real device cannot tell them apart either, and answers with
/// the field's value. Trying the queries first reproduces that.
pub fn classify(request: &str) -> Option<Request> {
    if let Some(&(_, q)) = GETS.iter().find(|(payload, _)| payload == request) {
        return Some(Request::Get(q));
    }
    if let Some(&(_, c)) = DOES.iter().find(|(payload, _)| payload == request) {
        return Some(Request::Do(c));
    }
    let tail = tail();
    let set = SETS.iter().find_map(|(prefix, r)| {
        let rest = request.strip_prefix(prefix.as_str())?;
        let value = rest.strip_suffix(tail.as_str())?;
        Some(Request::Set(*r, value.to_string()))
    });
    set.or_else(|| {
        CONFIGURES.iter().find_map(|(head, tail, s)| {
            let rest = request.strip_prefix(head.as_str())?;
            let value = rest.strip_suffix(tail.as_str())?;
            Some(Request::Configure(*s, value.to_string()))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_query_payload_classifies_back_to_its_own_variant() {
        for &q in Query::ALL {
            let payload = q.instruction().payload;
            assert_eq!(
                classify(&payload),
                Some(Request::Get(q)),
                "{q} did not classify back to itself"
            );
        }
    }

    #[test]
    fn every_command_payload_classifies_back_to_its_own_variant() {
        for &c in Command::ALL {
            let payload = c.instruction().payload;
            assert_eq!(
                classify(&payload),
                Some(Request::Do(c)),
                "{c} did not classify back to itself"
            );
        }
    }

    /// Also pins the prefix disambiguation: `M1` vs `M10`..`M16` share a stem,
    /// and only the `*` keeps a write to one from being read as the other.
    #[test]
    fn every_register_write_classifies_back_with_its_value() {
        for &r in Register::ALL {
            let payload = r.instruction("a value").payload;
            assert_eq!(
                classify(&payload),
                Some(Request::Set(r, "a value".into())),
                "{r} did not classify back to itself"
            );
        }
    }

    /// `1` is the one value every setting shape accepts, so it reaches every
    /// *name* in the catalog; the test below covers a value with more in it.
    #[test]
    fn every_setting_write_classifies_back_with_its_value() {
        for &s in Setting::ALL {
            let payload = s
                .instruction("1")
                .expect("'1' is accepted by every setting shape")
                .payload;
            assert_eq!(
                classify(&payload),
                Some(Request::Configure(s, "1".into())),
                "{s} did not classify back to itself"
            );
        }
    }

    /// A text value long enough to contain the other catalogs' delimiters,
    /// pinning that the value is recovered whole rather than cut at the first
    /// thing that looks like framing.
    #[test]
    fn a_text_setting_write_recovers_its_whole_value() {
        let value = "Europe/Vienna*M13 RCDR";
        let payload = Setting::Timezone
            .instruction(value)
            .expect("a timezone is free text")
            .payload;
        assert_eq!(
            classify(&payload),
            Some(Request::Configure(Setting::Timezone, value.into()))
        );
    }

    /// The protocol's one genuine ambiguity, pinned so a future reordering of
    /// `classify` cannot turn a read into a write. See its doc comment.
    #[test]
    fn an_empty_setting_write_is_the_read_of_the_same_field() {
        let payload = Setting::UnitName
            .instruction("")
            .expect("free text accepts an empty value")
            .payload;
        assert_eq!(classify(&payload), Some(Request::Get(Query::UnitName)));
    }

    /// What makes matching a setting by its trailing verb sound: no setting's
    /// write frame ends the way another's does. The `M1`/`M16` note on
    /// [`classify`] is the same property for registers, kept true by the `*`.
    #[test]
    fn no_setting_write_frame_ends_like_another() {
        for &a in Setting::ALL {
            for &b in Setting::ALL {
                if a == b {
                    continue;
                }
                let (_, a_tail) = write_frame(a);
                let (_, b_tail) = write_frame(b);
                assert!(
                    !a_tail.ends_with(&b_tail),
                    "a write to {a} ends like a write to {b}, so one would classify as the other"
                );
            }
        }
    }

    #[test]
    fn unknown_requests_are_rejected() {
        assert_eq!(classify("not a sis request"), None);
        assert_eq!(classify(""), None);
    }
}
