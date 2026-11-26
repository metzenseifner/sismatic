//! Integration tests against the simulated device from
//! [`sismatic_core::simulator`] — a *real* SSH server that offers only
//! `keyboard-interactive` authentication, like an Extron SMP, so
//! [`RusshConnector`]'s auth fallback runs end to end over a genuine russh
//! handshake rather than a mock.
//!
//! The server and its config loader are library code shared verbatim with
//! `bin/simulated-device.rs`, so these tests exercise the device a developer
//! runs by hand. `simulator_config.rs` guards its config against the catalogs
//! without a socket; this file is the part that needs one.
#![cfg(feature = "simulator")]

use std::collections::BTreeSet;
use std::sync::{Arc, Once};
use std::time::Duration;

use sismatic_core::devices::config::{DeviceConfig, Uuid};
use sismatic_core::devices::connector::{ConnectError, Connector};
use sismatic_core::devices::device::Device;
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_core::protocol::instructions::commands::Command;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_core::protocol::instructions::register::Register;
use sismatic_core::protocol::instructions::setting::Setting;
use sismatic_core::protocol::{RecordingState, Value};
use sismatic_core::simulator::{DeviceState, ReplyStream, SimulatedDevice, bind};
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry};

const CONFIG: &str = include_str!("fixtures/device.yaml");

const USERNAME: &str = "admin";
const PASSWORD: &str = "extron";
/// The unit name the fixture gives the device, named once so a fixture change
/// is a one-line update.
const UNIT_NAME: &str = "Main Hall SMP";

static INIT: Once = Once::new();
fn init_tracing() {
    INIT.call_once(|| {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"));
        let formatting_layer = BunyanFormattingLayer::new(
            "sismatic-core".into(),
            //tracing_subscriber::fmt::TestWriter::default, // see gotcha #2
            std::io::stdout,
        );
        Registry::default()
            .with(env_filter)
            .with(JsonStorageLayer)
            .with(formatting_layer)
            .init();
    });
}

/// Bind the simulated SMP on a random loopback port, seeded from the checked-in
/// config. Returns once it is listening, so there is no bind to race.
async fn spawn_device(reply: ReplyStream) -> SimulatedDevice {
    init_tracing();
    let state = DeviceState::from_yaml_str(CONFIG).expect("the checked-in config parses");
    bind(("127.0.0.1", 0), Arc::new(state), reply)
        .await
        .expect("bind loopback")
}

/// A device config pointed at `simulated_device`, authenticating with `password`.
fn device_config(simulated_device: &SimulatedDevice, password: &str) -> DeviceConfig {
    DeviceConfig {
        id: "sim".into(),
        host: "127.0.0.1".into(),
        port: simulated_device.port(),
        username: USERNAME.into(),
        password: password.into(),
        connect_timeout: Duration::from_secs(5),
        exchange_timeout: Duration::from_secs(5),
        eager: false,
        sis_keepalive: None,
        eager_retry: None,
        cold_backoff: None,
        uuid: Uuid::nil(),
        // No client-side veto: these tests are about what the *device*
        // answers, so every field must reach it. Refusals are
        // `simulator_license.rs`.
        disabled_fields: BTreeSet::new(),
        auto_disable_after: 0,
        self_heal: None,
    }
    .derive_uuid()
}

/// The same `Device` stack production uses, so the whole path — RusshConnector,
/// Controller, the real parser — runs over SSH.
async fn connected(simulated_device: &SimulatedDevice) -> Device {
    Device::new(
        device_config(simulated_device, PASSWORD),
        Arc::new(RusshConnector),
    )
}

#[tokio::test]
async fn keyboard_interactive_auth_succeeds() {
    let simulated_device = spawn_device(ReplyStream::Stdout).await;

    RusshConnector
        .connect(&device_config(&simulated_device, PASSWORD))
        .await
        .expect("keyboard-interactive auth should succeed");
}

