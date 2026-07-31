//! The composition root: read the two config files, then hand the resolved
//! values to [`sismatic_server::run`], which starts both sides of the system on
//! one Tokio runtime — the `sync` write-side task set ([`sismatic_sync::spawn`])
//! and the read-side http-api ([`sismatic_http_api::run`]).
//!
//! Every impure step lives here: the server config read, the environment read
//! that helps locate it, and the device config read that `devices_config_path`
//! names. Each is one line — the point being that the logic they feed is pure
//! and unit-tested, [`resolve_config_path`] included.
//!
//! Which file to read is itself a three-layer fallback, most specific first:
//!
//! 1. `--config`, for a one-off run or a systemd unit that names its own file,
//! 2. `SISMATIC_SERVER__CONFIG`, for an environment that pins one
//!    deployment-wide,
//! 3. `configuration.yaml`, so neither has to be named at all.
//!
//! That mirrors the precedence the file's *contents* resolve by (see
//! [`sismatic_server::configuration`]): the more specific layer wins, and every
//! layer is optional.
//!
//! `SISMATIC_SERVER__CONFIG` is spelled like every other variable — the prefix,
//! then a key path — but it is the one this binary reads by hand, because it is
//! the one that cannot be a config key: it names *which* document to load, and
//! so must be answered before there is a document to look in. It comes from
//! [`CONFIG_PATH_ENV`] rather than a literal here, because the loader has to
//! exclude precisely the variable this reads, and two spellings of one name
//! would eventually disagree.
//!
//! Because every layer is optional, the file can be missing entirely — and that
//! one failure is answered by the help text rather than by a panic, since help
//! is what lists the three layers. See [`exit_with_help`].
//!
//! `--host` and `--port` sit *above* a config that has already been fully
//! resolved — file and environment both — so they are folded in here rather
//! than in the resolver; see [`HttpConfig::with_overrides`]. Every flag is
//! therefore an `Option` with no clap-side default: a default supplied by clap
//! would be indistinguishable from one the operator typed, and would outrank
//! everything below it on every run.
//!
//! The same reasoning keeps clap's `env` attribute off these flags. The
//! environment already reaches these two settings as
//! `SISMATIC_SERVER__HTTP__HOST` and `SISMATIC_SERVER__HTTP__PORT`, read
//! alongside every other key by the loader; a clap-side `env` would give one
//! variable two readers that must agree, and would still leave every key
//! *without* a flag — `sync.interval_secs`, `devices_config_path` — needing the
//! other mechanism anyway. One mechanism, one derivation rule. So the full
//! precedence for a host or a port is:
//!
//! 1. `--host` / `--port`,
//! 2. `SISMATIC_SERVER__HTTP__HOST` / `SISMATIC_SERVER__HTTP__PORT`,
//! 3. `http:` in the file, then `defaults:`,
//! 4. the built-in constant.
//!
//! [`CONFIG_PATH_ENV`]: sismatic_server::configuration::CONFIG_PATH_ENV
//! [`HttpConfig::with_overrides`]: sismatic_server::configuration::HttpConfig::with_overrides

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use sismatic_server::telemetry::{get_subscriber, init_subscriber};

use clap::{CommandFactory, Parser};
use sismatic_core::devices::config;
use sismatic_server::configuration::{CONFIG_PATH_ENV, ServerConfig, get_configuration};
use sismatic_server::run;
use tracing::{info, instrument};

#[derive(Parser, Debug)]
#[command(version, author="Jonathan L. Komar", about, long_about = None)]
struct Args {
    /// Path to configuration file e.g. configuration.yaml
    /// [env: SISMATIC_SERVER__CONFIG] [default: configuration.yaml]
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Host to serve on e.g. 127.0.0.1
    /// [env: SISMATIC_SERVER__HTTP__HOST] [default: from config, else 127.0.0.1]
    #[arg(short = 'H', long)]
    host: Option<String>,

    /// Port to serve on e.g. 8080
    /// [env: SISMATIC_SERVER__HTTP__PORT] [default: from config, else 8080]
    #[arg(short, long)]
    port: Option<u16>,
}

