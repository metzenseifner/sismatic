//! Loading and resolving device definitions from a `devices.toml`.
//!
//! The file has an optional `[defaults]` table and a list of `[[device]]`
//! tables. Each device inherits every default it does not set itself, so a
//! nearby device can be a few lines while a far one overrides only the timeouts
//! it needs:
//!
//! ```toml
//! [defaults]
//! port = 22023       # optional, defaults to 22023
//! connect_secs = 5   # optional, defaults to 5
//! command_secs = 3   # optional, defaults to 3
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
//! Resolution is format-agnostic: [`resolve_config`] turns an already-parsed
//! [`RawConfig`] into fully-resolved [`DeviceConfig`]s and is the only step this
//! crate guarantees in every build. Turning file *text* into a `RawConfig` is
//! delegated to a serde deserializer chosen by the caller; enabling the `toml`,
//! `json`, or `yaml` feature adds a ready-made loader (`from_toml_str` and
//! friends, plus an extension-dispatching `load`). `id` and `host` are the only
//! fields a device must state itself, and `username`/`password` must be resolvable
//! from the device or the defaults; `port`, `connect_secs`, and `command_secs` fall
//! back to built-in defaults (22023, 5, 3) when set in neither place.

use std::collections::HashSet;
use std::fmt;
#[cfg(any(feature = "toml", feature = "yaml", feature = "json"))]
use std::path::Path;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

/// The SIS keepalive interval applied when `eager` is on but `sis_keepalive_secs` is
/// left unset. Comfortably under the SMP's default 5-minute idle disconnect, with
/// room for one failed round-trip to self-heal before the window closes.
const DEFAULT_SIS_KEEPALIVE_SECS: u64 = 120;

/// The SMP's SIS-over-SSH port, used when neither the device nor `[defaults]` names one.
const DEFAULT_PORT: u16 = 22023;

/// Connect timeout applied when neither the device nor `[defaults]` names one.
const DEFAULT_CONNECT_SECS: u64 = 5;

/// Per-command timeout applied when neither the device nor `[defaults]` names one.
const DEFAULT_COMMAND_SECS: u64 = 3;

/// A device credential held as a [`SecretString`]: redacted in `Debug` output and
/// zeroized on drop, so a password can't leak into logs or linger in memory.
///
/// Wrapping the secret in a newtype (rather than storing a bare `SecretString`)
/// lets [`DeviceConfig`] keep its derived `PartialEq`/`Eq`. `secrecy` deliberately
/// withholds equality from `SecretString` to discourage non-constant-time secret
/// comparisons; we opt back in here for the one place that needs it — asserting on
/// resolved configs in tests. `#[serde(transparent)]` forwards deserialization
/// straight to the inner string, so `password = "..."` parses unchanged and no bare
/// `String` copy of the secret is ever materialized.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Password(SecretString);

impl Password {
    /// Borrow the plaintext for the one legitimate use: handing it to SSH auth.
    /// This is the single audit point — grep `expose_secret` to find every read.
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password([REDACTED])")
    }
}

/// Compares the exposed secrets with a plain (non-constant-time) `==` — the very
/// comparison `secrecy` avoids by withholding `PartialEq`. Acceptable here because
/// configs are only compared in tests, in memory, between our own values: there is
/// no attacker-controlled input and no observable timing boundary. Reach for
/// `subtle::ConstantTimeEq` if a secret ever needs comparing on a live path.
impl PartialEq for Password {
    fn eq(&self, other: &Self) -> bool {
        self.expose_secret() == other.expose_secret()
    }
}

impl Eq for Password {}

impl From<String> for Password {
    fn from(s: String) -> Self {
        Password(SecretString::from(s))
    }
}

impl From<&str> for Password {
    fn from(s: &str) -> Self {
        Password(SecretString::from(s))
    }
}