#[tokio::test]
async fn wrong_password_is_rejected() {
    let simulated_device = spawn_device(ReplyStream::Stdout).await;

    match RusshConnector
        .connect(&device_config(&simulated_device, "not-the-password"))
        .await
    {
        Err(ConnectError::Failed(_)) => {}
        Err(other) => panic!("expected an auth failure, got {other:?}"),
        Ok(_) => panic!("expected an auth failure, but the connection succeeded"),
    }
}

#[tokio::test]
async fn unit_name_query_round_trips() {
    let simulated_device = spawn_device(ReplyStream::Stdout).await;
    let device = connected(&simulated_device).await;

    let value = device
        .run(&Query::UnitName.instruction())
        .await
        .expect("unit-name query should succeed");

    assert_eq!(value, Value::Text(UNIT_NAME.into()));
}

#[tokio::test]
async fn unit_name_query_over_stderr_round_trips() {
    // Regression: real Extron SMP devices reply on the SSH extended-data
    // (stderr) stream. The transport must read stderr as well as stdout, or the
    // query times out (see the `into_stream` note in the ssh transport).
    let simulated_device = spawn_device(ReplyStream::Stderr).await;
    let device = connected(&simulated_device).await;

    let value = device
        .run(&Query::UnitName.instruction())
        .await
        .expect("unit-name query should succeed when the device replies on stderr");

    assert_eq!(value, Value::Text(UNIT_NAME.into()));
}

/// A reply that is not free text, so the config's wire text is put through the
/// real parser over a real connection and not only in `simulator_config.rs`.
#[tokio::test]
async fn a_typed_query_decodes_over_the_wire() {
    let simulated_device = spawn_device(ReplyStream::Stderr).await;
    let device = connected(&simulated_device).await;

    let value = device
        .run(&Query::SshPort.instruction())
        .await
        .expect("ssh-port query should succeed");

    assert_eq!(value, Value::Port(22023));
}

/// The pairing check nothing else covers: for *every* register, a write to
/// `M<i>` must land where a read of the same field looks. `Register::index`
/// picks the write address and `Query::instruction` independently picks the
/// read verb, and nothing but this forces them to agree.
///
/// Each register gets a distinct probe, so a write landing in the wrong cell
/// shows up as a mismatch rather than passing by coincidence.
#[tokio::test]
async fn set_then_get_round_trips_every_register() {
    use std::str::FromStr;

    let simulated_device = spawn_device(ReplyStream::Stderr).await;
    let device = connected(&simulated_device).await;

    for &register in Register::ALL {
        // `every_settable_register_has_a_read_path` guarantees this resolves.
        let query = Query::from_str(register.name())
            .unwrap_or_else(|_| panic!("{register} has no matching query"));
        let probe = format!("probe-{}-{}", register.name(), register.index());

        device
            .run(&register.instruction(&probe))
            .await
            .unwrap_or_else(|e| panic!("writing {register} should succeed: {e:?}"));

        let value = device
            .run(&query.instruction())
            .await
            .unwrap_or_else(|e| panic!("reading {query} back should succeed: {e:?}"));

        assert_eq!(
            value,
            Value::Text(probe),
            "{register} (M{}) did not read back through {query}",
            register.index()
        );
    }
}

