//! Server configuration: the process-level knobs (where the devices file lives,
//! how often to poll, where to listen), kept distinct from the *device* config
//! that `sismatic-core` owns.
//!
//! The split mirrors core's own discipline (see [`sismatic_core::devices::config`]):
//! [`get_configuration`] is the only impure step — it reads one file — and
//! [`resolve_config`] is a pure, total function from an already-parsed
//! [`RawServerConfig`] to a [`ServerConfig`] with every default folded in. That
//! is what makes this layer testable without a filesystem: the precedence rules
//! below are plain unit tests over values, and `devices_config_path` is *only* a
//! path here. Turning that path into devices is core's `config::load`, which
//! core already tests; the server calls it once, in the composition root, and
//! hands the result to [`run`](crate::run).
//!
//! Precedence for every field is the same three-layer fallback, most specific
//! first:
//!
//! 1. the field's own section (`devices_config_path`, `[sync]`, `[http]`),
//! 2. the `[defaults]` table,
//! 3. the built-in constant.
//!
//! Relative paths resolve against the *config file's* directory rather than the
//! process's working directory, so a config and the devices file it names travel
//! together and no test (or systemd unit) has to care where it was launched from.

use std::path::{Path, PathBuf};

use config::ConfigError;
use serde::Deserialize;

/// Devices file used when neither the config nor `[defaults]` names one; matches
/// the CLI's `--config` default so both front-ends agree on the convention.
const DEFAULT_DEVICES_CONFIG_PATH: &str = "devices.toml";
const DEFAULT_INTERVAL_SECS: u64 = 30;
const DEFAULT_FIELDS: &[&str] = &["RUNNING_STATE"];
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;

/// Read a server config file and resolve it. The only step that touches the
/// filesystem; everything after the read is a pure function of the bytes.
///
/// The format is inferred from the extension by the `config` crate, and
/// relative paths inside the file are anchored to the file's own directory.
pub fn get_configuration(path: impl AsRef<Path>) -> Result<ServerConfig, ConfigError> {
    let path = path.as_ref();
    let raw: RawServerConfig = config::Config::builder()
        .add_source(config::File::from(path))
        .build()?
        .try_deserialize()?;
    Ok(resolve_config(base_dir(path), raw))
}

/// The directory a config file's relative paths resolve against. A bare
/// `configuration.yaml` has no parent, which is exactly the working directory —
/// i.e. the empty base, so `join` leaves the path as written.
fn base_dir(config_path: &Path) -> &Path {
    config_path.parent().unwrap_or_else(|| Path::new(""))
}

/// Fold `[defaults]` and the built-in defaults into a fully-resolved config.
///
/// Pure and total: same input, same output, no I/O, no panics, nothing read from
/// the environment. `base` is the directory relative paths are anchored to,
/// passed in rather than discovered so this stays a function of its arguments.
pub fn resolve_config(base: &Path, raw: RawServerConfig) -> ServerConfig {
    let defaults = raw.defaults;
    let sync = raw.sync.unwrap_or_default();
    let http = raw.http.unwrap_or_default();

    let devices_config_path = base.join(
        raw.devices_config_path
            .or(defaults.devices_config_path)
            .unwrap_or_else(|| DEFAULT_DEVICES_CONFIG_PATH.to_owned()),
    );

    let interval_secs = sync
        .interval_secs
        .or(defaults.interval_secs)
        .unwrap_or(DEFAULT_INTERVAL_SECS);

    let fields = sync
        .fields
        .or(defaults.fields)
        .unwrap_or_else(|| DEFAULT_FIELDS.iter().map(|f| (*f).to_owned()).collect());

    let host = http
        .host
        .or(defaults.host)
        .unwrap_or_else(|| DEFAULT_HOST.to_owned());

    let port = http.port.or(defaults.port).unwrap_or(DEFAULT_PORT);

    ServerConfig {
        devices_config_path,
        sync: SyncConfig {
            interval_secs,
            fields,
        },
        http: HttpConfig { host, port },
    }
}

/// The config file exactly as written: every field optional, so a config may
/// state only what it overrides. `deny_unknown_fields` turns a typo into an
/// error at startup instead of a silently ignored setting.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawServerConfig {
    #[serde(default)]
    pub defaults: Defaults,
    pub devices_config_path: Option<String>,
    pub sync: Option<RawSync>,
    pub http: Option<RawHttp>,
}

