//! Server configuration: the process-level knobs (where the devices file lives,
//! how often to poll, where to listen), kept distinct from the *device* config
//! that `sismatic-core` owns.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use config::ConfigError;
use serde::Deserialize;
use serde::de::{self, MapAccess, value::MapAccessDeserializer};
use sismatic_core::protocol::instructions::query::Query;

/// Default devices config path relative to the configuration file.
const DEFAULT_DEVICES_CONFIG_PATH: &str = "devices.toml";
const DEFAULT_INTERVAL_SECS: u64 = 30;
const DEFAULT_FIELDS: &[&str] = &["RUNNING_STATE"];
/// The `sync.fields` entry standing for every field core can query. Canonical
/// query names are `UPPER_SNAKE`, so this can never collide with one.
const ALL_FIELDS: &str = "*";
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;

const DEFAULT_INTENT_RELAY_POLL_MS: u64 = 250;
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

const ENV_PREFIX: &str = "SISMATIC_SERVER";
const ENV_SEPARATOR: &str = "__";
const ENV_LIST_SEPARATOR: &str = ",";
/// The key [`CONFIG_PATH_ENV`] collapses to once the prefix is stripped, and so
/// the one [`env_source`] must drop before the document is deserialized.
const ENV_CONFIG_PATH_KEY: &str = "config";

/// The variable naming which config file to read.
///
/// Public because the composition root has to answer it *before* there is a
/// config to resolve, and it must spell the name identically to the source
/// that excludes it. `env_var_names_agree` below pins the two together.
pub const CONFIG_PATH_ENV: &str = "SISMATIC_SERVER__CONFIG";

/// Read a server config file, merge the environment over it, and resolve the
/// result.
///
/// The format is inferred from the extension by the `config` crate, and
/// relative paths — whichever layer wrote them — are anchored to the file's own
/// directory.
pub fn get_configuration(path: impl AsRef<Path>) -> Result<ServerConfig, ConfigError> {
    get_configuration_with_env(path, env_source())
}

/// [`get_configuration`] using an explicit process environment source.
///
/// The abstraction seam exists to enable testing of the process environment input:
/// `std::env::set_var` is `unsafe` in edition 2024 precisely because cargo runs tests on threads
/// that share one environment. Passing `env_source().source(Some(vars))` resolves against a literal
/// map instead, so a test states the variables it means and no other test can see them.
pub fn get_configuration_with_env(
    path: impl AsRef<Path>,
    env: EnvSource,
) -> Result<ServerConfig, ConfigError> {
    let path = path.as_ref();
    Ok(resolve_config(
        base_dir(path),
        raw_config(config::File::from(path), env)?,
    ))
}

/// How the intent relay drains the write outbox.
///
/// `poll_ms` is a floor on how long an accepted command waits before a device
/// hears about it, so it trades idle wake-ups against apparent latency. 250 ms
/// is below the threshold at which an operator pressing "start" perceives a
/// delay, and costs four wake-ups per device per second.
///
/// `max_attempts` counts total tries, not retries: `1` means a command that
/// fails once is failed for good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentRelayConfig {
    pub poll: Duration,
    pub max_attempts: u32,
}

/// The environment as a config source: every key of [`RawServerConfig`],
/// addressable as `SISMATIC_SERVER__` + its path.
pub fn env_source() -> EnvSource {
    EnvSource(
        config::Environment::with_prefix(ENV_PREFIX)
            // separator of namespace prefix from the variable name
            .separator(ENV_SEPARATOR)
            // An unset shell variable expands to the empty string, and a systemd
            // unit writes one for `Environment=SISMATIC_SERVER__HTTP__HOST=`.
            // Neither is a request for an empty host, so both read as "not set".
            .ignore_empty(true)
            // Environment values are all strings. `try_parsing` applies types to the ones that are
            // obviously numbers, and — the reason it is not optional here — it is what gates
            // `list_separator` at all.
            .try_parsing(true)
            .list_separator(ENV_LIST_SEPARATOR)
            .with_list_parse_key("sync.fields")
            .with_list_parse_key("defaults.fields"),
    )
}

/// The `SISMATIC_SERVER__` environment, restricted to the keys that are part of
/// the config document. Enables us to simulate the process environment in tests.
#[derive(Debug, Clone)]
pub struct EnvSource(config::Environment);

impl EnvSource {
    /// Read `source` instead of the process's environment, prefix filtering and
    /// all. The seam tests use — see [`get_configuration_with_env`].
    #[must_use]
    pub fn source(self, source: Option<config::Map<String, String>>) -> Self {
        Self(self.0.source(source))
    }
}

impl config::Source for EnvSource {
    fn clone_into_box(&self) -> Box<dyn config::Source + Send + Sync> {
        Box::new(self.clone())
    }

    fn collect(&self) -> Result<config::Map<String, config::Value>, ConfigError> {
        let mut keys = self.0.collect()?;
        keys.remove(ENV_CONFIG_PATH_KEY);
        Ok(keys)
    }
}

/// Merge the two writers of the in-memory config document into one parsed
/// [`RawServerConfig`]. Order is the precedence: the environment is added last,
/// so it wins at any key it names and is silent at every key it does not.
///
/// Generic over the file source so the unit tests below can feed config *text*
/// through exactly this path.
fn raw_config(
    file: impl config::Source + Send + Sync + 'static,
    env: EnvSource,
) -> Result<RawServerConfig, ConfigError> {
    config::Config::builder()
        // Abstracts support for multiple server config file formats
        .add_source(file)
        .add_source(env)
        .build()?
        .try_deserialize()
}

/// Resolve a given path's parent path segment if given in path literal.
fn base_dir(config_path: &Path) -> &Path {
    config_path.parent().unwrap_or_else(|| Path::new(""))
}

