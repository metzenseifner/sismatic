//! A standalone simulated Extron SMP: an SSH server whose behaviour is
//! declared by a YAML config file.
//!
//! ```text
//! # validate the config against the instruction catalog, then exit
//! simulated-device --check-config
//!
//! # serve on 127.0.0.1:2222 using the checked-in fixture
//! simulated-device --port 2222
//! ```
//!
//! The server logic is [`sismatic_core::simulator`], shared verbatim with the
//! integration tests, so anything this binary answers is what the tests
//! exercise.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use sismatic_core::simulator::{DeviceState, ReplyStream, bind};

/// Default config path, relative to the workspace root — where `cargo run`
/// puts the working directory.
const DEFAULT_CONFIG: &str = "crates/sismatic-core/tests/fixtures/device.yaml";

#[derive(Parser, Debug)]
#[command(
    name = "simulated-device",
    about = "A declaratively configured, simulated Extron SMP over SSH"
)]
struct Args {
    /// Device config file (YAML).
    #[arg(short, long, default_value = DEFAULT_CONFIG)]
    config: PathBuf,

    /// Validate the config against the instruction catalog and exit.
    #[arg(long)]
    check_config: bool,

    /// Port to listen on. 0 lets the kernel choose.
    #[arg(short, long, default_value_t = 2222)]
    port: u16,

    /// Address to bind.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Answer on stdout instead of the extended-data (stderr) stream that real
    /// Extron hardware uses.
    #[arg(long)]
    stdout: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let text = match std::fs::read_to_string(&args.config) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("cannot read {}: {e}", args.config.display());
            return ExitCode::FAILURE;
        }
    };
    let state = match DeviceState::from_yaml_str(&text) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("cannot parse {}: {e}", args.config.display());
            return ExitCode::FAILURE;
        }
    };

    let issues = state.check();
    for issue in &issues {
        eprintln!("{}: {issue}", args.config.display());
    }

    if args.check_config {
        return if issues.is_empty() {
            println!("{}: ok", args.config.display());
            ExitCode::SUCCESS
        } else {
            eprintln!("{} issue(s)", issues.len());
            ExitCode::FAILURE
        };
    }

    // Serve anyway when there are issues: a missing field just goes unanswered
    // (the client sees a command timeout), which is more useful to debug
    // against than a device that refuses to start.
    let reply = if args.stdout {
        ReplyStream::Stdout
    } else {
        ReplyStream::Stderr
    };
    let device = match bind((args.host.as_str(), args.port), Arc::new(state), reply).await {
        Ok(device) => device,
        Err(e) => {
            eprintln!("cannot bind {}:{}: {e}", args.host, args.port);
            return ExitCode::FAILURE;
        }
    };

    tracing::info!(host = %args.host, port = device.port(), "simulated device listening");
    device.serve_forever().await;
    ExitCode::SUCCESS
}
