//! Drift guards between `tests/fixtures/device.yaml` and the instruction
//! catalogs. These are pure — no SSH, no sockets — so a drift failure points at
//! the config or the catalog, never at the transport.
#![cfg(feature = "simulator")]

use std::collections::BTreeSet;

use sismatic_core::protocol::instructions::query::Query;
use sismatic_core::protocol::instructions::register::Register;
use sismatic_core::protocol::instructions::setting::Setting;
use sismatic_core::simulator::{DeviceState, Overrides, catalog_fields};

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

/// The same question for the other write catalog, which answers differently:
/// `names(Setting) \ names(Query)` is *not* empty, and the exact set it leaves
/// is pinned here.
///
/// `every_settable_register_has_a_read_path` can demand the difference be empty
/// because metadata is symmetric — every register pairs with a query. Settings
/// are not: SIS offers no read of an RTMP push URL and no read of a push enable
/// flag, only of whether the push is *live*, which is a different field with no
/// write (see the closing note in `setting.rs`). So nine settings can be
/// written and never read back, and that is the protocol's shape rather than an
/// oversight to fix.
///
/// Pinning the set rather than asserting it empty is what keeps the guard
/// useful. The hole costs real coverage — the simulator cannot validate a write
/// to these against a read parser, `check()` can only confirm they are present,
/// and `set_then_get_round_trips_every_setting` can see the echo but not the
/// stored value — so a *new* write-only setting must be a deliberate decision
/// and not something that lands quietly. If this fails because a setting was
/// added: add the matching `Query` variant, or add it here and accept that it
/// is checked for presence alone.
#[test]
fn write_only_settings_are_exactly_the_attested_rtmp_hole() {
    let gettable: BTreeSet<&str> = Query::ALL.iter().map(|q| q.name()).collect();
    let writable: BTreeSet<&str> = Setting::ALL.iter().map(|s| s.name()).collect();

    let write_only: BTreeSet<_> = writable.difference(&gettable).copied().collect();
    let attested: BTreeSet<&str> = [
        "RTMP_1_URL",
        "RTMP_1_BACKUP_URL",
        "RTMP_1_STATE",
        "RTMP_2_URL",
        "RTMP_2_BACKUP_URL",
        "RTMP_2_STATE",
        "RTMP_3_URL",
        "RTMP_3_BACKUP_URL",
        "RTMP_3_STATE",
    ]
    .into_iter()
    .collect();

    assert_eq!(
        write_only, attested,
        "the set of settings with no read path changed; see this test's note"
    );
}

/// The two write catalogs name disjoint fields, which is what lets the write
/// path route by name: `PUT /settings/TITLE` is refused as "that is metadata"
/// by looking `TITLE` up in `Register`, and nothing else. An overlap would make
/// that lookup ambiguous and would put one storage cell under two write rules —
/// one of which applies a recording freeze and one of which does not.
#[test]
fn no_field_is_both_a_metadata_register_and_a_device_setting() {
    let registers: BTreeSet<&str> = Register::ALL.iter().map(|r| r.name()).collect();
    let settings: BTreeSet<&str> = Setting::ALL.iter().map(|s| s.name()).collect();

    let both: BTreeSet<_> = registers.intersection(&settings).copied().collect();
    assert!(
        both.is_empty(),
        "these fields are in both write catalogs: {both:?}"
    );
}

// ---- layered configuration ---------------------------------------------
//
// The binary resolves three layers — built-in config, `--config` file, then
// flags — and the merge lives in `from_yaml_str_with` so these tests exercise
// the same precedence an operator gets from the command line.

/// An override beats the file, which is the whole precedence rule.
#[test]
fn an_override_beats_the_file() {
    let state = DeviceState::from_yaml_str_with(
        CONFIG,
        &Overrides {
            username: Some("operator".into()),
            password: Some("hunter2".into()),
            ..Overrides::default()
        },
    )
    .expect("the checked-in config parses");

    assert_eq!(state.credentials().username, "operator");
    assert_eq!(state.credentials().password, "hunter2");
}

/// An override left unset leaves the file's value alone, so a flag the operator
/// did not pass cannot quietly reset a field to a default.
#[test]
fn an_absent_override_leaves_the_file_alone() {
    let from_file = DeviceState::from_yaml_str(CONFIG).expect("parses");
    let with_empty =
        DeviceState::from_yaml_str_with(CONFIG, &Overrides::default()).expect("parses");

    assert_eq!(
        with_empty.credentials().username,
        from_file.credentials().username
    );
    assert_eq!(with_empty.get("UNIT_NAME"), from_file.get("UNIT_NAME"));
    assert_eq!(
        with_empty.license().refused_fields(),
        from_file.license().refused_fields()
    );
}

/// A field override seeds one cell and leaves the other sixty alone — the
/// `--field NAME=VALUE` flag, which is the quickest way to put the device in a
/// particular state without editing a file.
#[test]
fn a_field_override_replaces_only_that_field() {
    let state = DeviceState::from_yaml_str_with(
        CONFIG,
        &Overrides {
            fields: [("unit-name".to_owned(), "Lecture Hall 2".to_owned())]
                .into_iter()
                .collect(),
            ..Overrides::default()
        },
    )
    .expect("parses");

    // The key canonicalised on the way in, like a file key would.
    assert_eq!(state.get("UNIT_NAME").as_deref(), Some("Lecture Hall 2"));
    assert_eq!(state.get("MODEL_NAME").as_deref(), Some("SMP 351"));
    // And the result is still a config with no drift.
    assert!(state.check().is_empty(), "{:?}", state.check());
}

/// The license override replaces the file's stream set rather than merging with
/// it. Merging would make a narrower command line impossible to express: a
/// fully-licensed file plus `--licensed-streams 1` has to mean "this unit has
/// one stream", not "the file's three, and also one".
#[test]
fn a_license_override_replaces_rather_than_merges() {
    let state = DeviceState::from_yaml_str_with(
        CONFIG,
        &Overrides {
            licensed_streams: Some([1].into_iter().collect()),
            ..Overrides::default()
        },
    )
    .expect("parses");

    assert_eq!(state.license().streams(), &BTreeSet::from([1]));
    assert!(state.license().refuses("STREAM_2_NAME"));
    assert!(!state.license().refuses("STREAM_1_NAME"));
}

/// A narrowed license is still a config with no drift: the fields it refuses
/// stop being required, and nothing else about the file changes. This is what
/// makes `--licensed-streams 1 --check-config` meaningful.
#[test]
fn a_base_unit_config_has_no_drift_either() {
    let state = DeviceState::from_yaml_str_with(
        CONFIG,
        &Overrides {
            licensed_streams: Some([1].into_iter().collect()),
            ..Overrides::default()
        },
    )
    .expect("parses");

    let issues = state.check();
    assert!(
        issues.is_empty(),
        "a one-stream unit should check clean:\n{}",
        issues
            .iter()
            .map(|i| format!("  - {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // Fourteen fields across the two unlicensed streams: a name, an enable
    // state, two push URLs, a push enable and two live states each.
    assert_eq!(state.license().refused_fields().len(), 14);
}