/// Fold `[defaults]` and the built-in defaults into a fully-resolved config.
pub fn resolve_config(base: &Path, raw: RawServerConfig) -> ServerConfig {
    let defaults = raw.defaults;
    let intent_relay = raw.intent_relay.unwrap_or_default();
    let sync = raw.sync.unwrap_or_default();
    let http = raw.http.unwrap_or_default();

    let devices_config_path = base.join(
        raw.devices_config_path
            .or(defaults.devices_config_path)
            .unwrap_or_else(|| DEFAULT_DEVICES_CONFIG_PATH.to_owned()),
    );

    let default_interval = handle_interval(
        sync.interval_secs
            .or(defaults.interval_secs)
            .unwrap_or(DEFAULT_INTERVAL_SECS),
    );

    let fields = resolve_fields(
        sync.fields
            .or(defaults.fields)
            .unwrap_or_else(|| DEFAULT_FIELDS.iter().map(|f| RawField::named(f)).collect()),
        default_interval,
    );

    let host = http
        .host
        .or(defaults.host)
        .unwrap_or_else(|| DEFAULT_HOST.to_owned());

    let port = http.port.or(defaults.port).unwrap_or(DEFAULT_PORT);

    ServerConfig {
        devices_config_path,
        intent_relay: IntentRelayConfig {
            poll: handle_poll(intent_relay.poll_ms.unwrap_or(DEFAULT_INTENT_RELAY_POLL_MS)),
            max_attempts: intent_relay.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS),
        },
        sync: SyncConfig {
            default_interval,
            fields,
        },
        http: HttpConfig { host, port },
    }
}

/// Decode `intent_relay.poll_ms`.
///
/// `0` is *not* the "never" sentinel it is under `sync` — a relay that never
/// looks at its queue accepts commands and performs none of them, which is a
/// deployment nobody means to write. It is read as "as fast as possible"
/// instead, which is one millisecond: `tokio::time::interval` panics on a zero
/// period, so the floor keeps a plausible-looking config from taking the
/// process down at startup.
fn handle_poll(ms: u64) -> Duration {
    Duration::from_millis(ms.max(1))
}

/// Decode the `interval_secs` sentinel: `0` is *never*, anything else is a
/// delay. The one place in the server that knows `0` is special — mirrors
/// core's `sis_keepalive_secs` / `eager_retry_secs`.
fn handle_interval(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Normalize the vector of FieldConfigs. Expand `"*"` and fold per-field overrides into one
/// ordered, duplicate-free schedule.
///
/// Two passes, because the wildcard has to honor an override that appears
/// *after* it: the first pass settles what
/// each explicitly-named field resolves to, and only then does the second pass
/// lay fields out in order, expanding the wildcard into whatever it did not
/// claim. Doing it in one pass would make the result depend on where in the list
/// the `"*"` happens to sit.
fn resolve_fields(raw: Vec<RawField>, default_interval: Option<Duration>) -> Vec<FieldConfig> {
    // Pass 1: every explicitly-named field, last mentioned wins.
    let mut explicit: Vec<(&str, Option<Duration>)> = Vec::new();
    // collect all explicitly-named fields
    for field in raw.iter().filter(|f| !f.is_wildcard()) {
        let resolved = field
            .interval_secs
            .map_or(default_interval, handle_interval);
        match explicit.iter_mut().find(|(name, _)| *name == field.name) {
            Some(entry) => entry.1 = resolved,
            None => explicit.push((&field.name, resolved)),
        }
    }
    let explicit_interval = |name: &str| {
        explicit
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, resolved)| *resolved)
    };

    // Pass 2: lay them out in the order the config first mentions them, the
    // wildcard mentioning the whole catalog at the position it appears in.
    let mut fields: Vec<FieldConfig> = Vec::new();
    let mut push = |name: &str, resolved| {
        if !fields.iter().any(|f: &FieldConfig| f.name == name) {
            fields.push(FieldConfig {
                name: name.to_owned(),
                interval: resolved,
            });
        }
    };
    for field in &raw {
        if field.is_wildcard() {
            // A wildcard may pin the interval it fills with; otherwise it fills
            // with whatever the layers below resolved to.
            let fill = field
                .interval_secs
                .map_or(default_interval, handle_interval);
            for query in Query::ALL {
                push(
                    query.name(),
                    explicit_interval(query.name()).unwrap_or(fill),
                );
            }
        } else {
            // Pass 1 recorded every non-wildcard entry, so this always hits;
            // falling back to the default keeps the function total regardless.
            push(
                &field.name,
                explicit_interval(&field.name).unwrap_or(default_interval),
            );
        }
    }
    fields
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
    pub intent_relay: Option<RawIntentRelay>,
    pub sync: Option<RawSync>,
    pub http: Option<RawHttp>,
}

/// Fallbacks for anything the sections above leave unset. Intentional flat structure usability
/// until complexity grows beyond some threshold.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub devices_config_path: Option<String>,
    pub interval_secs: Option<u64>,
    pub fields: Option<Vec<RawField>>,
    pub host: Option<String>,
    pub port: Option<u16>,
}

/// The `intent_relay` section as written.
///
/// A section of its own rather than keys folded into [`Defaults`]: the two
/// numbers only mean anything together with the relay, and nothing else in the
/// document would ever want to inherit them.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawIntentRelay {
    pub poll_ms: Option<u64>,
    pub max_attempts: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSync {
    /// The interval every entry of `fields` inherits unless it pins its own.
    pub interval_secs: Option<u64>,
    pub fields: Option<Vec<RawField>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHttp {
    pub host: Option<String>,
    pub port: Option<u16>,
}

/// One entry of `sync.fields` as written. The two spellings — a bare name, and
/// a table that also pins an interval — parse into this one shape, so the
/// resolver has a single case to handle and a config may mix them freely.
///
/// `interval_secs` stays `Option<u64>` at this layer precisely because we must distinguish
/// "unset" from "given"  until [`resolve_config`]
/// collapses them. Also makes makes an explicit `interval_secs: 0`: means *never* rather than reading as
/// absent and inheriting the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawField {
    pub name: String,
    pub interval_secs: Option<u64>,
}

