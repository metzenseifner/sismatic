//! The simulated SMP's license, exercised over a real SSH connection against
//! the production client.
//!
//! An Extron SMP sells its second and third stream as licensed options, and a
//! unit without the license does not answer those fields with a default — it
//! refuses them, every time, with a SIS error code. The client has a whole
//! mechanism built on that behaviour (`auto_disable_after` counts consecutive
//! refusals and takes the field out of the schedule), and until now there was
//! no way to rehearse it without a base unit on the bench.
//!
//! So these tests assert the pairing rather than the bytes: what the simulator
//! puts on the wire for an unlicensed field is what
//! [`ControllerError::Rejected`] is built from, which is the signal that whole
//! mechanism reads. `simulator/state.rs` unit-tests the resolution itself; this
//! file is about the two halves meeting over SSH.
#![cfg(feature = "simulator")]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use sismatic_core::devices::config::{DeviceConfig, Uuid};
use sismatic_core::devices::controller::ControllerError;
use sismatic_core::devices::device::{Device, DeviceError};
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_core::protocol::instructions::setting::Setting;
use sismatic_core::protocol::{SisError, Value};
use sismatic_core::simulator::{DeviceState, Overrides, ReplyStream, SimulatedDevice, bind};

/// The checked-in config, which licenses all three streams. Every test here
/// narrows it through [`Overrides`] — the same merge the binary's
/// `--licensed-streams` flag goes through.
const CONFIG: &str = include_str!("fixtures/device.yaml");

const USERNAME: &str = "admin";
const PASSWORD: &str = "extron";

/// A simulated SMP licensed for `streams`, bound on a random loopback port.
async fn spawn_licensed_for(streams: [u8; 1]) -> SimulatedDevice {
    spawn_with(Overrides {
        licensed_streams: Some(streams.into_iter().collect()),
        ..Overrides::default()
    })
    .await
}

async fn spawn_with(overrides: Overrides) -> SimulatedDevice {
    let state = DeviceState::from_yaml_str_with(CONFIG, &overrides).expect("the config parses");
    bind(("127.0.0.1", 0), Arc::new(state), ReplyStream::Stderr)
        .await
        .expect("bind loopback")
}

/// A client pointed at `simulated_device`, with its own field veto switched off
/// so every query reaches the wire. That is the whole point here: the refusal
/// under test is the *device's*, and a client-side `disabled_fields` would
/// short-circuit it before the SSH connection was ever used.
fn connected(simulated_device: &SimulatedDevice) -> Device {
    let config = DeviceConfig {
        id: "sim".into(),
        host: "127.0.0.1".into(),
        port: simulated_device.port(),
        username: USERNAME.into(),
        password: PASSWORD.into(),
        connect_timeout: Duration::from_secs(5),
        exchange_timeout: Duration::from_secs(5),
        eager: false,
        sis_keepalive: None,
        eager_retry: None,
        cold_backoff: None,
        uuid: Uuid::nil(),
        disabled_fields: BTreeSet::new(),
        auto_disable_after: 0,
        self_heal: None,
    }
    .derive_uuid();

    Device::new(config, Arc::new(RusshConnector))
}

/// The SIS error a failed exchange carried, or a panic naming what it was
/// instead. A refusal must arrive as `Rejected` and not as a timeout: the two
/// are the distinction the device layer turns on, and a simulator that stayed
/// silent would look like a broken connection rather than a declined field.
fn refusal(result: Result<Value, DeviceError>, what: &str) -> SisError {
    match result {
        Err(DeviceError::Command(ControllerError::Rejected { error, .. })) => error,
        Err(other) => panic!("{what} should have been refused, got {other:?}"),
        Ok(value) => panic!("{what} should have been refused, but answered {value:?}"),
    }
}

/// A base unit — one stream, no Dual Mode and no Confidence — refuses the
/// fields of the streams it does not have, and answers the one it does.
#[tokio::test]
async fn a_base_unit_refuses_stream_two_and_three_but_answers_stream_one() {
    let simulated_device = spawn_licensed_for([1]).await;
    let device = connected(&simulated_device);

    // Stream 1 is licensed and configured, so it answers normally.
    assert_eq!(
        device
            .run(&Query::Stream1Name.instruction())
            .await
            .expect("stream 1 is licensed"),
        Value::Text("Archive Channel A".into())
    );

    // Stream 2 and 3 are refused, with the code a real unit sends.
    for query in [Query::Stream2Name, Query::Stream3Name] {
        let error = refusal(device.run(&query.instruction()).await, &query.to_string());
        assert_eq!(error, SisError { code: 13 });
        assert_eq!(
            error.description(),
            Some("invalid parameter (number is out of range)")
        );
    }
}

/// The license covers the RTMP fields of an unlicensed stream too, not just the
/// `STREAM_n_*` pair — a stream the unit does not have cannot have a push
/// target either.
#[tokio::test]
async fn a_base_unit_refuses_the_rtmp_fields_of_an_unlicensed_stream() {
    let simulated_device = spawn_licensed_for([1]).await;
    let device = connected(&simulated_device);

    assert_eq!(
        device
            .run(&Query::RtmpStream1LiveState.instruction())
            .await
            .expect("stream 1 is licensed"),
        Value::Flag(true)
    );

    for query in [
        Query::RtmpStream2LiveState,
        Query::RtmpBackupStream2LiveState,
        Query::RtmpStream3LiveState,
        Query::RtmpBackupStream3LiveState,
    ] {
        let error = refusal(device.run(&query.instruction()).await, &query.to_string());
        assert_eq!(error, SisError { code: 13 }, "{query}");
    }
}

