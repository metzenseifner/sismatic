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

use sismatic_core::devices::config;
use sismatic_core::devices::registry::{Registry, Target};
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
    /// List the ids of every configured device group.
    Groups,
    /// Read a built-in field from a device or group (e.g. `firmware`, `ssh_port`).
    Query { target: String, name: String },
    /// Run a recorder command on a device or group (e.g. `start`, `stop`, `pause`).
    Command { target: String, name: String },
    /// Write a value into a metadata register on a device or group (e.g. `title`).
    Register {
        target: String,
        name: String,
        value: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let resolved =
        config::load(&cli.config).with_context(|| format!("loading {}", cli.config.display()))?;
    let registry = Registry::build(resolved.devices, resolved.groups, Arc::new(RusshConnector));

    match cli.action {
        Action::Ids => {
            let mut ids = registry.ids();
            ids.sort();
            for id in ids {
                println!("{id}");
            }
        }
        Action::Groups => {
            let mut ids = registry.group_ids();
            ids.sort();
            for id in ids {
                println!("{id}");
            }
        }
        Action::Query { target, name } => {
            let instruction = Query::from_str(&name)?.instruction();
            run(&registry, &target, instruction).await?;
        }
        Action::Command { target, name } => {
            let instruction = SisCommand::from_str(&name)?.instruction();
            run(&registry, &target, instruction).await?;
        }
        Action::Register {
            target,
            name,
            value,
        } => {
            let instruction = Register::from_str(&name)?.instruction(&value);
            run(&registry, &target, instruction).await?;
        }
    }

    Ok(())
}

/// Resolve `target` to a device or group, run one instruction, and print the
/// result. A single device prints just its value; a group prints one
/// `device-id: value` line per member so the fan-out is visible.
async fn run(registry: &Registry, target: &str, instruction: Instruction) -> Result<()> {
    match registry
        .target(target)
        .ok_or_else(|| anyhow!("unknown device or group '{target}'"))?
    {
        Target::Device(device) => {
            let value: Value = device.run(&instruction).await?;
            println!("{value}");
        }
        Target::Group(group) => {
            let results = group.run(&instruction).await?;
            for (id, value) in results {
                println!("{id}: {value}");
            }
        }
    }
    Ok(())
}