impl RawField {
    /// A field that inherits whatever interval the layers below it resolve to.
    fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            interval_secs: None,
        }
    }

    /// Whether this entry is the `"*"` catch-all rather than one field's name.
    fn is_wildcard(&self) -> bool {
        self.name == ALL_FIELDS
    }
}

/// Provides parsing user feedback through `deny_unknown_fields`. Avoid accidental default value
/// from silently being used in due to a spelling mistake e.g. `intreval_secs`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFieldTable {
    name: String,
    interval_secs: Option<u64>,
}

/// Hand-written rather than `#[serde(untagged)]`: untagged reports every failure
/// as "data did not match any variant", which would throw away the precise
/// unknown-key error `RawFieldTable` produces. Dispatching on the input's own
/// shape keeps that error intact.
impl<'de> Deserialize<'de> for RawField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct FieldVisitor;

        impl<'de> de::Visitor<'de> for FieldVisitor {
            type Value = RawField;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a field name, or a table with `name` and an optional `interval_secs`")
            }

            fn visit_str<E: de::Error>(self, name: &str) -> Result<RawField, E> {
                Ok(RawField::named(name))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<RawField, A::Error> {
                let table = RawFieldTable::deserialize(MapAccessDeserializer::new(map))?;
                Ok(RawField {
                    name: table.name,
                    interval_secs: table.interval_secs,
                })
            }
        }

        deserializer.deserialize_any(FieldVisitor)
    }
}

/// A fully-resolved server config: every field concrete, every path anchored.
/// Deliberately *not* `Deserialize` — the only way to obtain one is
/// [`resolve_config`], so no code path can skip it (folds in the default values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Where the devices file lives, using the server config's directory as base.
    pub devices_config_path: PathBuf,
    pub intent_relay: IntentRelayConfig,
    pub sync: SyncConfig,
    pub http: HttpConfig,
}

/// Represents command line input
///
/// Deliberately free of any `clap` types, so the command-line parser stays in
/// the composition root: `main` maps its own `Args` into this.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Overrides {
    pub devices_config_path: Option<PathBuf>,
    pub host: Option<String>,
    pub port: Option<u16>,
}

