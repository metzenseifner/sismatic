//! Loading and resolving device definitions from a `devices.toml`.
//!
//! The file has an optional `[defaults]` table and a list of `[[device]]`
//! tables. Each device inherits every default it does not set itself, so a
//! nearby device can be a few lines while a far one overrides only the timeouts
//! it needs:
//!
//! ```toml
//! [defaults]
//! port = 22023
//! connect_secs = 5
//! command_secs = 3
//! eager = true       # connect to every device at startup and keep it warm
//! sis_keepalive_secs = 120  # re-issue `Q` this often; 0 disables the SIS keepalive
//!
//! [[device]]
//! id = "atrium-101"
//! host = "10.0.0.7"
//! username = "admin"
//! password = "extron"
//!
//! [[device]]
//! id = "annex-far"
//! host = "10.9.40.12"
//! username = "admin"
//! password = "extron"
//! connect_secs = 20
//! command_secs = 10
//! ```
//!
//! Resolution is a pure function of the file's text: [`parse`] turns a string
//! into fully-resolved [`DeviceConfig`]s, and [`load`] is the thin wrapper that
//! reads the file first. `id` and `host` are the only fields a device must
//! state itself; every other field may come from the device or the defaults.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// The SIS keepalive interval applied when `eager` is on but `sis_keepalive_secs` is
/// left unset. Comfortably under the SMP's default 5-minute idle disconnect, with
/// room for one failed round-trip to self-heal before the window closes.
const DEFAULT_SIS_KEEPALIVE_SECS: u64 = 120;

/// A fully-resolved device: every field has a concrete value, with defaults
/// already folded in. This is what the registry consumes to open a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceConfig {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub connect_timeout: Duration,
    pub command_timeout: Duration,
    /// Open this device's connection at startup and keep it warm, rather than
    /// waiting for the first command. The keep-warm loop is [`sis_keepalive`].
    ///
    /// [`sis_keepalive`]: DeviceConfig::sis_keepalive
    pub eager: bool,
    /// How often to re-issue the `Q` query to reset the SMP's idle-disconnect
    /// timer while eager. `None` means never (a bare `sis_keepalive_secs = 0`), so an
    /// eager connection is warmed once and then left to self-heal. Ignored unless
    /// [`eager`] is set.
    ///
    /// [`eager`]: DeviceConfig::eager
    pub sis_keepalive: Option<Duration>,
}

/// Why a `devices.toml` could not be turned into [`DeviceConfig`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The file could not be read from disk (only produced by [`load`]).
    Io(String),
    /// The text was not valid TOML, or a required `id`/`host` was absent.
    Toml(String),
    /// Two devices share the same `id`, so one would shadow the other.
    DuplicateId(String),
    /// A field was set neither on the device nor in `[defaults]`.
    MissingField { device: String, field: &'static str },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "reading devices file: {e}"),
            ConfigError::Toml(e) => write!(f, "parsing devices file: {e}"),
            ConfigError::DuplicateId(id) => write!(f, "duplicate device id `{id}`"),
            ConfigError::MissingField { device, field } => {
                write!(
                    f,
                    "device `{device}` is missing `{field}` (set it on the device or in [defaults])"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Read `path` and resolve every device defined in it.
pub fn load(path: impl AsRef<Path>) -> Result<Vec<DeviceConfig>, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
    parse(&text)
}

/// Resolve every device from the text of a `devices.toml`. Pure: the same input
/// always yields the same output and nothing is read from the environment.
pub fn parse(text: &str) -> Result<Vec<DeviceConfig>, ConfigError> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Toml(e.to_string()))?;

    let mut seen = HashSet::new();
    let mut resolved = Vec::with_capacity(raw.devices.len());
    for device in raw.devices {
        if !seen.insert(device.id.clone()) {
            return Err(ConfigError::DuplicateId(device.id));
        }
        resolved.push(resolve(&raw.defaults, device)?);
    }
    Ok(resolved)
}

