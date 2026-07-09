//! Command-line front-end over [`sismatic_core`].
//!
//! Loads a device pool from a `devices.toml` and runs one instruction against a
//! named device, printing the decoded value. This is a thin adapter: all the
//! protocol and connection logic lives in the core crate.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};

use sismatic_core::devices::registry::Registry;
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_core::protocol::Value;
use sismatic_core::protocol::instructions::Instruction;
use sismatic_core::protocol::instructions::commands::Command as SisCommand;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_core::protocol::instructions::register::Register;

#[derive(Parser)]
#[command(
    name = "sismatic",
    version,
    about = "Drive Extron SIS devices over SSH"
)]
struct Cli {
    /// Path to the devices.toml describing the device pool.
    #[arg(short, long, default_value = "devices.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// List the ids of every configured device.
    Ids,
    /// Read a built-in field from a device (e.g. `firmware`, `ssh_port`).
    Query { device: String, name: String },
    /// Run a recorder command on a device (e.g. `start`, `stop`, `pause`).
    Command { device: String, name: String },
    /// Write a value into a metadata register on a device (e.g. `title`).
    Register {
        device: String,
        name: String,
        value: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let registry = Registry::load(&cli.config, Arc::new(RusshConnector))
        .with_context(|| format!("loading {}", cli.config.display()))?;

    match cli.action {
        Action::Ids => {
            let mut ids = registry.ids();
            ids.sort();
            for id in ids {
                println!("{id}");
            }
        }
        Action::Query { device, name } => {
            let instruction = Query::from_str(&name)?.instruction();
            println!("{}", run(&registry, &device, instruction).await?);
        }
        Action::Command { device, name } => {
            let instruction = SisCommand::from_str(&name)?.instruction();
            println!("{}", run(&registry, &device, instruction).await?);
        }
        Action::Register {
            device,
            name,
            value,
        } => {
            let instruction = Register::from_str(&name)?.instruction(&value);
            println!("{}", run(&registry, &device, instruction).await?);
        }
    }

    Ok(())
}

/// Look up `device` in the registry and run one instruction against it.
async fn run(registry: &Registry, device: &str, instruction: Instruction) -> Result<Value> {
    let device = registry
        .device(device)
        .ok_or_else(|| anyhow!("unknown device '{device}'"))?;
    Ok(device.run(&instruction).await?)
}