impl ServerConfig {
    /// Fold the command line in above a config the file and the environment have
    /// already fully resolved — the whole of the top layer, in one place.
    ///
    /// `None` at a field means *the caller named nothing*, so the resolved value
    /// stands; [`Overrides::default`] is therefore the identity. That is what
    /// makes the composition root's flags `Option` with no clap-side defaults: a
    /// `default_value` would arrive here indistinguishable from a value the
    /// operator typed, and would silently outrank the config file every time.
    ///
    /// Note what does *not* happen to `devices_config_path` here. It is not
    /// anchored to the config file's directory the way [`resolve_config`] anchors
    /// a path the file's devices_config_path named, because a path typed at a shell prompt is
    /// relative to the process's working directory. Re-anchoring it would hand
    /// back the very file the flag set out to replace — see
    /// [the module docs](self#relative-paths).
    #[must_use]
    pub fn with_overrides(self, overrides: Overrides) -> Self {
        Self {
            devices_config_path: overrides
                .devices_config_path
                .unwrap_or(self.devices_config_path),
            // No command-line flag reaches the relay, so it passes through
            // untouched — the same as `sync`.
            intent_relay: self.intent_relay,
            sync: self.sync,
            http: HttpConfig {
                host: overrides.host.unwrap_or(self.http.host),
                port: overrides.port.unwrap_or(self.http.port),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConfig {
    /// The interval a field inherits when it sets none explicitly, `None` means
    /// that inherited value is *never*.
    pub default_interval: Option<Duration>,
    /// Every field the config lists, each with its own resolved schedule —
    /// including the disabled ones, so a config's full intent stays legible.
    pub fields: Vec<FieldConfig>,
}

/// One field and its polling frequency
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldConfig {
    /// Canonical query name, e.g. `"RUNNING_STATE"`.
    pub name: String,
    /// How often to poll it, or `None` for never (`interval_secs: 0`).
    pub interval: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse config *text* with a stated environment rather than a file: these
    /// tests exercise the same merge and the same serde surface the real loader
    /// uses, with no filesystem and no process environment involved.
    ///
    /// `vars` is spelled the way an operator would spell it, `SISMATIC_SERVER__` and
    /// all, because [`env_source`] filters an injected map by prefix exactly as
    /// it filters the real environment — so a test that gets the prefix wrong
    /// fails the same way a deployment would.
    fn try_raw(text: &str, vars: &[(&str, &str)]) -> Result<RawServerConfig, ConfigError> {
        let vars = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        raw_config(
            config::File::from_str(text, config::FileFormat::Yaml),
            env_source().source(Some(vars)),
        )
    }

    fn raw(text: &str) -> RawServerConfig {
        try_raw(text, &[]).expect("building config")
    }

    fn resolve(base: &str, text: &str) -> ServerConfig {
        resolve_config(Path::new(base), raw(text))
    }

    /// [`resolve`] with an environment layered over the text.
    fn resolve_env(base: &str, text: &str, vars: &[(&str, &str)]) -> ServerConfig {
        resolve_config(
            Path::new(base),
            try_raw(text, vars).expect("building config"),
        )
    }

    /// `(name, secs)` pairs — `None` being *never* — the shape assertions about
    /// a schedule read best in.
    fn schedule(cfg: &ServerConfig) -> Vec<(&str, Option<u64>)> {
        cfg.sync
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.interval.map(|i| i.as_secs())))
            .collect()
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
            "defaults:\n  interval_secs: 7\n  port: 9999\nsync:\n  fields: [FIRMWARE]\nhttp:\n  host: 0.0.0.0\n",
        );
        // section unset -> defaults table
        assert_eq!(cfg.sync.default_interval, Some(Duration::from_secs(7)));
        assert_eq!(cfg.http.port, 9999);
        // section set -> section wins over the built-ins
        assert_eq!(schedule(&cfg), [("FIRMWARE", Some(7))]);
        assert_eq!(cfg.http.host, "0.0.0.0");
    }

    #[test]
    fn a_bare_field_name_inherits_the_sync_interval() {
        let cfg = resolve(
            "",
            "sync:\n  interval_secs: 5\n  fields: [RUNNING_STATE, FIRMWARE]\n",
        );
        assert_eq!(
            schedule(&cfg),
            [("RUNNING_STATE", Some(5)), ("FIRMWARE", Some(5))]
        );
    }

    #[test]
    fn a_field_may_pin_its_own_interval_above_the_sync_default() {
        let cfg = resolve(
            "",
            "sync:\n  interval_secs: 5\n  fields:\n    - RUNNING_STATE\n    - name: FIRMWARE\n      interval_secs: 3600\n",
        );
        assert_eq!(
            schedule(&cfg),
            [("RUNNING_STATE", Some(5)), ("FIRMWARE", Some(3600))]
        );
    }

    #[test]
    fn a_field_table_without_an_interval_still_inherits() {
        // The table spelling is about *being able* to override, not about
        // having to: naming a field as a table must not change its schedule.
        let cfg = resolve(
            "",
            "sync:\n  interval_secs: 5\n  fields:\n    - name: RUNNING_STATE\n",
        );
        assert_eq!(schedule(&cfg), [("RUNNING_STATE", Some(5))]);
    }

    #[test]
    fn a_field_override_survives_every_layer_below_it() {
        // defaults -> sync -> field, each beating the one before.
        let cfg = resolve(
            "",
            "defaults:\n  interval_secs: 7\nsync:\n  interval_secs: 5\n  fields:\n    - RUNNING_STATE\n    - name: FIRMWARE\n      interval_secs: 3600\n",
        );
        assert_eq!(cfg.sync.default_interval, Some(Duration::from_secs(5)));
        assert_eq!(
            schedule(&cfg),
            [("RUNNING_STATE", Some(5)), ("FIRMWARE", Some(3600))]
        );
    }

    #[test]
    fn fields_inherit_the_defaults_table_when_sync_names_no_interval() {
        let cfg = resolve(
            "",
            "defaults:\n  interval_secs: 7\nsync:\n  fields:\n    - RUNNING_STATE\n    - name: FIRMWARE\n      interval_secs: 3600\n",
        );
        assert_eq!(
            schedule(&cfg),
            [("RUNNING_STATE", Some(7)), ("FIRMWARE", Some(3600))]
        );
    }

    #[test]
    fn the_built_in_interval_reaches_fields_that_override_nothing() {
        let cfg = resolve("", "sync:\n  fields: [RUNNING_STATE]\n");
        assert_eq!(
            schedule(&cfg),
            [("RUNNING_STATE", Some(DEFAULT_INTERVAL_SECS))]
        );
    }

    #[test]
    fn per_field_intervals_work_in_the_defaults_table_too() {
        // `[defaults]` accepts the same field spelling as `[sync]`, so pinning a
        // fleet-wide schedule there loses no expressiveness.
        let cfg = resolve(
            "",
            "defaults:\n  interval_secs: 5\n  fields:\n    - RUNNING_STATE\n    - name: FIRMWARE\n      interval_secs: 3600\n",
        );
        assert_eq!(
            schedule(&cfg),
            [("RUNNING_STATE", Some(5)), ("FIRMWARE", Some(3600))]
        );
    }

    /// What a field resolved to, by name — the shape wildcard assertions read
    /// best in, since the expansion is far too long to write out.
    fn interval_of(cfg: &ServerConfig, name: &str) -> Option<Option<u64>> {
        cfg.sync
            .fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.interval.map(|i| i.as_secs()))
    }

    #[test]
    fn the_wildcard_expands_to_exactly_the_fields_core_can_query() {
        // The property that makes the wildcard worth having: this is core's
        // catalog, so a query added there is polled with no config edit. If this
        // ever needs updating because core grew a field, the wildcard is broken.
        let cfg = resolve("", "sync:\n  fields: ['*']\n");

        let expanded: Vec<&str> = cfg.sync.fields.iter().map(|f| f.name.as_str()).collect();
        let catalog: Vec<&str> = Query::ALL.iter().map(|q| q.name()).collect();
        assert_eq!(expanded, catalog);
    }

    #[test]
    fn the_wildcard_fills_with_the_inherited_interval() {
        let cfg = resolve("", "sync:\n  interval_secs: 300\n  fields: ['*']\n");
        assert_eq!(interval_of(&cfg, "RUNNING_STATE"), Some(Some(300)));
        assert_eq!(interval_of(&cfg, "MAC_ADDRESS"), Some(Some(300)));
    }

    #[test]
    fn an_override_beats_the_wildcard_and_leaves_the_rest_alone() {
        let cfg = resolve(
            "",
            "sync:\n  interval_secs: 300\n  fields:\n    - '*'\n    - name: RUNNING_STATE\n      interval_secs: 5\n    - name: MAC_ADDRESS\n      interval_secs: 0\n",
        );
        assert_eq!(interval_of(&cfg, "RUNNING_STATE"), Some(Some(5)));
        assert_eq!(interval_of(&cfg, "MAC_ADDRESS"), Some(None));
        // ...and every other field still came along at the inherited interval.
        assert_eq!(interval_of(&cfg, "FIRMWARE"), Some(Some(300)));
        assert_eq!(cfg.sync.fields.len(), Query::ALL.len());
    }