/// Fold the defaults into one raw device, failing if a required field is unset.
fn resolve(defaults: &Defaults, device: RawDevice) -> Result<DeviceConfig, ConfigError> {
    let id = device.id;

    let port = require(&id, "port", device.port.or(defaults.port))?;
    let username = require(
        &id,
        "username",
        device.username.or_else(|| defaults.username.clone()),
    )?;
    let password = require(
        &id,
        "password",
        device.password.or_else(|| defaults.password.clone()),
    )?;
    let connect_secs = require(
        &id,
        "connect_secs",
        device.connect_secs.or(defaults.connect_secs),
    )?;
    let command_secs = require(
        &id,
        "command_secs",
        device.command_secs.or(defaults.command_secs),
    )?;

    // `eager` and `sis_keepalive_secs` are optional everywhere: a device that sets
    // neither behaves exactly as before (lazy connect, no keep-warm loop).
    let eager = device.eager.or(defaults.eager).unwrap_or(false);
    let sis_keepalive_secs = device
        .sis_keepalive_secs
        .or(defaults.sis_keepalive_secs)
        .unwrap_or(DEFAULT_SIS_KEEPALIVE_SECS);
    let sis_keepalive = (sis_keepalive_secs > 0).then(|| Duration::from_secs(sis_keepalive_secs));

    Ok(DeviceConfig {
        host: device.host,
        port,
        username,
        password,
        connect_timeout: Duration::from_secs(connect_secs),
        command_timeout: Duration::from_secs(command_secs),
        eager,
        sis_keepalive,
        id,
    })
}

/// Return the value or a [`ConfigError::MissingField`] naming the device.
fn require<T>(device: &str, field: &'static str, value: Option<T>) -> Result<T, ConfigError> {
    value.ok_or_else(|| ConfigError::MissingField {
        device: device.to_string(),
        field,
    })
}

// ---- raw deserialization mirror of the file ------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    defaults: Defaults,
    #[serde(default, rename = "device")]
    devices: Vec<RawDevice>,
}

/// Every field is optional: a default only applies where a device omits it.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Defaults {
    port: Option<u16>,
    username: Option<String>,
    password: Option<String>,
    connect_secs: Option<u64>,
    command_secs: Option<u64>,
    eager: Option<bool>,
    sis_keepalive_secs: Option<u64>,
}

/// A device as written: `id` and `host` are required, the rest may inherit.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDevice {
    id: String,
    host: String,
    port: Option<u16>,
    username: Option<String>,
    password: Option<String>,
    connect_secs: Option<u64>,
    command_secs: Option<u64>,
    eager: Option<bool>,
    sis_keepalive_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_EXAMPLE: &str = r#"
[defaults]
port = 22023
connect_secs = 5
command_secs = 3

[[device]]
id = "atrium-101"
host = "10.0.0.7"
username = "admin"
password = "extron"

[[device]]
id = "annex-far"
host = "10.9.40.12"
username = "admin"
password = "extron"
connect_secs = 20
command_secs = 10
"#;

    fn get<'a>(devices: &'a [DeviceConfig], id: &str) -> &'a DeviceConfig {
        devices.iter().find(|d| d.id == id).expect("device present")
    }

    #[test]
    fn resolves_example_with_inheritance_and_overrides() {
        let devices = parse(USER_EXAMPLE).unwrap();
        assert_eq!(devices.len(), 2);

        // Order is preserved from the file.
        assert_eq!(devices[0].id, "atrium-101");
        assert_eq!(devices[1].id, "annex-far");

        // Nearby device inherits every default.
        let atrium = get(&devices, "atrium-101");
        assert_eq!(atrium.host, "10.0.0.7");
        assert_eq!(atrium.port, 22023);
        assert_eq!(atrium.connect_timeout, Duration::from_secs(5));
        assert_eq!(atrium.command_timeout, Duration::from_secs(3));

        // Far device overrides only the timeouts, still inherits the port.
        let annex = get(&devices, "annex-far");
        assert_eq!(annex.port, 22023);
        assert_eq!(annex.connect_timeout, Duration::from_secs(20));
        assert_eq!(annex.command_timeout, Duration::from_secs(10));
    }

    #[test]
    fn device_overrides_default_port() {
        let text = r#"
[defaults]
port = 22023
connect_secs = 5
command_secs = 3

[[device]]
id = "odd-port"
host = "10.0.0.9"
username = "admin"
password = "extron"
port = 22
"#;
        assert_eq!(parse(text).unwrap()[0].port, 22);
    }

    #[test]
    fn credentials_may_come_from_defaults() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
