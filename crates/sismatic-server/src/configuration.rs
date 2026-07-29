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
//! Precedence for every setting is the same three-layer fallback, most specific
//! first:
//!
//! 1. the setting's own section (`devices_config_path`, `[sync]`, `[http]`),
//! 2. the `[defaults]` table,
//! 3. the built-in constant.
//!
//! `interval_secs` adds a fourth, still-more-specific layer at the top: an entry
//! in `sync.fields` may pin its own. That is what makes a mixed schedule
//! expressible — `RUNNING_STATE` is worth polling every few seconds, `FIRMWARE`
//! changes about once a year — without every deployment restating an interval
//! for every field it lists:
//!
//! ```yaml
//! sync:
//!   interval_secs: 5          # the default every field inherits
//!   fields:
//!     - RUNNING_STATE         # inherits 5
//!     - name: FIRMWARE
//!       interval_secs: 3600   # overrides it
//! ```
//!
//! Both spellings of an entry — a bare name and a table — resolve to the same
//! [`FieldConfig`], so [`SyncConfig::fields`] is a flat list of fields that each
//! already know their own interval. Nothing downstream re-derives it.
//!
//! # The `"*"` wildcard
//!
//! An entry of `"*"` stands for *every field core knows how to query* — it
//! expands to [`Query::ALL`], the list the `instruction_catalog!` macro
//! generates alongside the [`Query`] variants themselves. Adding a query to
//! core's catalog therefore starts polling it on the next restart, with no edit
//! to any server config. (Quote it: a bare `*` opens a YAML alias.)
//!
//! ```yaml
//! sync:
//!   interval_secs: 300
//!   fields:
//!     - "*"                   # every field core knows about, at 300s
//!     - name: RUNNING_STATE
//!       interval_secs: 5      # ...except this one, which is worth watching
//!     - name: MAC_ADDRESS
//!       interval_secs: 0      # ...and this one, which never changes
//! ```
//!
//! The wildcard *fills*, it does not assert: a field named explicitly anywhere
//! in the list keeps the interval that entry gives it, whether the entry sits
//! above or below the `"*"`. That makes the expansion order-independent, so
//! there is no way to write the two lines in the "wrong" order and silently lose
//! an override. Among explicit entries the last mention of a field wins, and a
//! field is emitted once no matter how many times it is mentioned. The wildcard
//! may also carry its own `interval_secs`, which then fills every field it
//! expands to instead of the inherited default — including `0`, which lists the
//! whole catalog switched off so individual fields can be turned back on.
//!
//! `interval_secs: 0` means *never*: the field stays listed but no poll loop is
//! started for it, which is how a field is turned off without deleting it from
//! the config. That is the same sentinel core spells for `sis_keepalive_secs`
//! and `eager_retry_secs`, decoded the same way — [`resolve_config`] turns it
//! into `None` here, so no consumer downstream has to know that `0` is special
//! (and a zero [`Duration`], which `tokio::time::interval` panics on, cannot be
//! constructed at all). Because it resolves like any other value, `0` works at
//! every layer: as a field's own override, or in `[sync]`/`[defaults]` to
//! default the whole fleet to off and re-enable named fields individually.
//!
//! Relative paths resolve against the *config file's* directory rather than the
//! process's working directory, so a config and the devices file it names travel
//! together and no test (or systemd unit) has to care where it was launched from.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use config::ConfigError;
use serde::Deserialize;
use serde::de::{self, MapAccess, value::MapAccessDeserializer};
use sismatic_core::protocol::instructions::query::Query;

/// Devices file used when neither the config nor `[defaults]` names one; matches
/// the CLI's `--config` default so both front-ends agree on the convention.
const DEFAULT_DEVICES_CONFIG_PATH: &str = "devices.toml";
const DEFAULT_INTERVAL_SECS: u64 = 30;
const DEFAULT_FIELDS: &[&str] = &["RUNNING_STATE"];
/// The `sync.fields` entry standing for every field core can query. Canonical
/// query names are `UPPER_SNAKE`, so this can never collide with one.
const ALL_FIELDS: &str = "*";
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

    // The interval a field inherits when it does not pin one of its own.
    let default_interval = interval(
        sync.interval_secs
            .or(defaults.interval_secs)
            .unwrap_or(DEFAULT_INTERVAL_SECS),
    );

    // Each entry's own `interval_secs` sits above that default; folding it in
    // here is what lets every consumer read one concrete schedule per field.
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
        sync: SyncConfig {
            default_interval,
            fields,
        },
        http: HttpConfig { host, port },
    }
}

