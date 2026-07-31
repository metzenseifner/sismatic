//! tests/server_checks.rs — what unit tests in `configuration.rs` deliberately
//! cannot cover.
//!
//! Everything about *which* path `devices_config_path` resolves to is a pure
//! function and is tested there, with no filesystem and no environment. What is
//! left is the wiring: that a real file on disk round-trips through
//! `get_configuration` into a path core's loader accepts, that the environment
//! layer composes with that read, that the *process's* environment is what the
//! loader actually consults, and that `run` can be driven with values alone.
//! Everything file-based uses a checked-in fixture directory addressed through
//! `CARGO_MANIFEST_DIR`, so no test depends on the working directory the test
//! binary is launched from.
//!
//! The last of those — `run` over literal values — is also where the two sides
//! of the system are shown to be joined at all: the config's `http` section
//! decides the socket the read-side API ends up answering on, and one shutdown
//! future stops the API and the sync poll loops together. That end-to-end path
//! is the only thing neither crate can test alone.
//!
//! The two tests that need a real environment rather than an injected one drive
//! the real executable, because a subprocess owns its environment: cargo runs
//! these on shared threads, where mutating the environment would be both
//! `unsafe` and visible to every other test.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use sismatic_core::devices::config::{self, Resolved};
use sismatic_core::protocol::instructions::query::Query;
use sismatic_server::configuration::{
    CONFIG_PATH_ENV, FieldConfig, ServerConfig, SyncConfig, env_source, get_configuration,
    get_configuration_with_env,
};
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

    assert_eq!(cfg.sync.default_interval, Some(Duration::from_secs(5)));
    assert_eq!(cfg.http.host, "0.0.0.0");
    assert_eq!(cfg.http.port, 9000);

    // The fixture writes one field bare, one as a table, and one disabled with
    // `interval_secs: 0`; they arrive as one flat list in which each field
    // already carries its own schedule, `None` being never.
    let schedule: Vec<(&str, Option<u64>)> = cfg
        .sync
        .fields
        .iter()
        .map(|f| (f.name.as_str(), f.interval.map(|i| i.as_secs())))
        .collect();
    assert_eq!(
        schedule,
        [
            ("RUNNING_STATE", Some(5)),
            ("FIRMWARE", Some(3600)),
            ("UNIT_NAME", None),
        ]
    );
}

/// The catch-all config, read from a real file: `"*"` survives YAML quoting and
/// expands to core's whole query catalog, with the two overrides still applied.
///
/// The unit tests prove the expansion *equals* `Query::ALL`; what only a real
/// file can show is that the spelling a deployment actually writes parses.
#[test]
fn the_wildcard_config_expands_through_the_real_loader() {
    let cfg = get_configuration(fixture("configuration-all-fields.yaml"))
        .expect("reading the wildcard fixture");

    let by_name = |name: &str| {
        cfg.sync
            .fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("{name} missing from the expansion"))
            .interval
    };

    // Far more than the three fields the file mentions by name.
    assert!(cfg.sync.fields.len() > 3, "wildcard did not expand");
    assert_eq!(by_name("RUNNING_STATE"), Some(Duration::from_secs(5)));
    assert_eq!(by_name("MAC_ADDRESS"), None);
    assert_eq!(by_name("FIRMWARE"), Some(Duration::from_secs(300)));

    // Every expanded field is a name core will actually accept, which is what
    // makes the wildcard safe to hand straight to the driver.
    for field in &cfg.sync.fields {
        assert!(
            Query::from_str(&field.name).is_ok(),
            "wildcard produced a field core cannot query: {}",
            field.name
        );
    }
}

