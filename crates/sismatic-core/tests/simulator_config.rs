//! Drift guards between `tests/fixtures/device.yaml` and the instruction
//! catalogs. These are pure — no SSH, no sockets — so a drift failure points at
//! the config or the catalog, never at the transport.
#![cfg(feature = "simulator")]

use std::collections::BTreeSet;

use sismatic_core::protocol::instructions::query::Query;
use sismatic_core::protocol::instructions::register::Register;
use sismatic_core::protocol::instructions::setting::Setting;
use sismatic_core::simulator::{DeviceState, Overrides, catalog_fields};

/// Pulled in at compile time, so a rename or deletion is a build failure rather
/// than a runtime "file not found". It must live under `tests/fixtures/`:
/// `flake.nix` filters the build source to cargo files plus `python/` and
/// `tests/fixtures/`, so a `.yaml` elsewhere is dropped inside the nix sandbox.
const CONFIG: &str = include_str!("fixtures/device.yaml");

fn load() -> DeviceState {
    DeviceState::from_yaml_str(CONFIG).expect("the checked-in config parses")
}

/// The headline guard: config and catalog agree in both directions, and every
/// value decodes through the production parser. A new catalog variant with no
/// config entry fails here and names the field, as does a typo'd key or a value
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
/// bytes it would put on the wire are fed to *that query's own production
/// parser*. A parser that changes shape — a verb tag appears, a terminator
/// changes — fails here rather than later as a command timeout against a
/// simulator that quietly speaks a dialect.
///
/// Decodability only. The decoded value is deliberately not compared to the
/// config text for non-text fields, because `Display` is a human-facing
/// projection rather than the wire encoding (`Value::State(Started)` prints
/// "started", not "1"); `check()` pins text fields and `simulated_ssh.rs` pins
/// semantics end to end.
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
/// A write-only register would be a field `check()` can only confirm the
/// *presence* of — never decode, and never observe through
/// `set_then_get_round_trips_every_register` — so it would be a silent hole in
/// the drift net. If this fails, either add the matching `Query` variant or
/// accept the hole deliberately.
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
/// `names(Setting) \ names(Query)` is *not* empty, so the set it leaves is
/// pinned instead.
///
/// Metadata is symmetric — every register pairs with a query — but settings
/// are not: SIS offers no read of an RTMP push URL or of a push enable flag,
/// only of whether the push is *live*, which is a different field with no write
/// (see the closing note in `setting.rs`). So nine settings can be written and
/// never read back, and that is the protocol's shape rather than an oversight.
///
/// The hole costs real coverage: a write to these cannot be validated against a
/// read parser, `check()` can only confirm presence, and
/// `set_then_get_round_trips_every_setting` sees the echo but not the stored
/// value. Pinning the set is what keeps a *new* write-only setting from landing
/// quietly. If this fails, add the matching `Query` variant — or add the name
/// here and accept that it is checked for presence alone.
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
/// path route by name: `PUT /settings/TITLE` is refused as metadata by looking
/// `TITLE` up in `Register` and nothing else. An overlap would make that lookup
/// ambiguous and put one cell under two write rules — one applying a recording
/// freeze, one not.
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
// flags — and the merge lives in `from_yaml_str_with`, so these tests exercise
// the precedence an operator gets from the command line.

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

/// A field override seeds one cell and leaves the rest alone — the
/// `--field NAME=VALUE` flag, the quickest way to put the device in a given
/// state without editing a file.
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

/// The license override replaces the file's stream set rather than merging.
/// Merging would make a narrower command line inexpressible: a fully-licensed
/// file plus `--licensed-streams 1` has to mean "this unit has one stream", not
/// "the file's three, and also one".
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

/// A narrowed license is still a config with no drift — the fields it refuses
/// stop being required and nothing else changes — which is what makes
/// `--licensed-streams 1 --check-config` meaningful.
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

/// `RECORDING_EVENTS` from a file, which is the form a realistic recorder
/// needs: a device with a few hundred recordings is a table, not something to
/// paste into a device config.
///
/// The path is absolute, built from `CARGO_MANIFEST_DIR`, because a relative
/// one in a config resolves against the process working directory — see
/// `RawFieldValue::File`.
#[test]
fn a_recording_event_document_can_come_from_a_file() {
    use sismatic_core::protocol::{Step, Value};

    let path = format!(
        "{}/tests/fixtures/recordings.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = format!(
        "\
username: admin
password: extron
fields:
  RECORDING_EVENTS:
    file: {path}
"
    );

    let state = DeviceState::from_yaml_str(&text).expect("the config loads");
    let reply = state
        .get(Query::RecordingEvents.name())
        .map(|body| format!("{body}\r\n"))
        .expect("the field is seeded");

    // The fixture's three rows, read back through the client's own parser —
    // including the sparse row, whose absent fields must stay absent rather
    // than being invented by the round trip through the file.
    match Query::RecordingEvents.instruction().parse_step(&reply) {
        Step::Done(Value::RecordingEvents(events)) => {
            assert_eq!(
                events.iter().map(|e| e.db_id).collect::<Vec<_>>(),
                vec![58, 59, 60]
            );
            assert_eq!(events[2].state, None);
            assert_eq!(events[2].media_present, None);
            assert_eq!(events[0].media_present, Some(true));
        }
        other => panic!("the simulator's own rendering did not parse: {other:?}"),
    }
}

/// The checked-in `recordings.json` is the documented example of the file form,
/// so it has to stay a valid document — a broken example is worse than none.
#[test]
fn the_checked_in_recordings_document_parses() {
    use sismatic_core::protocol::RecordingEvents;

    const DOCUMENT: &str = include_str!("fixtures/recordings.json");
    let document: RecordingEvents = serde_json::from_str(DOCUMENT).expect("valid JSON document");
    assert_eq!(document.recording_events.len(), 3);
    assert!(!document.wire_body().is_empty());
}