/// Decode the `interval_secs` sentinel: `0` is *never*, anything else is a
/// delay. The one place in the server that knows `0` is special — mirrors
/// core's `sis_keepalive_secs` / `eager_retry_secs`.
fn interval(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Expand `"*"` and fold per-field overrides into one ordered, duplicate-free
/// schedule.
///
/// Two passes, because the wildcard has to honour an override that appears
/// *after* it as readily as one that appears before: the first pass settles what
/// each explicitly-named field resolves to, and only then does the second pass
/// lay fields out in order, expanding the wildcard into whatever it did not
/// claim. Doing it in one pass would make the result depend on where in the list
/// the `"*"` happens to sit.
fn resolve_fields(raw: Vec<RawField>, default_interval: Option<Duration>) -> Vec<FieldConfig> {
    // Pass 1: every explicitly-named field, last mention winning.
    let mut explicit: Vec<(&str, Option<Duration>)> = Vec::new();
    for field in raw.iter().filter(|f| !f.is_wildcard()) {
        let resolved = field.interval_secs.map_or(default_interval, interval);
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
            let fill = field.interval_secs.map_or(default_interval, interval);
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
    pub fields: Option<Vec<RawField>>,
    pub host: Option<String>,
    pub port: Option<u16>,
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
/// `interval_secs` stays `Option<u64>` at this layer precisely because "unset"
/// and "written down" must remain distinguishable until [`resolve_config`]
/// collapses them — that is the whole content of an override, and it is what
/// makes an explicit `interval_secs: 0` mean *never* rather than reading as
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

/// The table spelling of a [`RawField`]. Split out as a named struct so it can
/// carry `deny_unknown_fields`: this is what makes `- name: FIRMWARE` with a
/// misspelled `intervl_secs` an error naming the key, rather than a field that
/// silently polls at the default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFieldTable {
    name: String,
    interval_secs: Option<u64>,
}

/// Hand-written rather than `#[serde(untagged)]`: untagged reports every failure
/// as "data did not match any variant", which would throw away the precise
/// unknown-key error [`RawFieldTable`] produces. Dispatching on the input's own
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
    /// The interval a field inherits when it pins none of its own, `None` if
    /// that inherited value is *never*. Already folded into every entry of
    /// `fields`, so no consumer needs it to schedule anything; it is kept
    /// because it is the answer to "what would a field added to this config poll
    /// at", which the entries alone do not give.
    pub default_interval: Option<Duration>,
    /// Every field the config lists, each with its own resolved schedule —
    /// including the disabled ones, so a config's full intent stays legible.
    pub fields: Vec<FieldConfig>,
}

/// One field and the schedule it actually polls on — the override already
/// applied, or the inherited default already substituted.
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

    #[test]
    fn a_misspelled_key_inside_a_field_table_is_rejected_by_name() {
        // The point of hand-rolling `RawField`'s `Deserialize`: an untagged enum
        // would report only "data did not match any variant" and lose the key.
        let err = config::Config::builder()
            .add_source(config::File::from_str(
                "sync:\n  fields:\n    - name: FIRMWARE\n      intervl_secs: 3600\n",
                config::FileFormat::Yaml,
            ))
            .build()
            .expect("building config")
            .try_deserialize::<RawServerConfig>()
            .unwrap_err();
        assert!(
            err.to_string().contains("intervl_secs"),
            "expected the unknown key in the error, got: {err}"
        );
    }

    #[test]
    fn a_field_table_missing_its_name_is_rejected() {
        let err = config::Config::builder()
            .add_source(config::File::from_str(
                "sync:\n  fields:\n    - interval_secs: 3600\n",
                config::FileFormat::Yaml,
            ))
            .build()
            .expect("building config")
            .try_deserialize::<RawServerConfig>()
            .unwrap_err();
        assert!(
            err.to_string().contains("name"),
            "expected the missing key in the error, got: {err}"
        );
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