/// A fully-resolved device: every field has a concrete value, with defaults
/// already folded in. This is what the registry consumes to open a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceConfig {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Password,
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
    /// The file could not be read from disk (produced by the file loader).
    Io(String),
    /// The text did not deserialize into a [`RawConfig`], or a required
    /// `id`/`host` was absent.
    Parse(String),
    /// The file extension has no compiled-in deserializer.
    UnsupportedFormat(String),
    /// Two devices share the same `id`, so one would shadow the other.
    DuplicateId(String),
    /// A field was set neither on the device nor in `[defaults]`.
    MissingField { device: String, field: &'static str },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "reading devices file: {e}"),
            ConfigError::Parse(e) => write!(f, "parsing devices file: {e}"),
            ConfigError::UnsupportedFormat(ext) => {
                write!(f, "unsupported config file extension `{ext}`")
            }
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

/// Read `path`, pick a deserializer from its extension, and resolve every device
/// defined in it. Only the file read is impure; everything after is a pure
/// function of the bytes. Available whenever at least one format feature
/// (`toml`, `json`, `yaml`) is enabled.
#[cfg(any(feature = "toml", feature = "yaml", feature = "json"))]
pub fn load(path: impl AsRef<Path>) -> Result<Vec<DeviceConfig>, ConfigError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
    match path.extension().and_then(|e| e.to_str()) {
        #[cfg(feature = "toml")]
        Some("toml") => from_toml_str(&text),
        #[cfg(feature = "yaml")]
        Some("yaml") | Some("yml") => from_yaml_str(&text),
        #[cfg(feature = "json")]
        Some("json") => from_json_str(&text),
        other => Err(ConfigError::UnsupportedFormat(
            other.unwrap_or("").to_string(),
        )),
    }
}

/// Deserialize TOML text into a [`RawConfig`], then [`resolve_config`].
#[cfg(feature = "toml")]
pub fn from_toml_str(text: &str) -> Result<Vec<DeviceConfig>, ConfigError> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    resolve_config(raw)
}

/// Deserialize JSON text into a [`RawConfig`], then [`resolve_config`].
#[cfg(feature = "json")]
pub fn from_json_str(text: &str) -> Result<Vec<DeviceConfig>, ConfigError> {
    let raw: RawConfig =
        serde_json::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    resolve_config(raw)
}

/// Deserialize YAML text into a [`RawConfig`], then [`resolve_config`].
#[cfg(feature = "yaml")]
pub fn from_yaml_str(text: &str) -> Result<Vec<DeviceConfig>, ConfigError> {
    let raw: RawConfig =
        serde_saphyr::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    resolve_config(raw)
}

/// Format-agnostic entry point: consume an already-parsed [`RawConfig`] and
/// resolve it into validated [`DeviceConfig`]s. Pure and total — the same input
/// always yields the same output, it never panics, and it reads nothing from the
/// environment. Callers who bring their own deserializer target this directly.
pub fn resolve_config(raw: RawConfig) -> Result<Vec<DeviceConfig>, ConfigError> {
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

    let port = device.port.or(defaults.port).unwrap_or(DEFAULT_PORT);
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
    let connect_secs = device
        .connect_secs
        .or(defaults.connect_secs)
        .unwrap_or(DEFAULT_CONNECT_SECS);
    let command_secs = device
        .command_secs
        .or(defaults.command_secs)
        .unwrap_or(DEFAULT_COMMAND_SECS);

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
pub struct RawConfig {
    #[serde(default)]
    defaults: Defaults,
    #[serde(default, alias = "device", alias = "devices")]
    devices: Vec<RawDevice>,
}

/// Every field is optional: a default only applies where a device omits it.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Defaults {
    port: Option<u16>,
    username: Option<String>,
    password: Option<Password>,
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
    password: Option<Password>,
    connect_secs: Option<u64>,
    command_secs: Option<u64>,
    eager: Option<bool>,
    sis_keepalive_secs: Option<u64>,
}

#[cfg(all(test, feature = "toml"))]
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
        let devices = from_toml_str(USER_EXAMPLE).unwrap();
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
        assert_eq!(from_toml_str(text).unwrap()[0].port, 22);
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
        let bare = &from_toml_str(text).unwrap()[0];
        assert_eq!(bare.username, "admin");
        assert_eq!(bare.password.expose_secret(), "extron");
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
            from_toml_str(text).unwrap_err(),
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
        assert!(matches!(
            from_toml_str(text).unwrap_err(),
            ConfigError::Parse(_)
        ));
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
            from_toml_str(text).unwrap_err(),
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
        assert!(matches!(
            from_toml_str(text).unwrap_err(),
            ConfigError::Parse(_)
        ));
    }

    #[test]
    fn empty_config_yields_no_devices() {
        assert_eq!(from_toml_str("").unwrap(), Vec::new());
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
        assert_eq!(from_toml_str(text).unwrap().len(), 1);
    }

    #[test]
    fn port_and_timeouts_fall_back_to_built_in_defaults() {
        // Neither the device nor a `[defaults]` table names port/connect_secs/command_secs.
        let text = r#"
[[device]]
id = "sparse"
host = "10.0.0.5"
username = "admin"
password = "extron"
"#;
        let device = &from_toml_str(text).unwrap()[0];
        assert_eq!(device.port, 22023);
        assert_eq!(device.connect_timeout, Duration::from_secs(5));
        assert_eq!(device.command_timeout, Duration::from_secs(3));
    }

    #[test]
    fn eager_defaults_off_with_a_standard_sis_keepalive_interval() {
        // A device that mentions neither field is unchanged: lazy, and its
        // (irrelevant-while-lazy) SIS keepalive falls back to the built-in default.
        let atrium = &from_toml_str(USER_EXAMPLE).unwrap()[0];
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
        let warm = &from_toml_str(text).unwrap()[0];
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
        let device = &from_toml_str(text).unwrap()[0];
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
        let devices = from_toml_str(text).unwrap();
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

    #[test]
    fn load_rejects_an_extension_without_a_deserializer() {
        let path = std::env::temp_dir().join(format!("sismatic-x-{}.ini", std::process::id()));
        std::fs::write(&path, "irrelevant").unwrap();
        let err = load(&path).unwrap_err();
        std::fs::remove_file(&path).ok();
        assert_eq!(err, ConfigError::UnsupportedFormat("ini".into()));
    }
}