    #[test]
    fn an_override_above_the_wildcard_works_the_same_as_one_below_it() {
        // The wildcard fills rather than asserts, so neither ordering can
        // silently lose the override.
        let above = resolve(
            "",
            "sync:\n  interval_secs: 300\n  fields:\n    - name: RUNNING_STATE\n      interval_secs: 5\n    - '*'\n",
        );
        let below = resolve(
            "",
            "sync:\n  interval_secs: 300\n  fields:\n    - '*'\n    - name: RUNNING_STATE\n      interval_secs: 5\n",
        );
        assert_eq!(interval_of(&above, "RUNNING_STATE"), Some(Some(5)));
        assert_eq!(interval_of(&below, "RUNNING_STATE"), Some(Some(5)));
        assert_eq!(above.sync.fields.len(), below.sync.fields.len());
    }

    #[test]
    fn the_wildcard_may_pin_the_interval_it_fills_with() {
        let cfg = resolve(
            "",
            "sync:\n  interval_secs: 30\n  fields:\n    - name: '*'\n      interval_secs: 600\n",
        );
        assert_eq!(interval_of(&cfg, "FIRMWARE"), Some(Some(600)));
        assert_eq!(interval_of(&cfg, "RUNNING_STATE"), Some(Some(600)));
    }

    #[test]
    fn a_disabled_wildcard_lists_the_catalog_switched_off() {
        // The opt-in shape: enumerate everything as *never*, then name the few
        // fields worth polling.
        let cfg = resolve(
            "",
            "sync:\n  fields:\n    - name: '*'\n      interval_secs: 0\n    - name: RUNNING_STATE\n      interval_secs: 5\n",
        );
        assert_eq!(interval_of(&cfg, "RUNNING_STATE"), Some(Some(5)));
        assert_eq!(interval_of(&cfg, "FIRMWARE"), Some(None));
        assert_eq!(cfg.sync.fields.len(), Query::ALL.len());
    }

    #[test]
    fn a_field_is_emitted_once_however_often_it_is_mentioned() {
        let cfg = resolve(
            "",
            "sync:\n  interval_secs: 30\n  fields:\n    - '*'\n    - '*'\n    - RUNNING_STATE\n    - name: RUNNING_STATE\n      interval_secs: 5\n",
        );
        assert_eq!(cfg.sync.fields.len(), Query::ALL.len());
        // ...and among explicit mentions the last one decides.
        assert_eq!(interval_of(&cfg, "RUNNING_STATE"), Some(Some(5)));
    }

    #[test]
    fn the_wildcard_works_from_the_defaults_table_too() {
        let cfg = resolve("", "defaults:\n  interval_secs: 300\n  fields: ['*']\n");
        assert_eq!(cfg.sync.fields.len(), Query::ALL.len());
        assert_eq!(interval_of(&cfg, "FIRMWARE"), Some(Some(300)));
    }

    #[test]
    fn without_a_wildcard_only_the_named_fields_are_polled() {
        // The wildcard is opt-in: naming fields explicitly must not drag the
        // rest of the catalog in behind them.
        let cfg = resolve("", "sync:\n  fields: [RUNNING_STATE]\n");
        assert_eq!(cfg.sync.fields.len(), 1);
        assert_eq!(interval_of(&cfg, "FIRMWARE"), None);
    }

    #[test]
    fn a_zero_interval_disables_that_field_without_removing_it() {
        // The field stays in the list — that is the point of `0` over deleting
        // the entry — but resolves to no schedule at all.
        let cfg = resolve(
            "",
            "sync:\n  interval_secs: 5\n  fields:\n    - RUNNING_STATE\n    - name: FIRMWARE\n      interval_secs: 0\n",
        );
        assert_eq!(
            schedule(&cfg),
            [("RUNNING_STATE", Some(5)), ("FIRMWARE", None)]
        );
    }

    #[test]
    fn a_zero_default_disables_every_field_that_inherits_it() {
        // `0` is not special-cased before the layering, so it falls through the
        // layers like any other value: off by default, on where named.
        let cfg = resolve(
            "",
            "sync:\n  interval_secs: 0\n  fields:\n    - FIRMWARE\n    - name: RUNNING_STATE\n      interval_secs: 5\n",
        );
        assert_eq!(cfg.sync.default_interval, None);
        assert_eq!(
            schedule(&cfg),
            [("FIRMWARE", None), ("RUNNING_STATE", Some(5))]
        );
    }

    #[test]
    fn a_zero_in_the_defaults_table_disables_just_as_well() {
        let cfg = resolve(
            "",
            "defaults:\n  interval_secs: 0\nsync:\n  fields: [FIRMWARE]\n",
        );
        assert_eq!(schedule(&cfg), [("FIRMWARE", None)]);
    }

    #[test]
    fn a_field_may_re_enable_itself_above_a_zero_default() {
        // The inverse of the case above: an explicit interval beats an inherited
        // *never*, so `0` in `[sync]` is an opt-in switch rather than a veto.
        let cfg = resolve(
            "",
            "defaults:\n  interval_secs: 0\nsync:\n  fields:\n    - name: RUNNING_STATE\n      interval_secs: 5\n",
        );
        assert_eq!(schedule(&cfg), [("RUNNING_STATE", Some(5))]);
    }