/// Which server config file to read. Pure and total: the environment is read by
/// the caller and passed in, so the precedence rule is a function of its
/// arguments and testable without touching the process environment.
fn resolve_config_path(arg: Option<PathBuf>, env: Option<OsString>) -> PathBuf {
    /// Server config used when neither `--config` nor `SISMATIC_SERVER__CONFIG`
    /// names one.
    const DEFAULT_CONFIG_PATH: &str = "configuration.yaml";

    arg.or_else(|| env.map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
}

/// Report a usage problem the way clap reports a bad flag — the message, then
/// the full help, on stderr — and exit without starting anything.
///
/// Reserved for the one failure the help text actually answers: no config file
/// where we looked. Help lists `--config`, its environment variable and its
/// default, which is exactly the missing knowledge. Every *other* failure here
/// still panics, because help cannot tell an operator what is wrong with a file
/// that parsed badly.
fn exit_with_help(message: impl fmt::Display) -> ! {
    /// clap's exit code for a usage error, kept distinct from the `1` a server
    /// that started and then failed would produce.
    const USAGE: i32 = 2;

    // Both streams are stderr so that stdout stays clean for a caller piping
    // this binary, and so the message cannot interleave behind the help.
    eprintln!("error: {message}\n");
    eprint!("{}", Args::command().render_help());
    std::process::exit(USAGE)
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let subscriber = get_subscriber("sismatic-server".into(), "info".into());
    init_subscriber(subscriber);
    let args = Args::parse();

    // Read by hand rather than through the config layer, because it decides
    // which document that layer reads. The name comes from `configuration` so
    // the one variable answered here and the one dropped there cannot drift.
    let config_path = resolve_config_path(args.config, std::env::var_os(CONFIG_PATH_ENV));

    // Asked before the read rather than recovered from it afterwards: the
    // `config` crate reports a missing file as a `Foreign` error wrapping an
    // `io::Error`, so telling it apart from a parse failure would mean
    // downcasting through the error type. Asking the filesystem states the
    // distinction that matters instead of inferring it.
    match config_path.try_exists() {
        Ok(true) => {}
        Ok(false) => exit_with_help(format_args!("no config file at {}", config_path.display())),
        Err(e) => exit_with_help(format_args!("cannot reach {}: {e}", config_path.display())),
    }

    let cfg = get_configuration(&config_path)
        .unwrap_or_else(|e| panic!("reading server config {}: {e}", config_path.display()));

    // The one layer neither the file nor the environment can express: flags the
    // operator typed. Folded in here rather than inside `resolve_config` so that
    // resolver stays a function of the document alone.
    let cfg = ServerConfig {
        http: cfg.http.with_overrides(args.host, args.port),
        ..cfg
    };

    let devices = config::load(&cfg.devices_config_path).unwrap_or_else(|e| {
        panic!(
            "loading devices config {}: {e}",
            cfg.devices_config_path.display()
        )
    });

    run(cfg, devices, shutdown_signal()).await
}

/// Wait for the operator's interrupt — the process's one shutdown trigger.
///
/// A named function rather than the inline `async` block it replaces, purely so
/// [`macro@instrument`] has an item to attach to. The span it opens covers the
/// *wait*, so it is entered at startup and closed when the wait ends, whichever
/// way it ends: either ctrl-c arrived, or `run`'s `select!` dropped this future
/// because the http server went down first. That is why the arrival is also an
/// event — the span's close says the wait is over, the event inside it says a
/// signal is what ended it.
///
/// No `skip`: there are no arguments to record, so the span carries the
/// function's location and nothing else.
#[instrument(name = "await_shutdown_signal")]
async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.expect("ctrl-c handler");
    info!("ctrl-c received");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(arg: Option<&str>, env: Option<&str>) -> PathBuf {
        resolve_config_path(arg.map(PathBuf::from), env.map(OsString::from))
    }

    #[test]
    fn the_cli_argument_wins_over_every_layer_below_it() {
        assert_eq!(
            resolve(Some("/etc/one-off.yaml"), Some("/etc/deployment.yaml")),
            PathBuf::from("/etc/one-off.yaml")
        );
    }

    #[test]
    fn the_environment_answers_when_the_cli_names_nothing() {
        assert_eq!(
            resolve(None, Some("/etc/deployment.yaml")),
            PathBuf::from("/etc/deployment.yaml")
        );
    }

    #[test]
    fn the_built_in_default_answers_when_neither_layer_does() {
        assert_eq!(resolve(None, None), PathBuf::from("configuration.yaml"));
    }
}