/// The environment layer, driven through the real file loader rather than
/// through config text: variables merge over what the fixture states, at the
/// keys they name and nowhere else.
///
/// Unit tests prove the merge rules; what only a real file can show is that the
/// two sources compose after the *extension-dispatched* parse, and that a
/// relative path arriving from the environment is still anchored to the config
/// file's directory.
#[test]
fn the_environment_layers_over_a_config_read_from_disk() {
    // Injected rather than exported into the process: `std::env::set_var` is
    // `unsafe` in edition 2024 because cargo's test threads share one
    // environment, and a test that mutated it could change another's result.
    let vars = [
        ("SISMATIC_SERVER__HTTP__PORT", "1234"),
        ("SISMATIC_SERVER__DEVICES_CONFIG_PATH", "devices.toml"),
        // In the namespace but not in the document: it named the file, and the
        // file has already been chosen. Dropped rather than rejected.
        (CONFIG_PATH_ENV, "/should/be/ignored.yaml"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();

    let cfg = get_configuration_with_env(
        fixture("configuration.yaml"),
        env_source().source(Some(vars)),
    )
    .expect("reading fixture config with an environment");

    assert_eq!(cfg.http.port, 1234, "the variable should win");
    assert_eq!(
        cfg.http.host, "0.0.0.0",
        "the key beside it should not move"
    );
    assert_eq!(
        cfg.devices_config_path,
        fixture("devices.toml"),
        "a relative path from the environment anchors to the config's directory"
    );
    // ...and the sections the environment said nothing about are untouched.
    assert_eq!(cfg.sync.default_interval, Some(Duration::from_secs(5)));
}

/// ...and that `get_configuration` is wired to the *process's* environment,
/// which every test above deliberately cannot show: they inject a map precisely
/// so they never touch it, so a `get_configuration` that had forgotten to add
/// [`env_source`] at all would pass all of them.
///
/// A subprocess is what makes reading the real environment safe here — it owns
/// its own, so setting a variable races nothing. The variable names a devices
/// file that cannot exist, and the failure quoting that path back is the proof
/// the value survived the merge and the resolve into `devices_config_path`.
#[test]
fn the_real_environment_reaches_the_real_binary() {
    const MISSING: &str = "/nonexistent/from-the-environment.toml";

    let out = Command::new(env!("CARGO_BIN_EXE_sismatic-server"))
        .arg("--config")
        .arg(fixture("configuration.yaml"))
        .env("SISMATIC_SERVER__DEVICES_CONFIG_PATH", MISSING)
        .output()
        .expect("running the server binary");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(MISSING),
        "expected the environment's devices path to have been used, got: {stderr}"
    );
    // The fixture's own devices path is what it displaced.
    assert!(
        !stderr.contains("fixture-atrium"),
        "the fixture's devices file should not have been read at all: {stderr}"
    );
}

/// A missing config file is an error, not a panic or a silent set of defaults.
#[test]
fn a_missing_config_file_is_reported() {
    assert!(get_configuration(fixture("does-not-exist.yaml")).is_err());
}

/// ...and the *binary* answers that failure with help rather than a panic, so
/// an operator who has not yet written a config is told which flag to reach
/// for. Drives the real executable, because the behaviour under test is the
/// composition root's — exit code and stderr, neither of which is a value any
/// unit test can inspect.
#[test]
fn a_missing_config_file_makes_the_binary_print_help() {
    let out = Command::new(env!("CARGO_BIN_EXE_sismatic-server"))
        .arg("--config")
        .arg(fixture("does-not-exist.yaml"))
        // The env layer must not rescue an explicitly-named missing file.
        .env(CONFIG_PATH_ENV, fixture("configuration.yaml"))
        .output()
        .expect("running the server binary");

    assert_eq!(out.status.code(), Some(2), "clap's usage exit code");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no config file at") && stderr.contains("does-not-exist.yaml"),
        "expected the missing path named, got: {stderr}"
    );
    assert!(
        stderr.contains("--config") && stderr.contains("--host"),
        "expected the help text, got: {stderr}"
    );
    // Help is a diagnostic, not output: a caller piping this binary gets nothing.
    assert!(out.stdout.is_empty(), "expected a clean stdout");
}

/// A `ServerConfig` naming no real devices file, with the two poll schedules
/// worth distinguishing — one enabled, one disabled — and an http section
/// pinned to `host`/`port`.
fn test_config(host: &str, port: u16) -> ServerConfig {
    ServerConfig {
        devices_config_path: PathBuf::from("unused-by-run.toml"),
        sync: SyncConfig {
            default_interval: Some(Duration::from_secs(1)),
            fields: vec![
                FieldConfig {
                    name: "RUNNING_STATE".to_owned(),
                    interval: Some(Duration::from_secs(1)),
                },
                // A disabled field must be as harmless to `run` as an enabled
                // one: it is skipped, not turned into a zero-delay ticker.
                FieldConfig {
                    name: "UNIT_NAME".to_owned(),
                    interval: None,
                },
            ],
        },
        http: sismatic_server::configuration::HttpConfig {
            host: host.to_owned(),
            port,
        },
    }
}

