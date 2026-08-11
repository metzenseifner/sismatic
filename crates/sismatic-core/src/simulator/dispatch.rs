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

impl Request {
    /// The canonical field name this request addresses, or `None` for a
    /// request that addresses no field.
    ///
    /// Recording commands are the `None` arm: `START` is not a field, so it has
    /// no license to check and no cell to look up. The three field-bearing
    /// variants all answer with the canonical name, which is what makes one
    /// license check cover reads and writes alike — a register, a setting and a
    /// query of the same field give the same string.
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

/// The head and tail of a setting write, taken from the catalog's own
/// [`Setting::write_frame`] rather than inferred here, for the same reason
/// [`write_prefix`] reads its prefix out of a real payload: the framing stays
/// in `setting.rs` and this file cannot drift from it.
///
/// A register's prefix can be recovered by encoding and stripping, because its
/// address (`M<i>`) is followed by a `*` that the value can never contain. A
/// setting's cannot — see the note on `Setting::write_frame` — so the catalog
/// publishes the frame instead of this module guessing at it.
fn write_frame(s: Setting) -> (String, String) {
    s.write_frame()
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

    /// A value `s` accepts, found by asking the catalog's own encoder rather
    /// than keeping a shape → value table here, which would be the drift this
    /// module exists to avoid. No single value works for every shape: a flag
    /// takes `1` and refuses a URL, a URL takes `rtmp://…` and refuses `1`.
    fn probe(s: Setting) -> String {
        ["1", "rtmp://cdn.example.edu/live/probe"]
            .into_iter()
            .find(|v| s.instruction(v).is_ok())
            .unwrap_or_else(|| panic!("no probe value for {s}: extend the ladder"))
            .to_owned()
    }

    /// Every setting's own write, recovered as itself with its value intact.
    ///
    /// This is the property that matters, and it subsumes the frame checks
    /// below: `classify` returns the *first* frame that matches, so every
    /// setting matching itself means no setting's frame shadows another's.
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

    /// What makes matching a setting by its frame sound: no setting's frame
    /// accepts another setting's write.
    ///
    /// The discriminator is the *pair*, not the tail alone. Six stream settings
    /// share the verb `STRC` and so share a tail, and are told apart entirely by
    /// their heads (`ESC N1*` names stream 1, `ESC 1*` enables it); the three
    /// `Prefixed` families are the mirror image, sharing the bare `ESC` head and
    /// differing only in verb. So the property is checked the way `classify`
    /// actually tests it — head as a prefix *and* tail as a suffix — rather than
    /// on either half on its own.
    ///
    /// The `M1`/`M16` note on [`classify`] is this property for registers, kept
    /// true by the `*`.
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
