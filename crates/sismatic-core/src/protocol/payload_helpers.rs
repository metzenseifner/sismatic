use crate::protocol::control_chars::{CR, ESC, RCDR};
// ---- payload helpers ------------------------------------------------------
pub fn esc_cr(verb: &str) -> String {
    format!("{ESC}{verb}{CR}")
}

pub fn esc_rcdr(verb: &str) -> String {
    format!("{ESC}{verb}{RCDR}{CR}")
}

pub fn normalize(s: &str) -> String {
    s.to_ascii_uppercase().replace('-', "_")
}

pub fn shorten(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

pub fn is_not_cr(c: char) -> bool {
    c != CR
}

/// A verb as the device echoes it: title case, `STRC` -> `Strc`.
///
/// Derived rather than tabled because the device applies one rule to every verb
/// attested so far — the recorder's `RCDR` comes back as `Rcdr` (see
/// `control_chars::RCDR_LOWER`), stream control's `STRC` as `Strc`, and RTMP's
/// `RTMP` as `Rtmp`.
///
/// Shared by the read and the write side: a query that expects an echo-framed
/// reply (`RtmpS1*1*0`) and the write that answers with the same frame must
/// derive the anchor identically, or a reply parses for one and not the other.
pub fn echoed(verb: &str) -> String {
    let mut chars = verb.chars();
    match chars.next() {
        Some(first) => format!(
            "{}{}",
            first.to_ascii_uppercase(),
            chars.as_str().to_ascii_lowercase()
        ),
        None => String::new(),
    }
}
