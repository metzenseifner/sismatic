//! Quantities an operator writes in words: `30d`, `1h 30min`, `512MiB`,
//! `2026-01-01`.
//!
//! Purely lexical. Nothing here knows what a duration is *for* — the sentinels
//! (`forever`, `never`, `unlimited`) belong to the settings that have them, and
//! each spells its own, because the word that means "switched off" is different
//! for a retention window than for a sweep interval. See
//! [`configuration`](crate::configuration) for where they are read.
//!
//! # Why the `[store]` section spells time differently from the rest
//!
//! Everywhere else in the config document a delay is a number with the unit in
//! its key: `interval_secs`, `poll_ms`. That works because those numbers are
//! seconds and milliseconds — quantities an operator holds in their head in one
//! unit and never converts.
//!
//! Retention is not like that. `2592000` is thirty days, and nobody reads it as
//! thirty days; they read it as a number they will have to check. The unit that
//! makes a retention window legible is the one it was decided in — a month, a
//! fortnight, an academic term — so the value carries its own unit and the key
//! stays `retain`. The same argument covers `max_memory`, where the readable
//! form is `512MiB` and the alternative is a nine-digit literal.
//!
//! Both spellings are accepted at every key here, so nothing is lost: a bare
//! integer is read as seconds for a duration and as bytes for a size, which is
//! also what makes these settings reachable from the environment, where
//! `config`'s type-guessing turns `300` into an integer before this crate ever
//! sees it.
//!
//! # Duration syntax
//!
//! [`humantime`]'s, which is a superset of the time spans a systemd unit file
//! accepts: a sequence of `<number><unit>` terms, summed, with optional
//! whitespace — `30d`, `2 weeks`, `1h 30min`, `1day 12h`. The units are `ns`,
//! `us`, `ms`, `s`/`sec`/`seconds`, `m`/`min`/`minutes`, `h`/`hr`/`hours`,
//! `d`/`days`, `w`/`weeks`, `M`/`months`, `y`/`years`, with `M` and `y` as
//! systemd defines them: an average month (30.44 days) and an average year
//! (365.25 days), not calendar arithmetic. A retention window is a rolling
//! measure of "how far back", so an average is the right thing for it to mean;
//! an operator who needs a calendar boundary writes the date instead.

use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};

/// Parse a duration written in words, or a bare integer read as seconds.
///
/// # Errors
///
/// Returns a message naming the offending text, phrased for a startup log an
/// operator reads once and has to act on without a manual.
pub fn duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();

    // Ahead of `humantime`, and not merely as a shortcut: it requires a unit,
    // so a bare `0` — the sentinel every other delay in this document uses —
    // would otherwise be a parse error rather than the disable it plainly is.
    if let Ok(secs) = text.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }

    humantime::parse_duration(text).map_err(|e| {
        format!("'{text}' is not a duration ({e}); write it as e.g. 30d, 2 weeks, 1h 30min, or a plain number of seconds")
    })
}

/// Parse a byte size written in words, or a bare integer read as bytes.
///
/// The two families of suffix mean what they say, which is the only rule that
/// avoids surprising either camp: `KiB`/`MiB`/`GiB`/`TiB` are powers of 1024,
/// `KB`/`MB`/`GB`/`TB` are powers of 1000, and the bare `K`/`M`/`G`/`T` are
/// 1024 — the reading `systemd`'s `MemoryMax=` gives them, which is where an
/// operator sizing a service most recently saw them. Case-insensitive, since no
/// two units here differ only by case.
///
/// # Errors
///
/// Returns a message naming the offending text and listing the suffixes.
pub fn bytes(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let bad = |what: &str| {
        format!(
            "'{text}' is not a size ({what}); write it as e.g. 512MiB, 2GB, or a plain number of bytes"
        )
    };

    // The unit is whatever follows the number, so the split is at the first
    // character that could not be part of one.
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);

    let scale = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1_u64,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        other => return Err(bad(&format!("unknown unit '{other}'"))),
    };

    // A decimal is worth accepting because the readable form of a size often is
    // one — `1.5GiB` says what `1610612736` does not. Integers keep the exact
    // path so a byte-precise budget stays byte-precise.
    if number.contains('.') {
        let value: f64 = number.parse().map_err(|_| bad("not a number"))?;
        if !value.is_finite() || value < 0.0 {
            return Err(bad("not a positive number"));
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a budget is an estimate enforced against an estimate, and \
                      f64 is exact to 2^53 — eight petabytes, well past any cap \
                      a machine could honour"
        )]
        let scaled = value * scale as f64;
        #[expect(
            clippy::cast_precision_loss,
            reason = "the bound only has to be conservative, and rounding \
                      u64::MAX up to the nearest f64 keeps it so"
        )]
        if scaled > u64::MAX as f64 {
            return Err(bad("too large"));
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "non-negative by the check above the multiply, and bounded \
                      above by the one on the line before"
        )]
        Ok(scaled as u64)
    } else {
        let value: u64 = number.parse().map_err(|_| bad("not a number"))?;
        value.checked_mul(scale).ok_or_else(|| bad("too large"))
    }
}

