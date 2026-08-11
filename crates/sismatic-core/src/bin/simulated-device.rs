//! A standalone simulated Extron SMP: an SSH server whose behavior is declared
//! by a YAML config file and adjusted from the command line.
//!
//! ```text
//! # validate the built-in config against the instruction catalog, then exit
//! simulated-device --check-config
//!
//! # serve the built-in config on 127.0.0.1:2222
//! simulated-device
//!
//! # a base unit: no Dual Mode, no Confidence stream
//! simulated-device --licensed-streams 1
//!
//! # someone else's unit, with two fields overridden
//! simulated-device --config ./lecture-hall.yaml \
//!   --field unit-name='Lecture Hall 2' --field running-state=1
//!
//! # what all of that actually resolved to
//! simulated-device --licensed-streams 1 --print-config
//! ```
//!
//! The server logic is [`sismatic_core::simulator`], shared verbatim with the
//! integration tests, so anything this binary answers is what the tests
//! exercise.
//!
//! # Where the configuration comes from
//!
//! Three layers, each beating the one before it: the built-in config, then a
//! `--config` file, then the individual flags. The merge itself lives in
//! [`DeviceState::from_yaml_str_with`] rather than here, so the integration
//! tests exercise the same precedence an operator gets.
//!
//! The built-in layer is the checked-in fixture, compiled in rather than read
//! from a path relative to the workspace root. That is what lets the binary run
//! from any directory, and it means `--check-config` with no arguments checks
//! something real.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use sismatic_core::simulator::{
    DeviceState, Overrides, ReplyStream, bind, canonical_name, catalog_streams,
};
use tracing_subscriber::EnvFilter;

/// The config served when `--config` is not given.
///
/// The same bytes `simulator_config.rs` runs its drift guards over, so the
/// device this binary serves by default is the device the test suite pins.
const BUILT_IN_CONFIG: &str = include_str!("../../tests/fixtures/device.yaml");

#[derive(Parser, Debug)]
#[command(
    name = "simulated-device",
    about = "A declaratively configured, simulated Extron SMP over SSH",
    after_help = "Flags override the config file, which overrides the built-in config.\n\
                  Use --print-config to see what a given combination resolves to."
)]
struct Args {
    /// Device config file (YAML). Defaults to the built-in config.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Validate the config against the instruction catalog and exit.
    #[arg(long)]
    check_config: bool,

    /// Print the resolved configuration and exit, without binding a port.
    #[arg(long)]
    print_config: bool,

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

    #[arg(short, long, env = "RUST_LOG", default_value = "info")]
    log_level: String,

    /// SSH username to accept, overriding the config.
    #[arg(short, long)]
    username: Option<String>,

    /// SSH password to accept, overriding the config.
    #[arg(short = 'P', long)]
    password: Option<String>,

    /// Streams this unit is licensed for, as a comma-separated list of indices
    /// — `1` for a base unit, `1,2` with Dual Mode, `all` for a fully-loaded
    /// one, `none` for a unit with no streams at all.
    ///
    /// Every field addressing a stream outside the list is refused with a SIS
    /// error rather than answered, which is what an Extron SMP does with an
    /// unlicensed stream. Replaces the config's list rather than adding to it.
    #[arg(long, value_name = "LIST", value_parser = parse_streams)]
    licensed_streams: Option<BTreeSet<u8>>,

    /// Refuse this field by name, whatever stream it belongs to. Repeatable.
    /// Replaces the config's list rather than adding to it.
    #[arg(long, value_name = "FIELD", value_parser = parse_field_name)]
    unlicensed_field: Vec<String>,

    /// The SIS error code refusals are answered with, without the `E`.
    #[arg(long, value_name = "CODE")]
    refusal_code: Option<u8>,

    /// Seed one field's initial value, as `NAME=VALUE`. Repeatable. Names are
    /// canonicalised through the instruction catalog, so `--field ssh-port=22`
    /// and `--field SSH_PORT=22` are the same field.
    #[arg(long, value_name = "NAME=VALUE", value_parser = parse_field_assignment)]
    field: Vec<(String, String)>,
}

