//! Decoding inbound SIS requests back into catalog instructions.
//!
//! The simulator never spells a wire payload of its own. Every byte pattern it
//! recognises is produced by the same `instruction()` constructors the client
//! encodes with, so a change to a payload in
//! [`query`](crate::protocol::instructions::query),
//! [`commands`](crate::protocol::instructions::commands) or
//! [`register`](crate::protocol::instructions::register) moves the client and
//! the simulator together by construction. There is no table here to forget to
//! update — only [`Query::ALL`], [`Command::ALL`] and [`Register::ALL`].

use std::sync::LazyLock;

use crate::protocol::control_chars::{CR, RCDR};
use crate::protocol::instructions::commands::Command;
use crate::protocol::instructions::query::Query;
use crate::protocol::instructions::register::Register;

/// A request the device understands, decoded from the raw channel bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Read a field.
    Get(Query),
    /// Write `value` into a metadata register.
    Set(Register, String),
    /// Run a recording command.
    Do(Command),
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

/// Decode one request. Returns `None` for anything outside the catalog, which
/// the caller answers with silence — the same way a real device ignores a
/// malformed request and the client sees a command timeout.
///
/// Assumes the request arrives as a single write, which holds over loopback.
///
/// The `*` delimiter keeps register prefixes unambiguous: `M1` cannot swallow a
/// write to `M16`, because the byte after `M1` is `6`, not `*`.
pub fn classify(request: &str) -> Option<Request> {
    if let Some(&(_, q)) = GETS.iter().find(|(payload, _)| payload == request) {
        return Some(Request::Get(q));
    }
    if let Some(&(_, c)) = DOES.iter().find(|(payload, _)| payload == request) {
        return Some(Request::Do(c));
    }
    let tail = tail();
    SETS.iter().find_map(|(prefix, r)| {
        let rest = request.strip_prefix(prefix.as_str())?;
        let value = rest.strip_suffix(tail.as_str())?;
        Some(Request::Set(*r, value.to_string()))
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

    #[test]
    fn unknown_requests_are_rejected() {
        assert_eq!(classify("not a sis request"), None);
        assert_eq!(classify(""), None);
    }
}