/// Fallbacks for anything the sections above leave unset. One flat table so a
/// deployment can pin, say, a devices path and a port without restating the
/// sections that own them.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub devices_config_path: Option<String>,
    pub interval_secs: Option<u64>,
    pub fields: Option<Vec<String>>,
    pub host: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSync {
    pub interval_secs: Option<u64>,
    pub fields: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHttp {
    pub host: Option<String>,
    pub port: Option<u16>,
}

/// A fully-resolved server config: every field concrete, every path anchored.
/// Deliberately *not* `Deserialize` — the only way to obtain one is
/// [`resolve_config`], so no code path can skip the defaulting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Where the devices file lives, anchored to the server config's directory.
    /// Consumed by the composition root, which loads it via core; the server
    /// runtime itself never sees a path.
    pub devices_config_path: PathBuf,
    pub sync: SyncConfig,
    pub http: HttpConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConfig {
    pub interval_secs: u64,
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse config *text* rather than a file: these tests exercise the same
    /// serde surface the real loader uses, with no filesystem involved.
    fn raw(text: &str) -> RawServerConfig {
        config::Config::builder()
            .add_source(config::File::from_str(text, config::FileFormat::Yaml))
            .build()
            .expect("building config")
            .try_deserialize()
            .expect("deserializing config")
    }

    fn resolve(base: &str, text: &str) -> ServerConfig {
        resolve_config(Path::new(base), raw(text))
    }

    #[test]
    fn devices_path_falls_back_to_the_built_in_default() {
        let cfg = resolve("", "{}");
        assert_eq!(cfg.devices_config_path, PathBuf::from("devices.toml"));
    }

    #[test]
    fn defaults_table_supplies_the_devices_path() {
        let cfg = resolve("", "defaults:\n  devices_config_path: pool.toml\n");
        assert_eq!(cfg.devices_config_path, PathBuf::from("pool.toml"));
    }

    #[test]
    fn explicit_devices_path_beats_the_defaults_table() {
        let cfg = resolve(
            "",
            "devices_config_path: explicit.toml\ndefaults:\n  devices_config_path: fallback.toml\n",
        );
        assert_eq!(cfg.devices_config_path, PathBuf::from("explicit.toml"));
    }

    #[test]
    fn relative_devices_path_is_anchored_to_the_config_file_directory() {
        let cfg = resolve("/etc/sismatic", "devices_config_path: devices.toml\n");
        assert_eq!(
            cfg.devices_config_path,
            PathBuf::from("/etc/sismatic/devices.toml")
        );
    }

    #[test]
    fn absolute_devices_path_ignores_the_config_file_directory() {
        let cfg = resolve("/etc/sismatic", "devices_config_path: /srv/devices.toml\n");
        assert_eq!(cfg.devices_config_path, PathBuf::from("/srv/devices.toml"));
    }

    #[test]
    fn base_dir_of_a_bare_filename_is_the_working_directory() {
        assert_eq!(base_dir(Path::new("configuration.yaml")), Path::new(""));
        assert_eq!(
            base_dir(Path::new("/etc/sismatic/configuration.yaml")),
            Path::new("/etc/sismatic")
        );
    }

    #[test]
    fn sync_and_http_fall_back_layer_by_layer() {
        let cfg = resolve(
            "",
            "defaults:\n  interval_secs: 7\n  port: 9999\nsync:\n  fields: [VIDEO_MUTE]\nhttp:\n  host: 0.0.0.0\n",
        );
        // section unset -> defaults table
        assert_eq!(cfg.sync.interval_secs, 7);
        assert_eq!(cfg.http.port, 9999);
        // section set -> section wins over the built-ins
        assert_eq!(cfg.sync.fields, vec!["VIDEO_MUTE".to_owned()]);
        assert_eq!(cfg.http.host, "0.0.0.0");
    }

    #[test]
    fn an_empty_config_resolves_entirely_to_built_in_defaults() {
        let cfg = resolve("", "{}");
        assert_eq!(
            cfg,
            ServerConfig {
                devices_config_path: PathBuf::from(DEFAULT_DEVICES_CONFIG_PATH),
                sync: SyncConfig {
                    interval_secs: DEFAULT_INTERVAL_SECS,
                    fields: vec!["RUNNING_STATE".to_owned()],
                },
                http: HttpConfig {
                    host: DEFAULT_HOST.to_owned(),
                    port: DEFAULT_PORT,
                },
            }
        );
    }

    #[test]
    fn a_partial_sync_section_is_accepted() {
        // The section's own fields are independently optional, so naming only
        // `interval_secs` does not force the caller to restate `fields`.
        let cfg = resolve("", "sync:\n  interval_secs: 1\n");
        assert_eq!(cfg.sync.interval_secs, 1);
        assert_eq!(cfg.sync.fields, vec!["RUNNING_STATE".to_owned()]);
    }

    #[test]
    fn a_misspelled_field_is_rejected_rather_than_ignored() {
        let err = config::Config::builder()
            .add_source(config::File::from_str(
                "devices_config_pth: devices.toml\n",
                config::FileFormat::Yaml,
            ))
            .build()
            .expect("building config")
            .try_deserialize::<RawServerConfig>()
            .unwrap_err();
        assert!(
            err.to_string().contains("devices_config_pth"),
            "expected the unknown key in the error, got: {err}"
        );
    }
}