/// `run` takes values, not paths: an empty device set spawns no poll loops, so
/// the whole server starts and shuts down cleanly with nothing on disk and
/// nothing but a loopback socket. This is the property that makes the seam worth
/// having.
///
/// `port: 0` is the kernel's choice of a free port — this test never addresses
/// the server, so it only has to bind *somewhere*.
#[tokio::test]
async fn run_starts_and_shuts_down_without_touching_the_filesystem() {
    run(
        test_config("127.0.0.1", 0),
        Resolved::default(),
        std::future::ready(()),
    )
    .await
    .expect("run should shut down cleanly");
}

/// The whole point of the composition root, end to end: the http-api that
/// `run` starts answers on the host and port the *config* named, and stops when
/// the caller's shutdown future resolves.
///
/// Every layer below this is already tested on its own — the resolver over
/// values, the http-api over a listener it was handed. What only this test can
/// show is that `run` joins them: that `cfg.http` is what the socket ends up
/// bound to, and that one shutdown signal stops both sides.
#[tokio::test]
async fn run_serves_the_health_check_on_the_configured_port() {
    // `run` binds from the config, so unlike the http-api's own tests this one
    // cannot be handed a listener and read the port back off it. The port is
    // instead one the kernel just confirmed was free, released immediately: a
    // closed listening socket with no accepted connections leaves nothing
    // behind (no `TIME_WAIT`), so the rebind below succeeds. Racing another
    // process for it in the window is possible in principle; picking a fixed
    // port would race *every* run of this suite instead.
    let port = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
        probe
            .local_addr()
            .expect("reading the bound address")
            .port()
    };

    let (stop, shutdown) = oneshot::channel::<()>();
    let server = tokio::spawn(run(
        test_config("127.0.0.1", port),
        Resolved::default(),
        async {
            // A dropped sender resolves this too, so a failing test tears the
            // server down rather than hanging.
            let _ = shutdown.await;
        },
    ));

    // Spoken by hand over a plain socket rather than through an HTTP client:
    // the assertion is about the status line an external probe reads, and a
    // dependency to produce one would be a dependency this crate otherwise has
    // no use for.
    let status = tokio::task::spawn_blocking(move || get_status_line(port))
        .await
        .expect("the client task");
    assert!(
        status.starts_with("HTTP/1.1 200"),
        "expected a 200 from the health check, got: {status}"
    );

    stop.send(()).expect("run should still be listening");
    server
        .await
        .expect("the server task")
        .expect("run should shut down cleanly");
}

/// `GET /health_check` on loopback, returning the response's status line.
///
/// Retries the connect: `run` is spawned, so there is no moment the test can
/// observe at which it has reached its `bind`. Blocking I/O, hence the
/// [`tokio::task::spawn_blocking`] at the call site.
fn get_status_line(port: u16) -> String {
    /// Long enough to cover a loaded CI machine starting a runtime; short
    /// enough that a server which never binds fails the test rather than
    /// hanging until the harness gives up.
    const DEADLINE: Duration = Duration::from_secs(10);
    const RETRY: Duration = Duration::from_millis(20);

    let addr = format!("127.0.0.1:{port}");
    let started = Instant::now();
    let mut socket = loop {
        match TcpStream::connect(&addr) {
            Ok(socket) => break socket,
            Err(_) if started.elapsed() < DEADLINE => std::thread::sleep(RETRY),
            Err(e) => panic!("nothing accepted on {addr} within {DEADLINE:?}: {e}"),
        }
    };

    // `Connection: close` so the server ends the body by closing, and
    // `read_to_string` therefore terminates without parsing a single header.
    socket
        .write_all(b"GET /health_check HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("sending the request");

    let mut response = String::new();
    socket
        .read_to_string(&mut response)
        .expect("reading the response");

    response.lines().next().unwrap_or_default().to_owned()
}
