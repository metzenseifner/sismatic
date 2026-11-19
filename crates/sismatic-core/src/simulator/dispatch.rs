//! Decoding inbound SIS requests back into catalog instructions.
//!
//! Every byte pattern recognised here is produced by the same `instruction()`
//! constructors the client encodes with, so a payload change in
//! [`query`](crate::protocol::instructions::query),
//! [`commands`](crate::protocol::instructions::commands),
//! [`register`](crate::protocol::instructions::register) or
//! [`setting`](crate::protocol::instructions::setting) moves both sides at
//! once. There is no table to forget to update — only `ALL` and the encoders.

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
    /// Distinct from [`Set`](Request::Set) because the protocol is too: a
    /// register write is addressed to the recorder subsystem (`RCDR`, an `M<i>`
    /// address, a `*` before the value) and a setting write to the device
    /// itself, with none of those. They also differ in policy — metadata writes
    /// take a recording freeze, settings do not.
    Configure(Setting, String),
}

impl Request {
    /// The canonical field name this request addresses, or `None` for one that
    /// addresses no field.
    ///
    /// Recording commands are the `None` arm: `START` is not a field, so it has
    /// no license to check and no cell to look up. The other three answer with
    /// the canonical name, which is what makes one license check cover reads
    /// and writes alike — a query, a register and a setting of the same field
    /// all give the same string.
    pub fn field(&self) -> Option<&'static str> {
        match self {
            Request::Get(q) => Some(q.name()),
            Request::Set(r, _) => Some(r.name()),
            Request::Configure(s, _) => Some(s.name()),
            Request::Do(_) => None,
        }
    }
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

/// Every register's write *prefix*: a write embeds its value
/// (`ESC M<i> * <value> RCDR CR`), so these match by prefix, not equality.
static SETS: LazyLock<Vec<(String, Register)>> = LazyLock::new(|| {
    Register::ALL
        .iter()
        .map(|&r| (write_prefix(r), r))
        .collect()
});

/// Every setting's write *frame*: the fixed bytes either side of the value.
/// A setting puts its value in the middle (`ESC <value> <verb> CR`), so —
/// unlike a register — a prefix alone cannot identify one.
static CONFIGURES: LazyLock<Vec<(String, String, Setting)>> = LazyLock::new(|| {
    Setting::ALL
        .iter()
        .map(|&s| {
            let (head, tail) = write_frame(s);
            (head, tail, s)
        })
        .collect()
});

/// The fixed head of a register write, recovered from the catalog's own
/// encoder: `Register::instruction("")` yields `ESC M<i> * RCDR CR`, and
/// everything before the trailing `RCDR CR` is the prefix. Re-formatting
/// `M{index}` here would be a second place to keep in step with
/// [`Register::index`].
fn write_prefix(r: Register) -> String {
    let payload = r.instruction("").payload;
    payload
        .strip_suffix(&tail())
        .expect("a register write payload ends with RCDR CR")
        .to_string()
}

/// The head and tail of a setting write, taken from the catalog's own
/// [`Setting::write_frame`].
///
/// [`write_prefix`] can recover a register's framing by encoding and stripping,
/// because the address (`M<i>`) is followed by a `*` the value can never
/// contain. A setting's cannot be recovered that way — see the note on
/// `Setting::write_frame` — so the catalog publishes the frame rather than this
/// module guessing at it.
fn write_frame(s: Setting) -> (String, String) {
    s.write_frame()
}

/// Decode one request. `None` for anything outside the catalog, which the
/// caller answers with silence — as a real device ignores a malformed request
/// and the client sees a command timeout.
///
/// Assumes the request arrives as a single write, which holds over loopback.
///
/// Register prefixes stay unambiguous because of the `*`: `M1` cannot swallow a
/// write to `M16`, since the byte after `M1` is `6`. Settings are matched on
/// head *and* tail together, a pair no other setting's write fits
/// (`no_setting_frame_accepts_another_settings_write`).
///
/// # Why queries are tried first
///
/// A setting write with an *empty* value is byte-for-byte the read of the same
/// field: both are `ESC <verb> CR`. That is the protocol's ambiguity, not this
/// function's — real hardware cannot tell them apart either and answers with
/// the field's value, which is what trying queries first reproduces.
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

    /// Also pins the prefix disambiguation: `M1` and `M10`..`M16` share a stem,
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

    /// A value `s` accepts, found by asking the catalog's own encoder rather
    /// than keeping a shape → value table here. No single value covers every
    /// shape: a flag takes `1` and refuses a URL, a URL the reverse.
    fn probe(s: Setting) -> String {
        ["1", "rtmp://cdn.example.edu/live/probe"]
            .into_iter()
            .find(|v| s.instruction(v).is_ok())
            .unwrap_or_else(|| panic!("no probe value for {s}: extend the ladder"))
            .to_owned()
    }

    /// Every setting's own write, recovered as itself with its value intact.
    ///
    /// The property that matters, and it subsumes the frame check below:
    /// `classify` returns the *first* frame that matches, so every setting
    /// matching itself means no setting's frame shadows another's.
    #[test]
    fn every_setting_write_classifies_back_with_its_value() {
        for &s in Setting::ALL {
            let value = probe(s);
            let payload = s.instruction(&value).expect("the probe is legal").payload;
            assert_eq!(
                classify(&payload),
                Some(Request::Configure(s, value)),
                "{s} did not classify back to itself"
            );
        }
    }

    /// A value containing the other catalogs' delimiters, so the value is
    /// recovered whole rather than cut at the first thing that looks like
    /// framing.
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

    /// What makes matching a setting by its frame sound: no setting's frame
    /// accepts another setting's write.
    ///
    /// The discriminator is the *pair*. Six stream settings share the verb
    /// `STRC`, so they share a tail and are told apart by their heads alone
    /// (`ESC N1*` names stream 1, `ESC 1*` enables it); the `Prefixed` families
    /// are the mirror image, sharing the bare `ESC` head and differing only in
    /// verb. So this checks head-as-prefix *and* tail-as-suffix, the way
    /// `classify` does, rather than either half alone.
    #[test]
    fn no_setting_frame_accepts_another_settings_write() {
        for &a in Setting::ALL {
            let payload = a
                .instruction(&probe(a))
                .expect("the probe is legal")
                .payload;
            for &b in Setting::ALL {
                if a == b {
                    continue;
                }
                let (head, tail) = write_frame(b);
                let matches = payload
                    .strip_prefix(head.as_str())
                    .and_then(|rest| rest.strip_suffix(tail.as_str()))
                    .is_some();
                assert!(
                    !matches,
                    "a write to {a} fits {b}'s frame ({head:?}, {tail:?}), \
                     so one would classify as the other"
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
