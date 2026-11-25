//! Golden snapshot of every instruction catalog.
//!
//! One line per variant, in `ALL` order, tab-separated:
//!
//! ```text
//! enum  index-in-ALL  Debug  name()  accepted()  description()
//! ```
//!
//! This is the contract the catalog declarations must keep: `ALL` order is
//! iterated by `sismatic-intent-relay`, `accepted()` order is what the docs and
//! the Python stub list, and `description()` is user-facing text. A change to
//! how the catalogs are *declared* must leave this file byte-for-byte alone.
//!
//! The golden file lives under `tests/fixtures/` rather than next to this module
//! because the flake's source filter keeps `tests/fixtures/` and drops a loose
//! `.txt` from `src/`.
//!
//! To regenerate after a deliberate change to a catalog, run the test with
//! `SISMATIC_BLESS_CATALOG_SNAPSHOT=1` and review the diff: it is the contract.

use std::fmt::{Debug, Write as _};

use crate::protocol::instructions::{
    commands::Command, query::Query, register::Register, setting::Setting,
};

const GOLDEN: &str = include_str!("../../tests/fixtures/catalog_snapshot.txt");

fn section<T: Copy + Debug + std::fmt::Display>(
    out: &mut String,
    enum_name: &str,
    all: &[T],
    name: fn(T) -> &'static str,
    accepted: fn(T) -> &'static [&'static str],
    description: fn(T) -> &'static str,
) {
    for (i, &v) in all.iter().enumerate() {
        // `Display` is part of the contract too; it must agree with `name()`.
        assert_eq!(v.to_string(), name(v), "{v:?}: Display differs from name()");
        writeln!(
            out,
            "{enum_name}\t{i}\t{v:?}\t{}\t{:?}\t{:?}",
            name(v),
            accepted(v),
            description(v)
        )
        .unwrap();
    }
}

fn render() -> String {
    let mut out = String::new();
    section(
        &mut out,
        "Command",
        Command::ALL,
        Command::name,
        Command::accepted,
        Command::description,
    );
    section(
        &mut out,
        "Register",
        Register::ALL,
        Register::name,
        Register::accepted,
        Register::description,
    );
    section(
        &mut out,
        "Query",
        Query::ALL,
        Query::name,
        Query::accepted,
        Query::description,
    );
    section(
        &mut out,
        "Setting",
        Setting::ALL,
        Setting::name,
        Setting::accepted,
        Setting::description,
    );
    out
}

#[test]
fn catalogs_match_golden_snapshot() {
    let rendered = render();
    if std::env::var_os("SISMATIC_BLESS_CATALOG_SNAPSHOT").is_some() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/catalog_snapshot.txt"
        );
        std::fs::write(path, &rendered).unwrap();
        return;
    }
    if rendered != GOLDEN {
        let first = rendered
            .lines()
            .zip(GOLDEN.lines())
            .position(|(a, b)| a != b)
            .map_or_else(
                || "line count differs".to_string(),
                |n| format!("first difference at line {}", n + 1),
            );
        panic!("catalog snapshot changed ({first}).\n--- rendered ---\n{rendered}");
    }
}
