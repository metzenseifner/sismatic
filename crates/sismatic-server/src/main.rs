//! The composition root: read the two config files, then hand the resolved
//! values to [`sismatic_server::run`], which starts both sides of the system on
//! one Tokio runtime — the `sync` write-side task set ([`sismatic_sync::spawn`])
//! and, eventually, the read-side http-api.
//!
//! Every impure step lives here: the server config read, and the device config
//! read that `devices_config_path` names. Both are one line each, which is the
//! point — the logic they feed is pure and unit-tested elsewhere.

use sismatic_core::devices::config;
use sismatic_server::configuration::get_configuration;
use sismatic_server::run;

/// Server config used when `SISMATIC_SERVER_CONFIG` names none.
const DEFAULT_CONFIG_PATH: &str = "configuration.yaml";

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let config_path =
        std::env::var("SISMATIC_SERVER_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_owned());

    let cfg = get_configuration(&config_path)
        .unwrap_or_else(|e| panic!("reading server config {config_path}: {e}"));

    let devices = config::load(&cfg.devices_config_path).unwrap_or_else(|e| {
        panic!(
            "loading devices config {}: {e}",
            cfg.devices_config_path.display()
        )
    });

    run(cfg, devices, async {
        tokio::signal::ctrl_c().await.expect("ctrl-c handler")
    })
    .await
}