/// Render a byte count as text [`bytes`] reads back as the same number.
///
/// The inverse of the parser above, and only ever the *exact* inverse: a figure
/// that is a whole number of some binary unit is written with that unit, and
/// anything else is written as plain bytes. There is no rounding and no `1.5GiB`
/// — approximation is fine in a status line and wrong here, because what this
/// renders is fed straight back in. `GET /v1/config` returns a document that is
/// a valid `PATCH` body, and this is one of the four values that promise rests
/// on.
///
/// Binary units rather than decimal, because those are the ones the bare
/// suffixes mean and the ones a `MemoryMax=` beside this config is written in.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    // Largest first, so 1 GiB is `1GiB` rather than `1024MiB`.
    const UNITS: [(u64, &str); 4] = [
        (1 << 40, "TiB"),
        (1 << 30, "GiB"),
        (1 << 20, "MiB"),
        (1 << 10, "KiB"),
    ];

    UNITS
        .iter()
        .find(|(scale, _)| bytes >= *scale && bytes.is_multiple_of(*scale))
        .map_or_else(
            || bytes.to_string(),
            |(scale, suffix)| format!("{}{suffix}", bytes / scale),
        )
}

/// Parse an absolute instant: RFC 3339, or a bare `YYYY-MM-DD` read as midnight
/// UTC.
///
/// The date-only form is not a convenience spelling of the other one — it is
/// the form an operator actually has, because the reason to pin a fixed floor
/// is usually a date ("nothing from before term started"). Midnight UTC rather
/// than local, because every other instant in this system is UTC and a floor
/// that shifted with the server's timezone would move under a deployment that
/// changed hosts.
///
/// # Errors
///
/// Returns a message naming the offending text and both accepted forms.
pub fn instant(text: &str) -> Result<DateTime<Utc>, String> {
    let text = text.trim();

    if let Ok(stamped) = DateTime::parse_from_rfc3339(text) {
        return Ok(stamped.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Ok(date.and_time(NaiveTime::MIN).and_utc());
    }

    Err(format!(
        "'{text}' is not an instant; write it as e.g. 2026-01-01T00:00:00Z or 2026-01-01"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(text: &str) -> u64 {
        duration(text).expect("parsing a duration").as_secs()
    }

    #[test]
    fn a_single_term_carries_its_unit() {
        assert_eq!(secs("30s"), 30);
        assert_eq!(secs("5min"), 300);
        assert_eq!(secs("2h"), 7200);
        assert_eq!(secs("30d"), 30 * 86_400);
        assert_eq!(secs("2w"), 14 * 86_400);
    }

    #[test]
    fn the_long_spellings_work_too() {
        // The form an operator writes when they are not in a hurry, and the one
        // most likely to appear in a config file someone else has to read.
        assert_eq!(secs("2 weeks"), 14 * 86_400);
        assert_eq!(secs("1 day"), 86_400);
        assert_eq!(secs("45 minutes"), 2_700);
    }

    #[test]
    fn terms_are_summed() {
        assert_eq!(secs("1h 30min"), 5_400);
        assert_eq!(secs("1d 12h"), 129_600);
        // ...with or without the whitespace, as systemd accepts them.
        assert_eq!(secs("1h30min"), 5_400);
    }

    #[test]
    fn a_bare_integer_is_seconds() {
        // The spelling the rest of the config document uses, and the one the
        // environment produces on its own: `config`'s type-guessing turns
        // `SISMATIC_SERVER__STORE__CLEANUP_INTERVAL=300` into an integer before
        // this is reached.
        assert_eq!(secs("300"), 300);
    }

    #[test]
    fn zero_parses_rather_than_failing() {
        // `humantime` requires a unit, so this is the case the fast path above
        // exists for: `0` is the disable sentinel every other delay in this
        // document uses, and it has to reach the layer that reads it as one.
        assert_eq!(secs("0"), 0);
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(secs("  30d  "), 30 * 86_400);
    }

    #[test]
    fn a_bad_duration_names_itself_and_the_accepted_forms() {
        let err = duration("thirty days").unwrap_err();
        assert!(err.contains("thirty days"), "got: {err}");
        assert!(
            err.contains("30d"),
            "the message has to show a good one: {err}"
        );
    }

    #[test]
    fn a_date_is_not_a_duration() {
        // The two forms `retain` accepts have to be distinguishable, or the
        // dispatch between them would depend on which was tried first.
        assert!(duration("2026-01-01").is_err());
    }

    // ---- sizes -----------------------------------------------------------

    #[test]
    fn a_bare_integer_is_bytes() {
        assert_eq!(bytes("1024").unwrap(), 1024);
        assert_eq!(bytes("0").unwrap(), 0);
    }

    #[test]
    fn the_binary_suffixes_are_powers_of_1024() {
        assert_eq!(bytes("1KiB").unwrap(), 1024);
        assert_eq!(bytes("512MiB").unwrap(), 512 * 1024 * 1024);
        assert_eq!(bytes("2GiB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(bytes("1TiB").unwrap(), 1u64 << 40);
    }

    #[test]
    fn the_bare_suffixes_read_as_systemd_reads_them() {
        // `MemoryMax=512M` is 512 MiB, and an operator who has just written one
        // must not find that this document disagrees by 4.9%.
        assert_eq!(bytes("512M").unwrap(), bytes("512MiB").unwrap());
        assert_eq!(bytes("2G").unwrap(), bytes("2GiB").unwrap());
    }

    #[test]
    fn the_decimal_suffixes_are_powers_of_1000() {
        // ...and are therefore *not* the same as the bare ones, which is the
        // whole reason both spellings are accepted rather than conflated.
        assert_eq!(bytes("1KB").unwrap(), 1_000);
        assert_eq!(bytes("2GB").unwrap(), 2_000_000_000);
        assert_ne!(bytes("512MB").unwrap(), bytes("512MiB").unwrap());
    }

    #[test]
    fn sizes_are_case_insensitive_and_may_be_spaced() {
        assert_eq!(bytes("512mib").unwrap(), bytes("512MiB").unwrap());
        assert_eq!(bytes("512 MiB").unwrap(), bytes("512MiB").unwrap());
        assert_eq!(bytes("  2gb ").unwrap(), 2_000_000_000);
    }

    #[test]
    fn a_fractional_size_is_accepted() {
        assert_eq!(bytes("1.5GiB").unwrap(), 1_610_612_736);
        assert_eq!(bytes("0.5KiB").unwrap(), 512);
    }

    #[test]
    fn an_unknown_unit_is_named_in_the_error() {
        let err = bytes("512QB").unwrap_err();
        assert!(err.contains("qb"), "the offending unit, got: {err}");
        assert!(err.contains("512MiB"), "and a good one, got: {err}");
    }

    #[test]
    fn a_size_that_would_overflow_is_rejected_rather_than_wrapping() {
        // Silently wrapping would turn "far too big" into "far too small",
        // which is the one arithmetic slip a memory budget must not make.
        assert!(bytes("99999999999999999999TiB").is_err());
        assert!(bytes(&format!("{}TiB", u64::MAX)).is_err());
    }

    #[test]
    fn nonsense_is_not_a_size() {
        assert!(bytes("").is_err());
        assert!(bytes("lots").is_err());
        assert!(bytes("-1").is_err());
        assert!(bytes("1.2.3MiB").is_err());
    }

    // ---- instants --------------------------------------------------------

    #[test]
    fn an_rfc_3339_instant_round_trips() {
        let at = instant("2026-01-01T00:00:00Z").unwrap();
        assert_eq!(at.to_rfc3339(), "2026-01-01T00:00:00+00:00");
    }

    #[test]
    fn an_offset_is_normalized_to_utc() {
        // The store compares timestamps as strings, so anything that reaches it
        // has to already be in the one timezone those strings are written in.
        assert_eq!(
            instant("2026-01-01T02:00:00+02:00").unwrap(),
            instant("2026-01-01T00:00:00Z").unwrap()
        );
    }

    #[test]
    fn a_bare_date_is_midnight_utc() {
        assert_eq!(
            instant("2026-01-01").unwrap(),
            instant("2026-01-01T00:00:00Z").unwrap()
        );
    }

    #[test]
    fn a_bad_instant_names_itself_and_the_accepted_forms() {
        let err = instant("last tuesday").unwrap_err();
        assert!(err.contains("last tuesday"), "got: {err}");
        assert!(err.contains("2026-01-01"), "got: {err}");
    }

    #[test]
    fn an_impossible_date_is_rejected() {
        assert!(instant("2026-02-30").is_err());
        assert!(instant("2026-13-01").is_err());
    }

    #[test]
    fn a_duration_is_not_an_instant() {
        assert!(instant("30d").is_err());
    }

    // ---- rendering a size back out ----------------------------------------

    #[test]
    fn a_size_is_written_in_the_largest_unit_that_divides_it() {
        assert_eq!(format_bytes(256 * 1024 * 1024), "256MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1GiB");
        assert_eq!(format_bytes(1024), "1KiB");
        assert_eq!(format_bytes(1 << 40), "1TiB");
    }

    #[test]
    fn a_size_that_is_no_whole_unit_is_written_in_bytes() {
        // Exact rather than pretty. What this renders is fed straight back in —
        // `GET /v1/config` returns a document that is a valid `PATCH` body — so
        // a rounded `1.2MiB` would make a read-modify-write cycle silently move
        // the budget.
        assert_eq!(format_bytes(1_234_567), "1234567");
        assert_eq!(format_bytes(0), "0");
        assert_eq!(format_bytes(1), "1");
        // ...including a figure that is *nearly* a unit.
        assert_eq!(format_bytes(1024 * 1024 - 1), "1048575");
    }

    #[test]
    fn every_rendered_size_parses_back_as_itself() {
        // The property the round trip rests on, over both the values a config
        // is likely to hold and the boundaries of each unit.
        for value in [
            0,
            1,
            1023,
            1024,
            1025,
            1_048_576,
            256 * 1024 * 1024,
            1_234_567,
            (1 << 30) + 1,
            1 << 40,
            u64::MAX,
        ] {
            let rendered = format_bytes(value);
            assert_eq!(
                bytes(&rendered),
                Ok(value),
                "{value} rendered as '{rendered}'"
            );
        }
    }

    #[test]
    fn every_rendered_duration_parses_back_as_itself() {
        // The same property for the other half of the `[store]` section, which
        // is rendered by `humantime` rather than by this module — asserted here
        // because it is this module's parser that has to accept it.
        for value in [
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(300),
            Duration::from_secs(3_600),
            Duration::from_secs(24 * 60 * 60),
            Duration::from_secs(30 * 86_400),
            Duration::from_secs(10 * 86_400 + 12 * 3_600),
        ] {
            let rendered = humantime::format_duration(value).to_string();
            assert_eq!(
                duration(&rendered),
                Ok(value),
                "{value:?} rendered as '{rendered}'"
            );
        }
    }
}
