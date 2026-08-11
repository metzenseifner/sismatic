//! Drift guards between `tests/fixtures/device.yaml` and the instruction
//! catalogs. These are pure — no SSH, no sockets — so a drift failure points at
//! the config or the catalog, never at the transport.
#![cfg(feature = "simulator")]

use std::collections::BTreeSet;

use sismatic_core::protocol::instructions::query::Query;
use sismatic_core::protocol::instructions::register::Register;
use sismatic_core::simulator::{DeviceState, catalog_fields};

/// The config is pulled in at compile time, so a rename or deletion is a build
/// failure rather than a runtime "file not found". It lives under
/// `tests/fixtures/` because `flake.nix` filters the build source down to cargo
/// files plus `python/` and `tests/fixtures/` — a `.yaml` anywhere else would
/// be dropped and these tests would not find it inside the nix sandbox.
const CONFIG: &str = include_str!("fixtures/device.yaml");

fn load() -> DeviceState {
    DeviceState::from_yaml_str(CONFIG).expect("the checked-in config parses")
}

/// The headline guard: the config and the catalog agree exactly, in both
/// directions, and every value decodes through the production parser.
///
/// Adding a `Query`, `Register`, or alias without a config entry fails here and
/// names the field. So does a typo'd key, and so does a value whose wire text
/// the real parser rejects.
#[test]
fn the_checked_in_config_has_no_drift() {
    let issues = load().check();
    assert!(
        issues.is_empty(),
        "device.yaml has drifted from the instruction catalog:\n{}",
        issues
            .iter()
            .map(|i| format!("  - {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Spelled out separately from `check()` so a coverage failure reports the two
/// set differences directly, rather than as a list of individual issues.
#[test]
fn config_keys_equal_the_catalog_field_set() {
    let state = load();
    let configured: BTreeSet<String> = catalog_fields()
        .into_iter()
        .filter(|f| state.get(f).is_some())
        .collect();
    assert_eq!(
        configured,
        catalog_fields(),
        "config is missing catalog fields"
    );
}

/// The round-trip that makes the simulator trustworthy: for every query, the
/// bytes the simulator would put on the wire are fed to *that query's own
/// production parser*. If a parser changes shape — a verb tag appears, a
/// terminator changes — this fails immediately instead of surfacing later as a
/// command timeout against a simulator that quietly speaks a dialect.
///
/// It does not assert the decoded value equals the config text for non-text
/// fields: `Display` is a human-facing projection, not the wire encoding
/// (`Value::State(Started)` prints "started", not "1"). Decodability is the
/// property that matters here; `check()` additionally pins text fields, and
/// `simulated_ssh.rs` pins semantics end to end.
#[test]
fn every_query_round_trips_through_its_production_parser() {
    use sismatic_core::protocol::Step;

    let state = load();
    for &q in Query::ALL {
        let value = state
            .get(q.name())
            .unwrap_or_else(|| panic!("{q} has no configured value"));
        let rendered = format!("{value}\r\n");
        assert!(
            matches!(q.instruction().parse_step(&rendered), Step::Done(_)),
            "{q}: the production parser rejected the simulator's rendering of {value:?}"
        );
    }
}

/// Every settable register must have a read path: `names(Register)` ⊆
/// `names(Query)`.
///
/// This is what makes the coverage guarantee total. A write-only register would
/// be a field `check()` can only confirm the *presence* of — it could never
/// decode it, and `set_then_get_round_trips_every_register` could never observe
/// it — so it would be a silent hole in the drift net. If this fails, either
/// add the matching `Query` variant or accept the hole deliberately.
#[test]
fn every_settable_register_has_a_read_path() {
    let gettable: BTreeSet<&str> = Query::ALL.iter().map(|q| q.name()).collect();
    let settable: BTreeSet<&str> = Register::ALL.iter().map(|r| r.name()).collect();

    let write_only: BTreeSet<_> = settable.difference(&gettable).copied().collect();
    assert!(
        write_only.is_empty(),
        "these registers can be written but never read back: {write_only:?}"
    );

    // The converse is expected and fine: read-only fields (FIRMWARE,
    // MAC_ADDRESS, DATE, IDENTIFIER, ...) have no register to write them.
    assert!(
        gettable.difference(&settable).count() > 0,
        "expected some read-only fields"
    );
}
