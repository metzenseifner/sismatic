//! tests/server_checks.rs — the two things that unit tests in
//! `configuration.rs` deliberately cannot cover.
//!
//! Everything about *which* path `devices_config_path` resolves to is a pure
//! function and is tested there, with no filesystem. What is left is the wiring:
//! that a real file on disk round-trips through `get_configuration` into a path
//! core's loader accepts, and that `run` can be driven with values alone. Both
//! use a checked-in fixture directory addressed through `CARGO_MANIFEST_DIR`, so
//! neither depends on the working directory the test binary is launched from.

use std::path::{Path, PathBuf};

use sismatic_core::devices::config::{self, Resolved};
use sismatic_server::configuration::{ServerConfig, SyncConfig, get_configuration};
use sismatic_server::run;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The seam that matters: a relative `devices_config_path` in a config file
/// resolves against that file's directory, and the result is a path core's
/// loader can actually open.
#[test]
fn devices_config_path_resolves_next_to_the_config_file_and_loads() {
    let cfg = get_configuration(fixture("configuration.yaml")).expect("reading fixture config");

    assert_eq!(cfg.devices_config_path, fixture("devices.toml"));

    let devices = config::load(&cfg.devices_config_path).expect("loading the devices it names");
    let ids: Vec<&str> = devices.devices.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, ["fixture-atrium", "fixture-annex"]);
    assert_eq!(devices.groups.len(), 1);
}

/// The rest of the file still parses into the resolved values the runtime reads.
#[test]
fn the_fixture_config_resolves_its_sync_and_http_sections() {
    let cfg = get_configuration(fixture("configuration.yaml")).expect("reading fixture config");

    assert_eq!(cfg.sync.interval_secs, 5);
    assert_eq!(cfg.sync.fields, vec!["RUNNING_STATE".to_owned()]);
    assert_eq!(cfg.http.host, "0.0.0.0");
    assert_eq!(cfg.http.port, 9000);
}

/// A missing config file is an error, not a panic or a silent set of defaults.
#[test]
fn a_missing_config_file_is_reported() {
    assert!(get_configuration(fixture("does-not-exist.yaml")).is_err());
}

/// `run` takes values, not paths: an empty device set spawns no poll loops, so
/// the whole server starts and shuts down cleanly with nothing on disk and no
/// network. This is the property that makes the seam worth having.
#[tokio::test]
async fn run_starts_and_shuts_down_without_touching_the_filesystem() {
    let cfg = ServerConfig {
        devices_config_path: PathBuf::from("unused-by-run.toml"),
        sync: SyncConfig {
            interval_secs: 1,
            fields: vec!["RUNNING_STATE".to_owned()],
        },
        http: sismatic_server::configuration::HttpConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
        },
    };

    run(cfg, Resolved::default(), std::future::ready(()))
        .await
        .expect("run should shut down cleanly");
}