    #[test]
    fn an_empty_config_resolves_entirely_to_built_in_defaults() {
        let cfg = resolve("", "{}");
        assert_eq!(
            cfg,
            ServerConfig {
                devices_config_path: PathBuf::from(DEFAULT_DEVICES_CONFIG_PATH),
                intent_relay: IntentRelayConfig {
                    poll: Duration::from_millis(DEFAULT_INTENT_RELAY_POLL_MS),
                    max_attempts: DEFAULT_MAX_ATTEMPTS,
                },
                sync: SyncConfig {
                    default_interval: Some(Duration::from_secs(DEFAULT_INTERVAL_SECS)),
                    fields: vec![FieldConfig {
                        name: "RUNNING_STATE".to_owned(),
                        interval: Some(Duration::from_secs(DEFAULT_INTERVAL_SECS)),
                    }],
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
        assert_eq!(cfg.sync.default_interval, Some(Duration::from_secs(1)));
        // ...and the built-in field list picks up the interval that was named.
        assert_eq!(schedule(&cfg), [("RUNNING_STATE", Some(1))]);
    }

    // ---- the intent relay section ----------------------------------------

    #[test]
    fn the_intent_relay_section_is_read() {
        let cfg = resolve("", "intent_relay:\n  poll_ms: 50\n  max_attempts: 7\n");
        assert_eq!(
            cfg.intent_relay,
            IntentRelayConfig {
                poll: Duration::from_millis(50),
                max_attempts: 7,
            }
        );
    }

    #[test]
    fn a_partial_intent_relay_section_is_accepted() {
        let cfg = resolve("", "intent_relay:\n  max_attempts: 1\n");
        assert_eq!(cfg.intent_relay.max_attempts, 1);
        assert_eq!(
            cfg.intent_relay.poll,
            Duration::from_millis(DEFAULT_INTENT_RELAY_POLL_MS)
        );
    }

    /// Unlike `sync`'s `interval_secs`, a zero here is not "never". A relay that
    /// never drains would accept commands and perform none of them, and a zero
    /// period panics `tokio::time::interval` — so it is read as "as fast as
    /// possible" rather than taking the process down at startup.
    #[test]
    fn a_zero_poll_is_a_floor_rather_than_a_never() {
        let cfg = resolve("", "intent_relay:\n  poll_ms: 0\n");
        assert_eq!(cfg.intent_relay.poll, Duration::from_millis(1));
    }

    /// The section is reachable through the environment like every other key,
    /// which is what a container deployment sets it with.
    #[test]
    fn the_intent_relay_section_can_be_set_from_the_environment() {
        let cfg = resolve_env(
            "",
            "{}",
            &[("SISMATIC_SERVER__INTENT_RELAY__POLL_MS", "75")],
        );
        assert_eq!(cfg.intent_relay.poll, Duration::from_millis(75));
    }

    /// `deny_unknown_fields` covers the new section too, so a typo is a startup
    /// error rather than a default silently standing.
    #[test]
    fn a_misspelled_intent_relay_key_is_an_error() {
        let err = try_raw("intent_relay:\n  pollms: 50\n", &[]).unwrap_err();
        assert!(
            err.to_string().contains("pollms"),
            "expected the unknown key named, got: {err}"
        );
    }

    /// The overrides an operator who typed every flag would produce.
    fn overrides(devices_config_path: &str, host: &str, port: u16) -> Overrides {
        Overrides {
            devices_config_path: Some(PathBuf::from(devices_config_path)),
            host: Some(host.to_owned()),
            port: Some(port),
        }
    }

    #[test]
    fn a_command_line_override_beats_every_layer_of_the_file() {
        let cfg = resolve(
            "",
            "devices_config_path: from-the-file.toml\ndefaults:\n  port: 9999\nhttp:\n  host: 0.0.0.0\n",
        )
        .with_overrides(overrides("/srv/typed.toml", "::1", 3000));

        assert_eq!(cfg.devices_config_path, PathBuf::from("/srv/typed.toml"));
        assert_eq!(cfg.http.host, "::1");
        assert_eq!(cfg.http.port, 3000);
    }

    #[test]
    fn an_unnamed_override_leaves_the_resolved_value_alone() {
        // The half-named case is the one worth pinning: `--port` must not drag
        // the built-in host along behind it and clobber what the file said, and
        // must leave the sections it says nothing about entirely alone.
        let resolved = resolve(
            "",
            "devices_config_path: from-the-file.toml\nhttp:\n  host: 0.0.0.0\n  port: 9999\n",
        );
        let cfg = resolved.clone().with_overrides(Overrides {
            port: Some(3000),
            ..Overrides::default()
        });

        assert_eq!(cfg.http.port, 3000);
        assert_eq!(cfg.http.host, "0.0.0.0");
        assert_eq!(cfg.devices_config_path, resolved.devices_config_path);
        assert_eq!(cfg.sync, resolved.sync);
    }

    #[test]
    fn overriding_nothing_is_the_identity() {
        // The property that makes one override step safe to apply
        // unconditionally: the composition root never has to ask whether the
        // operator typed anything before calling it.
        let cfg = resolve(
            "",
            "devices_config_path: from-the-file.toml\nhttp:\n  host: 0.0.0.0\n  port: 9999\n",
        );
        assert_eq!(cfg.clone().with_overrides(Overrides::default()), cfg);
    }

    #[test]
    fn a_command_line_path_is_not_anchored_to_the_config_directory() {
        // The one asymmetry worth stating in a test rather than only in prose:
        // the same relative path means different things by the route it took.
        // A document travels with the deployment, so its path anchors to the
        // config's directory; a flag travels with the operator, so its path is
        // theirs to resolve against the working directory they typed it in.
        let resolved = resolve("/etc/sismatic", "devices_config_path: devices.toml\n");
        assert_eq!(
            resolved.devices_config_path,
            PathBuf::from("/etc/sismatic/devices.toml")
        );

        let cfg = resolved.with_overrides(Overrides {
            devices_config_path: Some(PathBuf::from("devices.toml")),
            ..Overrides::default()
        });
        assert_eq!(cfg.devices_config_path, PathBuf::from("devices.toml"));
    }

    #[test]
    fn applying_two_override_sets_is_applying_their_merge() {
        // `with_overrides` is a monoid homomorphism from `Overrides` under
        // left-biased choice into endomorphisms under composition. Practically:
        // a future second source of overrides can be merged in first or applied
        // second, and neither ordering re-derives the precedence rule.
        let cfg = resolve("", "http:\n  host: 0.0.0.0\n  port: 9999\n");
        let inner = Overrides {
            host: Some("::1".to_owned()),
            ..Overrides::default()
        };
        let outer = Overrides {
            port: Some(3000),
            ..Overrides::default()
        };

        let sequential = cfg
            .clone()
            .with_overrides(inner.clone())
            .with_overrides(outer.clone());
        let merged = cfg.with_overrides(Overrides {
            devices_config_path: outer.devices_config_path.or(inner.devices_config_path),
            host: outer.host.or(inner.host),
            port: outer.port.or(inner.port),
        });
        assert_eq!(sequential, merged);
    }

    #[test]
    fn an_environment_variable_beats_the_file_at_the_key_it_names() {
        let cfg = resolve_env(
            "",
            "http:\n  host: 0.0.0.0\n  port: 9999\n",
            &[("SISMATIC_SERVER__HTTP__PORT", "1234")],
        );
        assert_eq!(cfg.http.port, 1234);
        // ...and says nothing about the key beside it: the two live in one
        // table, and tables merge rather than overwrite.
        assert_eq!(cfg.http.host, "0.0.0.0");
    }

    #[test]
    fn every_section_is_reachable_by_its_key_path() {
        // The property that makes the scheme worth adopting: one derivation rule
        // — `SISMATIC_SERVER__` + the path, upper-cased — reaches a top-level key, a
        // nested one, and the `[defaults]` table alike, with nothing per-key
        // written down anywhere in this crate.
        let cfg = resolve_env(
            "",
            "{}",
            &[
                ("SISMATIC_SERVER__DEVICES_CONFIG_PATH", "/srv/pool.toml"),
                ("SISMATIC_SERVER__SYNC__INTERVAL_SECS", "5"),
                ("SISMATIC_SERVER__HTTP__HOST", "0.0.0.0"),
                ("SISMATIC_SERVER__DEFAULTS__PORT", "9000"),
                // `[defaults]` takes the same field spelling `[sync]` does, so
                // it needs the same list handling to be reachable at all.
                ("SISMATIC_SERVER__DEFAULTS__FIELDS", "FIRMWARE,UNIT_NAME"),
            ],
        );
        assert_eq!(cfg.devices_config_path, PathBuf::from("/srv/pool.toml"));
        assert_eq!(cfg.sync.default_interval, Some(Duration::from_secs(5)));
        assert_eq!(cfg.http.host, "0.0.0.0");
        assert_eq!(cfg.http.port, 9000);
        assert_eq!(
            schedule(&cfg),
            [("FIRMWARE", Some(5)), ("UNIT_NAME", Some(5))]
        );
    }

    #[test]
    fn the_environment_writes_the_document_rather_than_outranking_it() {
        // The distinction the module docs insist on: `[defaults]` from the
        // environment is still `[defaults]`, so it loses to an `[http]` the file
        // states, exactly as it would if the file had written both.
        let cfg = resolve_env(
            "",
            "http:\n  host: 0.0.0.0\n",
            &[("SISMATIC_SERVER__DEFAULTS__HOST", "10.0.0.1")],
        );
        assert_eq!(cfg.http.host, "0.0.0.0");
        // The same variable does reach a file that left the section unset.
        let cfg = resolve_env("", "{}", &[("SISMATIC_SERVER__DEFAULTS__HOST", "10.0.0.1")]);
        assert_eq!(cfg.http.host, "10.0.0.1");
    }

    #[test]
    fn a_relative_path_from_the_environment_is_anchored_like_one_from_the_file() {
        // It lands in the same key, so it resolves by the same rule — there is
        // no second anchoring policy for values that arrived by another route.
        let cfg = resolve_env(
            "/etc/sismatic",
            "{}",
            &[("SISMATIC_SERVER__DEVICES_CONFIG_PATH", "devices.toml")],
        );
        assert_eq!(
            cfg.devices_config_path,
            PathBuf::from("/etc/sismatic/devices.toml")
        );
    }

    #[test]
    fn a_field_list_from_the_environment_replaces_the_file_s() {
        // Lists overwrite where tables merge, so this is a replacement and not
        // an append — the config below contributes no field at all.
        let cfg = resolve_env(
            "",
            "sync:\n  interval_secs: 5\n  fields: [UNIT_NAME]\n",
            &[("SISMATIC_SERVER__SYNC__FIELDS", "RUNNING_STATE,FIRMWARE")],
        );
        assert_eq!(
            schedule(&cfg),
            [("RUNNING_STATE", Some(5)), ("FIRMWARE", Some(5))]
        );
    }

    #[test]
    fn a_one_element_field_list_is_still_a_list() {
        // The case a naive split would get wrong by producing a bare string,
        // which `fields` could not accept at all.
        let cfg = resolve_env("", "{}", &[("SISMATIC_SERVER__SYNC__FIELDS", "FIRMWARE")]);
        assert_eq!(schedule(&cfg), [("FIRMWARE", Some(DEFAULT_INTERVAL_SECS))]);
    }

    #[test]
    fn the_wildcard_survives_the_environment_intact() {
        let cfg = resolve_env(
            "",
            "sync:\n  interval_secs: 300\n",
            &[("SISMATIC_SERVER__SYNC__FIELDS", ALL_FIELDS)],
        );
        assert_eq!(cfg.sync.fields.len(), Query::ALL.len());
        assert_eq!(interval_of(&cfg, "FIRMWARE"), Some(Some(300)));
    }

    #[test]
    fn the_never_sentinel_survives_the_environment_intact() {
        // `0` is decoded after the merge like any other value, so the
        // environment inherits the sentinel without knowing about it.
        let cfg = resolve_env(
            "",
            "sync:\n  interval_secs: 5\n  fields: [FIRMWARE]\n",
            &[("SISMATIC_SERVER__SYNC__INTERVAL_SECS", "0")],
        );
        assert_eq!(cfg.sync.default_interval, None);
        assert_eq!(schedule(&cfg), [("FIRMWARE", None)]);
    }

    #[test]
    fn an_empty_variable_reads_as_unset_rather_than_as_an_empty_value() {
        // What an unset shell expansion and a bare systemd `Environment=` both
        // produce. Overriding a good host with `""` would fail at bind time,
        // far from the cause.
        let cfg = resolve_env(
            "",
            "http:\n  host: 0.0.0.0\n",
            &[("SISMATIC_SERVER__HTTP__HOST", "")],
        );
        assert_eq!(cfg.http.host, "0.0.0.0");
    }

    #[test]
    fn env_var_names_agree() {
        // The one name spelled twice: `CONFIG_PATH_ENV` is what the composition
        // root reads, `ENV_CONFIG_PATH_KEY` is what this module drops, and they
        // have to be the same variable. Derived here rather than concatenated at
        // the definition so `CONFIG_PATH_ENV` stays a literal an operator can
        // grep for.
        assert_eq!(
            CONFIG_PATH_ENV,
            format!(
                "{ENV_PREFIX}{ENV_SEPARATOR}{}",
                ENV_CONFIG_PATH_KEY.to_uppercase()
            )
        );
    }

    #[test]
    fn the_config_path_variable_is_not_a_document_key() {
        // It shares the namespace with every other variable — that is the point
        // of the naming — but it chose the file rather than saying anything
        // about its contents. Left in the document it would be an unknown field,
        // so the variable that picked the config would abort reading it.
        let cfg = resolve_env(
            "",
            "http:\n  port: 9999\n",
            &[(CONFIG_PATH_ENV, "/etc/sismatic/configuration.yaml")],
        );
        assert_eq!(cfg.http.port, 9999);
    }

    #[test]
    fn a_config_key_written_in_the_file_is_still_rejected() {
        // The other half of dropping it in the *source*: `config` never became a
        // real field, so a document naming another document is still the typo it
        // almost certainly is.
        let err = try_raw("config: other.yaml\n", &[]).unwrap_err();
        assert!(
            err.to_string().contains("config"),
            "expected the unknown key in the error, got: {err}"
        );
    }

    #[test]
    fn an_unrelated_variable_is_left_alone() {
        let cfg = resolve_env(
            "",
            "{}",
            &[
                ("PATH", "/usr/bin"),
                ("HOME", "/root"),
                // Including one that shares the prefix but not the separator:
                // the namespace this source claims is `SISMATIC_SERVER__`, so a
                // sibling binary's variables cannot leak into this document.
                ("SISMATIC_WEB__HTTP__PORT", "1234"),
            ],
        );
        assert_eq!(cfg.http.port, DEFAULT_PORT);
    }

    #[test]
    fn a_misspelled_variable_is_rejected_by_name() {
        // `deny_unknown_fields` reaches the environment exactly as it reaches
        // the file, so a typo is a startup error rather than a setting that
        // silently did nothing.
        let err = try_raw("{}", &[("SISMATIC_SERVER__HTTP__PROT", "1234")]).unwrap_err();
        assert!(
            err.to_string().contains("prot"),
            "expected the unknown key in the error, got: {err}"
        );
    }

    #[test]
    fn a_variable_that_is_not_the_key_s_type_is_rejected() {
        let err = try_raw("{}", &[("SISMATIC_SERVER__HTTP__PORT", "not-a-port")]).unwrap_err();
        assert!(
            err.to_string().contains("port"),
            "expected the offending key in the error, got: {err}"
        );
    }

    #[test]
    fn a_command_line_override_beats_the_environment_too() {
        // The top of the whole stack: flag, then variable, then file, then
        // built-in. The flag is folded in outside the resolver, so this is the
        // test that the two mechanisms compose in the stated order.
        let cfg = resolve_env(
            "",
            "http:\n  host: 0.0.0.0\n  port: 9999\n",
            &[
                ("SISMATIC_SERVER__HTTP__HOST", "10.0.0.1"),
                ("SISMATIC_SERVER__HTTP__PORT", "1234"),
            ],
        );
        assert_eq!((cfg.http.host.as_str(), cfg.http.port), ("10.0.0.1", 1234));

        let cfg = cfg.with_overrides(Overrides {
            host: Some("::1".to_owned()),
            ..Overrides::default()
        });
        assert_eq!(cfg.http.host, "::1");
        // ...and a flag the operator did not type leaves the variable standing.
        assert_eq!(cfg.http.port, 1234);
    }

    #[test]
    fn a_misspelled_key_inside_a_field_table_is_rejected_by_name() {
        // The point of hand-rolling `RawField`'s `Deserialize`: an untagged enum
        // would report only "data did not match any variant" and lose the key.
        let err = try_raw(
            "sync:\n  fields:\n    - name: FIRMWARE\n      intervl_secs: 3600\n",
            &[],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("intervl_secs"),
            "expected the unknown key in the error, got: {err}"
        );
    }

    #[test]
    fn a_field_table_missing_its_name_is_rejected() {
        let err = try_raw("sync:\n  fields:\n    - interval_secs: 3600\n", &[]).unwrap_err();
        assert!(
            err.to_string().contains("name"),
            "expected the missing key in the error, got: {err}"
        );
    }

    #[test]
    fn a_misspelled_field_is_rejected_rather_than_ignored() {
        let err = try_raw("devices_config_pth: devices.toml\n", &[]).unwrap_err();
        assert!(
            err.to_string().contains("devices_config_pth"),
            "expected the unknown key in the error, got: {err}"
        );
    }
}

#[cfg(test)]
mod shipped_config_check {
    /// The config file that ships in this crate must be one the loader accepts.
    /// `deny_unknown_fields` makes that a real question: a section present in
    /// the YAML and absent from `RawServerConfig` is a startup failure, not a
    /// setting that is quietly ignored.
    #[test]
    fn the_shipped_configuration_parses() {
        let cfg = super::get_configuration(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/server_configuration.yaml"
        ))
        .expect("the shipped server_configuration.yaml must load");
        assert_eq!(
            cfg.intent_relay,
            super::IntentRelayConfig {
                poll: std::time::Duration::from_millis(250),
                max_attempts: 3,
            }
        );
    }
}
