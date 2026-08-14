//! Integration test against the simulated device from
//! [`sismatic_core::simulator`] — a *real* SSH server that behaves like an
//! Extron SMP: it offers only `keyboard-interactive` authentication, never
//! `password`. This exercises [`RusshConnector`]'s auth fallback end to end
//! over a genuine russh handshake rather than a mock.
//!
//! The server and its config loader are library code, shared verbatim with
//! `bin/simulated-device.rs`, so these tests exercise the same device a
//! developer runs by hand. Everything the device answers is derived from the
//! instruction catalogs (see the `simulator` module docs); `simulator_config.rs`
//! guards the config against them.
#![cfg(feature = "simulator")]

use std::sync::{Arc, Once};
use std::time::Duration;

use sismatic_core::devices::config::DeviceConfig;
use sismatic_core::devices::connector::{ConnectError, Connector};
use sismatic_core::devices::device::Device;
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_core::protocol::instructions::commands::Command;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_core::protocol::instructions::register::Register;
use sismatic_core::protocol::{RecordingState, Value};
use sismatic_core::simulator::{DeviceState, ReplyStream, SimulatedDevice, bind};
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry};

const CONFIG: &str = include_str!("fixtures/device.yaml");

const USERNAME: &str = "admin";
const PASSWORD: &str = "extron";
/// The unit name the config gives the device. Asserted rather than hardcoded
/// twice: if the fixture changes, this constant is the only thing to update.
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
/// config. Returns once it is listening.
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
        command_timeout: Duration::from_secs(5),
        eager: false,
        sis_keepalive: None,
        eager_retry: None,
        cold_backoff: None,
    }
}

/// Drive an instruction through the same `Device` stack production uses, so the
/// whole path — RusshConnector, Controller, the real parser — runs over SSH.
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

/// A typed query whose reply is not free text, proving the config's wire text
/// survives the real parser over a real connection and not just in
/// `simulator_config.rs`.
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
/// read address; nothing but this test forces them to agree.
///
/// Each register gets a distinct probe value, so a write that lands in the
/// wrong cell shows up as a mismatch rather than passing by coincidence.
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

/// Config is the *initial* state: a command mutates the device, and the change
/// is visible to a later query. Also pins the simulator's command → state-code
/// mapping against `Query::RunningState`'s parser, which is the one place those
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