command_secs = 3

[[device]]
id = "bare"
host = "10.0.0.5"
"#;
        let bare = &parse(text).unwrap()[0];
        assert_eq!(bare.username, "admin");
        assert_eq!(bare.password, "extron");
    }

    #[test]
    fn missing_resolvable_field_names_device_and_field() {
        // No password anywhere.
        let text = r#"
[defaults]
port = 22023
username = "admin"
connect_secs = 5
command_secs = 3

[[device]]
id = "no-pass"
host = "10.0.0.5"
"#;
        assert_eq!(
            parse(text).unwrap_err(),
            ConfigError::MissingField {
                device: "no-pass".into(),
                field: "password",
            }
        );
    }

    #[test]
    fn missing_required_host_is_a_toml_error() {
        let text = r#"
[[device]]
id = "no-host"
"#;
        assert!(matches!(parse(text).unwrap_err(), ConfigError::Toml(_)));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
command_secs = 3

[[device]]
id = "dup"
host = "10.0.0.1"

[[device]]
id = "dup"
host = "10.0.0.2"
"#;
        assert_eq!(
            parse(text).unwrap_err(),
            ConfigError::DuplicateId("dup".into())
        );
    }

    #[test]
    fn unknown_field_is_rejected() {
        let text = r#"
[[device]]
id = "typo"
host = "10.0.0.1"
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
command_secs = 3
hostname = "oops"
"#;
        assert!(matches!(parse(text).unwrap_err(), ConfigError::Toml(_)));
    }

    #[test]
    fn empty_config_yields_no_devices() {
        assert_eq!(parse("").unwrap(), Vec::new());
    }

    #[test]
    fn no_defaults_table_is_fine_when_devices_are_complete() {
        let text = r#"
[[device]]
id = "self-contained"
host = "10.0.0.1"
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
command_secs = 3
"#;
        assert_eq!(parse(text).unwrap().len(), 1);
    }

    #[test]
    fn eager_defaults_off_with_a_standard_sis_keepalive_interval() {
        // A device that mentions neither field is unchanged: lazy, and its
        // (irrelevant-while-lazy) SIS keepalive falls back to the built-in default.
        let atrium = &parse(USER_EXAMPLE).unwrap()[0];
        assert!(!atrium.eager);
        assert_eq!(atrium.sis_keepalive, Some(Duration::from_secs(120)));
    }

    #[test]
    fn eager_and_sis_keepalive_inherit_from_defaults() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
command_secs = 3
eager = true
sis_keepalive_secs = 90

[[device]]
id = "warm"
host = "10.0.0.5"
"#;
        let warm = &parse(text).unwrap()[0];
        assert!(warm.eager);
        assert_eq!(warm.sis_keepalive, Some(Duration::from_secs(90)));
    }

    #[test]
    fn sis_keepalive_secs_zero_disables_the_sis_keepalive() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
command_secs = 3
eager = true
sis_keepalive_secs = 0

[[device]]
id = "warm-once"
host = "10.0.0.5"
"#;
        let device = &parse(text).unwrap()[0];
        assert!(device.eager);
        assert_eq!(device.sis_keepalive, None);
    }

    #[test]
    fn a_device_overrides_eager_and_sis_keepalive_from_defaults() {
        let text = r#"
[defaults]
port = 22023
username = "admin"
password = "extron"
connect_secs = 5
command_secs = 3
eager = true
sis_keepalive_secs = 120

[[device]]
id = "lazy-one"
host = "10.0.0.5"
eager = false

[[device]]
id = "slow-poll"
host = "10.0.0.6"
sis_keepalive_secs = 30
"#;
        let devices = parse(text).unwrap();
        let lazy = get(&devices, "lazy-one");
        assert!(!lazy.eager);
        let slow = get(&devices, "slow-poll");
        assert!(slow.eager);
        assert_eq!(slow.sis_keepalive, Some(Duration::from_secs(30)));
    }

    #[test]
    fn load_reads_from_disk() {
        let path =
            std::env::temp_dir().join(format!("sismatic-devices-{}.toml", std::process::id()));
        std::fs::write(&path, USER_EXAMPLE).unwrap();
        let devices = load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(devices.len(), 2);
    }
}
