//! Black-box test against a real Extron device over SSH.
//!
//! Opt-in: the whole file is empty unless the `ssh` feature is on, and the test
//! itself skips (and passes) unless `SMP_TEST_HOST`, `SMP_TEST_USER`, and
//! `SMP_TEST_PASS` are all set. To run it against real hardware:
//!
//! ```sh
//! SMP_TEST_HOST=10.0.0.7 SMP_TEST_USER=admin SMP_TEST_PASS=extron \
//!     cargo test --features ssh --test real_ssh -- --nocapture
//! ```
#![cfg(feature = "ssh")]

use std::sync::Arc;
use std::time::Duration;

use sismatic_core::devices::config::DeviceConfig;
use sismatic_core::devices::registry::Registry;
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_core::protocol::Value;
use sismatic_core::protocol::instructions::query::Query;

/// Returns the device credentials, or `None` if the test should be skipped.
fn device_from_env() -> Option<(String, String, String)> {
    Some((
        std::env::var("SMP_TEST_HOST").ok()?,
        std::env::var("SMP_TEST_USER").ok()?,
        std::env::var("SMP_TEST_PASS").ok()?,
    ))
}

#[tokio::test]
async fn queries_firmware_over_real_ssh() {
    let Some((host, user, pass)) = device_from_env() else {
        eprintln!("SMP_TEST_HOST/USER/PASS not all set; skipping real-device test");
        return;
    };

    let port: u16 = std::env::var("SMP_TEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(22023);
    let configs = vec![DeviceConfig {
        id: "real".into(),
        host,
        port,
        username: user,
        password: pass,
        connect_timeout: Duration::from_secs(10),
        command_timeout: Duration::from_secs(5),
        eager: false,
        sis_keepalive: None,
    }];
    let registry = Registry::from_configs(configs, Arc::new(RusshConnector));
    let device = registry.device("real").expect("device present");

    let firmware = device
        .run(&Query::Firmware.instruction())
        .await
        .expect("firmware query succeeds");

    println!("firmware = {firmware}");
    assert!(
        matches!(firmware, Value::Version(_)),
        "expected a firmware version, got {firmware:?}"
    );
}