/// Parse `--licensed-streams`. See the flag's own help for the accepted forms.
fn parse_streams(raw: &str) -> Result<BTreeSet<u8>, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
        return Ok(BTreeSet::new());
    }
    if raw.eq_ignore_ascii_case("all") {
        return Ok(catalog_streams());
    }
    raw.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<u8>()
                .map_err(|_| format!("`{part}` is not a stream index"))
        })
        .collect()
}

/// Resolve a field name through the catalog, so a typo is a parse error with
/// the misspelling in it rather than a flag that silently vetoes nothing.
fn parse_field_name(raw: &str) -> Result<String, String> {
    canonical_name(raw).ok_or_else(|| format!("`{raw}` is not a field in the instruction catalog"))
}

/// Parse `NAME=VALUE`, validating the name the same way.
fn parse_field_assignment(raw: &str) -> Result<(String, String), String> {
    let (name, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected NAME=VALUE, got `{raw}`"))?;
    Ok((parse_field_name(name.trim())?, value.to_owned()))
}

impl Args {
    /// Where the config text came from, for error messages and `--check-config`
    /// output. The built-in config has no path, so it is named.
    fn config_label(&self) -> String {
        match &self.config {
            Some(path) => path.display().to_string(),
            None => "<built-in config>".to_owned(),
        }
    }

    fn overrides(&self) -> Overrides {
        Overrides {
            username: self.username.clone(),
            password: self.password.clone(),
            licensed_streams: self.licensed_streams.clone(),
            // An empty `--unlicensed-field` list means the flag was not given,
            // so the config's list stands. Clearing a config's list is
            // `--unlicensed-field` with nothing to say, which no operator means
            // — and a unit that refuses nothing is `--licensed-streams all`.
            unlicensed_fields: (!self.unlicensed_field.is_empty())
                .then(|| self.unlicensed_field.iter().cloned().collect()),
            refusal_code: self.refusal_code,
            fields: self.field.iter().cloned().collect::<BTreeMap<_, _>>(),
        }
    }

    fn reply_stream(&self) -> ReplyStream {
        if self.stdout {
            ReplyStream::Stdout
        } else {
            ReplyStream::Stderr
        }
    }
}

/// What the three layers resolved to, printed by `--print-config`.
///
/// The password is printed rather than masked: this is a device that exists to
/// be connected to, and the credential it will accept is the thing an operator
/// came here to find out.
fn print_resolved(args: &Args, state: &DeviceState) {
    let license = state.license();
    let credentials = state.credentials();
    let refused = license.refused_fields();

    println!("config:            {}", args.config_label());
    println!("listen:            {}:{}", args.host, args.port);
    println!("reply stream:      {:?}", args.reply_stream());
    println!("username:          {}", credentials.username);
    println!("password:          {}", credentials.password);
    println!(
        "licensed streams:  {}",
        if license.streams().is_empty() {
            "none".to_owned()
        } else {
            license
                .streams()
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    println!("refusal code:      E{:02}", license.code());
    println!("refused fields:    {}", refused.len());
    for field in &refused {
        println!("  - {field}");
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    // Built from the CLI string, which `RUST_LOG` also feeds. Falls back to
    // "info" rather than failing: a mistyped filter should not stop the device
    // from starting.
    let filter = EnvFilter::try_new(&args.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let text = match &args.config {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("cannot read {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None => BUILT_IN_CONFIG.to_owned(),
    };

    let label = args.config_label();
    let state = match DeviceState::from_yaml_str_with(&text, &args.overrides()) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("cannot parse {label}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if args.print_config {
        print_resolved(&args, &state);
        return ExitCode::SUCCESS;
    }

    let issues = state.check();
    for issue in &issues {
        eprintln!("{label}: {issue}");
    }

    if args.check_config {
        return if issues.is_empty() {
            println!("{label}: ok");
            ExitCode::SUCCESS
        } else {
            eprintln!("{} issue(s)", issues.len());
            ExitCode::FAILURE
        };
    }

    // Serve anyway when there are issues: a missing field just goes unanswered
    // (the client sees a command timeout), which is more useful to debug
    // against than a device that refuses to start.
    let device = match bind(
        (args.host.as_str(), args.port),
        Arc::new(state),
        args.reply_stream(),
    )
    .await
    {
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
