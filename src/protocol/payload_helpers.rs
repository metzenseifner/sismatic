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