/// A write to an unlicensed stream is refused as well, and the refusal is the
/// same shape as a read's. A unit without Dual Mode will not be talked into
/// naming its second stream.
#[tokio::test]
async fn a_write_to_an_unlicensed_stream_is_refused() {
    let simulated_device = spawn_licensed_for([1]).await;
    let device = connected(&simulated_device);

    let instruction = Setting::Stream2Name
        .instruction("Lecture Hall B")
        .expect("a legal stream name");
    let error = refusal(device.run(&instruction).await, "a stream 2 name write");
    assert_eq!(error, SisError { code: 13 });

    // And the write did not land: stream 2 is still refused rather than
    // answering the value that was pushed at it.
    let error = refusal(
        device.run(&Query::Stream2Name.instruction()).await,
        "a stream 2 name read after the refused write",
    );
    assert_eq!(error, SisError { code: 13 });
}

/// Dual Mode without the Confidence stream — the middle license, which proves
/// the veto is per-index rather than "stream 1 only" or "all of them".
#[tokio::test]
async fn a_dual_mode_unit_answers_stream_two_and_refuses_stream_three() {
    let simulated_device = spawn_with(Overrides {
        licensed_streams: Some([1, 2].into_iter().collect()),
        ..Overrides::default()
    })
    .await;
    let device = connected(&simulated_device);

    assert_eq!(
        device
            .run(&Query::Stream2Name.instruction())
            .await
            .expect("stream 2 is licensed under Dual Mode"),
        Value::Text("Archive Channel B".into())
    );

    let error = refusal(
        device.run(&Query::Stream3Name.instruction()).await,
        "stream 3 on a Dual Mode unit",
    );
    assert_eq!(error, SisError { code: 13 });
}

/// A fully-loaded unit refuses nothing, which is what the checked-in config
/// describes and what the rest of the integration suite assumes.
#[tokio::test]
async fn a_fully_loaded_unit_answers_every_stream() {
    let simulated_device = spawn_with(Overrides::default()).await;
    let device = connected(&simulated_device);

    for (query, expected) in [
        (Query::Stream1Name, "Archive Channel A"),
        (Query::Stream2Name, "Archive Channel B"),
        (Query::Stream3Name, "Confidence"),
    ] {
        assert_eq!(
            device
                .run(&query.instruction())
                .await
                .unwrap_or_else(|e| panic!("{query} should be answered: {e:?}")),
            Value::Text(expected.into()),
            "{query}"
        );
    }
}

/// The by-name veto reaches a field that belongs to no stream at all — the
/// device-side mirror of the client's own `disabled_fields`.
#[tokio::test]
async fn an_unlicensed_field_is_refused_whatever_stream_it_belongs_to() {
    let simulated_device = spawn_with(Overrides {
        unlicensed_fields: Some(["SNMP_STATE".to_owned()].into_iter().collect()),
        ..Overrides::default()
    })
    .await;
    let device = connected(&simulated_device);

    let error = refusal(
        device.run(&Query::SnmpState.instruction()).await,
        "a by-name vetoed field",
    );
    assert_eq!(error, SisError { code: 13 });

    // A neighbouring SNMP field is untouched: the veto is by field, not by
    // family.
    assert!(
        device
            .run(&Query::SnmpUnitLocation.instruction())
            .await
            .is_ok()
    );
}

/// The refusal code is configurable, and an undocumented one still round-trips
/// as itself — the client keeps the digits rather than mapping them onto a
/// meaning it cannot attest.
#[tokio::test]
async fn a_configured_refusal_code_arrives_as_itself() {
    let simulated_device = spawn_with(Overrides {
        licensed_streams: Some([1].into_iter().collect()),
        refusal_code: Some(14),
        ..Overrides::default()
    })
    .await;
    let device = connected(&simulated_device);

    let error = refusal(
        device.run(&Query::Stream2Name.instruction()).await,
        "stream 2 on a unit answering E14",
    );
    assert_eq!(error, SisError { code: 14 });
    assert_eq!(
        error.description(),
        Some("not valid for this configuration")
    );
}

/// Refusals repeat. The field-veto design note turns on a device that "keeps
/// saying so", and `auto_disable_after` counts *consecutive* refusals — so a
/// simulator that refused once and then answered would make the mechanism it
/// exists to rehearse untestable.
#[tokio::test]
async fn a_refusal_is_not_a_one_off() {
    let simulated_device = spawn_licensed_for([1]).await;
    let device = connected(&simulated_device);

    for attempt in 1..=3 {
        let error = refusal(
            device.run(&Query::Stream2Name.instruction()).await,
            &format!("stream 2, attempt {attempt}"),
        );
        assert_eq!(error, SisError { code: 13 }, "attempt {attempt}");
    }
}