/// The same round trip for the other write catalog — the one the `PUT
/// .../settings/{field}` route ends in. Two things have to hold, and neither is
/// checkable without a connection.
///
/// The device must *acknowledge* the write in bytes `setting_echo` accepts: a
/// setting echo carries no `RCDR` and no `*`, unlike a register's, and getting
/// that wrong is not a wrong answer but a **command timeout**, five seconds of
/// nothing. And the value must be observable through the query sharing the
/// setting's name, which pairs `Setting::verb` against the read verb
/// `Query::instruction` picks independently.
#[tokio::test]
async fn set_then_get_round_trips_every_setting() {
    use std::str::FromStr;

    let simulated_device = spawn_device(ReplyStream::Stderr).await;
    let device = connected(&simulated_device).await;

    for (i, &setting) in Setting::ALL.iter().enumerate() {
        let probe = probe_value(i, setting);

        let echoed = device
            .run(&setting.instruction(&probe).expect("the probe is legal"))
            .await
            .unwrap_or_else(|e| panic!("writing {setting} should succeed: {e:?}"));

        // Every setting echo decodes to `Value::Text`, whatever the field's
        // read type — it is the device repeating the characters it was sent.
        assert_eq!(
            echoed,
            Value::Text(probe.clone()),
            "{setting} did not echo the value it was written"
        );

        // The nine RTMP settings with no read path stop at the echo: SIS offers
        // no read of a push URL or an enable flag, so there is no query to pair
        // the write with. This is the coverage that hole costs, and
        // `write_only_settings_are_exactly_the_attested_rtmp_hole` pins it.
        let Ok(query) = Query::from_str(setting.name()) else {
            continue;
        };

        let read_back = device
            .run(&query.instruction())
            .await
            .unwrap_or_else(|e| panic!("reading {query} back should succeed: {e:?}"));

        // `Display` is the wire text for the three types a setting can have
        // (text, port, flag). It is a human-facing projection only for
        // `Value::State`, which no setting writes.
        assert_eq!(
            read_back.to_string(),
            probe,
            "{setting} did not read back through {query}"
        );
    }
}

/// A value `setting` accepts, found by asking the catalog's own encoder rather
/// than keeping a per-setting table here that every new setting would have to
/// be added to by hand.
///
/// The ladder runs most to least distinctive, so free-text settings get a value
/// unique to them and narrower shapes fall through to one they accept. `i`
/// keeps the ports apart and alternates the flags, so a write landing in the
/// wrong cell shows up as a mismatch rather than passing by coincidence.
fn probe_value(i: usize, setting: Setting) -> String {
    [
        format!("probe-{}", setting.name()),
        // An RTMP push target takes neither rung: it demands an `rtmp://`
        // scheme, which rules out free text, and it is not a number. The field
        // name in the path keeps it as distinctive as the free-text rung.
        format!("rtmp://cdn.example.edu/live/probe-{}", setting.name()),
        (40001 + i).to_string(),
        (i % 2).to_string(),
    ]
    .into_iter()
    .find(|candidate| setting.instruction(candidate).is_ok())
    .unwrap_or_else(|| panic!("no probe value for {setting}: extend the ladder"))
}

/// Config is the *initial* state: a command mutates the device and the change
/// is visible to a later query. Also pins `running_state_code`'s command →
/// state-code mapping against `Query::RunningState`'s parser, the one place the
/// two could drift apart.
#[tokio::test]
async fn commands_mutate_the_running_state() {
    let simulated_device = spawn_device(ReplyStream::Stderr).await;
    let device = connected(&simulated_device).await;

    // The config starts the device stopped.
    assert_eq!(
        device
            .run(&Query::RunningState.instruction())
            .await
            .unwrap(),
        Value::State(RecordingState::Stopped)
    );

    for (command, expected) in [
        (Command::Start, RecordingState::Started),
        (Command::Pause, RecordingState::Paused),
        (Command::Stop, RecordingState::Stopped),
    ] {
        device
            .run(&command.instruction())
            .await
            .unwrap_or_else(|e| panic!("{command} should be acknowledged: {e:?}"));

        assert_eq!(
            device
                .run(&Query::RunningState.instruction())
                .await
                .unwrap(),
            Value::State(expected),
            "{command} should leave the device {expected}"
        );
    }
}

/// A reboot is acknowledged with `Boot1`, then the simulated unit drops the
/// session like the hardware. The next call must dial a fresh session rather
/// than write into the ended one, and finds the recording ended.
#[tokio::test]
async fn reboot_is_acknowledged_and_ends_the_session() {
    let simulated_device = spawn_device(ReplyStream::Stderr).await;
    let device = connected(&simulated_device).await;

    device.run(&Command::Start.instruction()).await.unwrap();
    assert_eq!(
        device
            .run(&Command::Reboot.instruction())
            .await
            .expect("the reboot should be acknowledged"),
        Value::Ack("Boot1".into())
    );
    assert_eq!(
        device
            .run(&Query::RunningState.instruction())
            .await
            .expect("the call after a reboot dials a fresh session"),
        Value::State(RecordingState::Stopped)
    );
}