/// The resolution core carries no format feature, so it is tested by building a
/// [`RawConfig`] directly — no TOML/YAML/JSON, no filesystem.
#[cfg(test)]
mod resolve_config_tests {
    use super::*;

    #[test]
    fn folds_defaults_and_resolves_directly() {
        let raw = RawConfig {
            defaults: Defaults {
                port: Some(22023),
                username: Some("admin".into()),
                password: Some("extron".into()),
                connect_secs: Some(5),
                command_secs: Some(3),
                eager: None,
                sis_keepalive_secs: None,
            },
            devices: vec![RawDevice {
                id: "bare".into(),
                host: "10.0.0.5".into(),
                port: None,
                username: None,
                password: None,
                connect_secs: None,
                command_secs: None,
                eager: None,
                sis_keepalive_secs: None,
            }],
        };
        let resolved = resolve_config(raw).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].port, 22023);
        assert_eq!(resolved[0].username, "admin");
        assert_eq!(resolved[0].connect_timeout, Duration::from_secs(5));
    }

    #[test]
    fn duplicate_ids_are_rejected_without_a_format() {
        let raw = RawConfig {
            defaults: Defaults::default(),
            devices: vec![
                RawDevice {
                    id: "dup".into(),
                    host: "10.0.0.1".into(),
                    port: Some(22023),
                    username: Some("admin".into()),
                    password: Some("extron".into()),
                    connect_secs: Some(5),
                    command_secs: Some(3),
                    eager: None,
                    sis_keepalive_secs: None,
                },
                RawDevice {
                    id: "dup".into(),
                    host: "10.0.0.2".into(),
                    port: Some(22023),
                    username: Some("admin".into()),
                    password: Some("extron".into()),
                    connect_secs: Some(5),
                    command_secs: Some(3),
                    eager: None,
                    sis_keepalive_secs: None,
                },
            ],
        };
        assert_eq!(
            resolve_config(raw).unwrap_err(),
            ConfigError::DuplicateId("dup".into())
        );
    }
}

/// The same logical document in JSON resolves to the same devices as its TOML
/// twin, because both feed one `serde::Deserialize` — see the design note
/// `docs/format-agnostic-config-opt-in-features.md`, Deep dive A.
#[cfg(all(test, feature = "json"))]
mod json_tests {
    use super::*;

    #[test]
    fn json_document_resolves() {
        let text = r#"
        {
          "defaults": { "port": 22023, "username": "admin", "password": "extron",
                        "connect_secs": 5, "command_secs": 3 },
          "device": [ { "id": "a", "host": "10.0.0.1" } ]
        }"#;
        let devices = from_json_str(text).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "a");
        assert_eq!(devices[0].port, 22023);
        assert_eq!(devices[0].username, "admin");
    }
}

/// The YAML twin resolves identically, exercising the `serde-saphyr` deserializer.
#[cfg(all(test, feature = "yaml"))]
mod yaml_tests {
    use super::*;

    #[test]
    fn yaml_document_resolves() {
        let text = "
defaults:
  port: 22023
  username: admin
  password: extron
  connect_secs: 5
  command_secs: 3
device:
  - id: a
    host: \"10.0.0.1\"
";
        let devices = from_yaml_str(text).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "a");
        assert_eq!(devices[0].port, 22023);
        assert_eq!(devices[0].username, "admin");
    }
}
