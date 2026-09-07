//! The server's own settings, as a caller reads and changes them.
//!
//! Every other DTO in this crate describes a *device*: what one was asked to do,
//! what it answered, which ones exist. These describe the **server** — how often
//! it polls, how much history it keeps, how fast it drains its write queue — and
//! they are the first shapes here that a request can change rather than only
//! observe.
//!
//! # Why the wire spells settings the way the config file does
//!
//! A duration is written `30d`, a size `512MiB`, and a delay whose key already
//! names its unit is a plain number (`interval_secs`, `poll_ms`). That is
//! exactly the vocabulary of the server's config document, and the agreement is
//! the point rather than a coincidence: an operator moving a value between a
//! ConfigMap and a `PATCH` body should not have to translate it, and the server
//! parses both with one parser, so a spelling accepted in the file is accepted
//! here and means the same thing.
//!
//! The same reasoning fixes how *off* is spelled. `interval_secs: 0` disables a
//! field, `cleanup_interval: "never"` disables the sweeper, `retain: "forever"`
//! disables expiry, and `max_memory: "unlimited"` removes the cap — each the
//! word its own setting uses in the file, none of them `null`. A `null` here
//! would have to mean two things at once, since in a [`ConfigPatch`] an absent
//! key already means *leave this alone*.
//!
//! # The document is a patch
//!
//! [`ConfigDocument`] states every setting, and [`ConfigPatch`] accepts every
//! setting optionally — so the body of a `GET` is a valid body for a `PATCH`,
//! and applying it changes nothing. That is what makes a read-modify-write cycle
//! safe to script: fetch the document, edit one number, send it back, and no
//! key you did not touch moves. It is also what lets a deployment hold the whole
//! document in version control and apply it wholesale.
//!
//! # What cannot change while the process runs
//!
//! [`ConfigDocument::http`] and [`ConfigDocument::devices_config_path`] are
//! reported and are not editable: the first is the socket the server is already
//! bound to, and the second names the file the device registry — with its live
//! SSH sessions — was built from. Both are carried by [`ConfigPatch`] anyway,
//! and that is deliberate. A patch that *names* one at the value already in
//! force is a no-op, so the round trip above still holds for the whole document;
//! a patch that would change one is refused, naming the setting and saying that
//! a restart is what applies it. The alternative — leaving them out of the patch
//! type — turns the same mistake into a schema error about an unknown field,
//! which says nothing about what to do next.

use serde::{Deserialize, Serialize};

use crate::FieldName;

/// Every setting this server is running under.
///
/// The body of `GET /v1/config`, and of a `PATCH` or a reload that succeeded —
/// in every case the settings *as they now stand*, so a caller never has to
/// re-read to find out what its own request produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ConfigDocument {
    /// What is polled, and how often.
    pub sync: SyncSettings,
    /// How much recorded history is kept, and how it is bounded.
    pub store: StoreSettings,
    /// How the write queue is drained.
    pub intent_relay: RelaySettings,
    /// The socket this server is bound to. Reported, not editable: rebinding a
    /// listener would drop every connection in flight, so a change here needs a
    /// restart.
    pub http: HttpSettings,
    /// The devices file this server's registry was built from. Reported, not
    /// editable, and note what that does *not* say: the file's contents are read
    /// once at startup, so a device added to it — under this path or any other —
    /// reaches the fleet by a restart and by nothing else.
    pub devices_config_path: String,
}

/// The poll schedule: one entry per field, each with its own frequency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SyncSettings {
    /// The interval an entry of `fields` inherits when it names none of its own.
    ///
    /// `0` is *never*, the spelling the config file uses. In a document this is
    /// what the layers resolved to; in a patch it re-times every field the same
    /// patch does not list — see [`SyncPatch::interval_secs`].
    #[cfg_attr(feature = "openapi", schema(example = 30))]
    pub interval_secs: u64,
    /// Every field the schedule names, in the order it names them, each with the
    /// frequency it resolved to. A field listed at `0` is listed and not polled,
    /// which is how a schedule stays legible about what it has switched off.
    pub fields: Vec<FieldSettings>,
}

/// One field of the poll schedule.
///
/// The same shape in a document and in a patch, with one difference that is the
/// reason the interval is optional: in a document it is always present, and in a
/// patch an absent `interval_secs` means *inherit* [`SyncSettings::interval_secs`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct FieldSettings {
    /// A canonical field name as `GET /v1/reads` publishes it, or `*` for every
    /// field this server can query — the same wildcard the config file accepts,
    /// expanded the same way.
    // See `Read::device` for why the alias is spelled out for utoipa.
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "RUNNING_STATE"))]
    pub name: FieldName,
    /// How often to poll it, `0` being never. Absent in a patch means inherit.
    #[cfg_attr(feature = "openapi", schema(example = 5))]
    pub interval_secs: Option<u64>,
}

/// What the store keeps, and what stops it keeping more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct StoreSettings {
    /// The oldest read to keep: a rolling window (`30d`, `2weeks`, `1h 30m`), a
    /// fixed floor (`2026-01-01T00:00:00Z`, `2026-01-01`), or `forever`.
    #[cfg_attr(feature = "openapi", schema(example = "1day"))]
    pub retain: String,
    /// How often to delete what `retain` has put out of scope, or `never`.
    #[cfg_attr(feature = "openapi", schema(example = "5m"))]
    pub cleanup_interval: String,
    /// The store's byte budget (`512MiB`, `2GB`, a plain number of bytes), or
    /// `unlimited`.
    ///
    /// Enforced on the write path rather than at each sweep, so it holds
    /// *between* cleanups — which is what makes it a cap rather than an average.
    /// Lowering it takes effect immediately: the oldest history is discarded
    /// until the store is within the new figure.
    #[cfg_attr(feature = "openapi", schema(example = "256MiB"))]
    pub max_memory: String,
}

/// How the queue of accepted writes is drained onto devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RelaySettings {
    /// How long an idle relay waits before looking for work again — a floor on
    /// how long an accepted write sits before a device hears about it.
    ///
    /// `0` is read as "as fast as possible" rather than as *never*: a relay that
    /// never drained would accept writes and perform none of them.
    #[cfg_attr(feature = "openapi", schema(example = 250))]
    pub poll_ms: u64,
    /// Total tries a write gets, not retries: `1` means a write that fails once
    /// is failed for good. Applies to writes admitted from here on; a write
    /// already in the queue keeps the budget it has spent.
    #[cfg_attr(feature = "openapi", schema(example = 3))]
    pub max_attempts: u32,
}

/// The socket the server is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct HttpSettings {
    #[cfg_attr(feature = "openapi", schema(example = "127.0.0.1"))]
    pub host: String,
    #[cfg_attr(feature = "openapi", schema(example = 8080))]
    pub port: u16,
}

/// A change to some of the settings — the body of `PATCH /v1/config`.
///
/// Every key is optional and an absent one means *leave this alone*, so a caller
/// states the settings it wants changed and nothing else. The request is applied
/// whole or not at all: a value that cannot be read, or one that would change a
/// setting fixed until restart, is refused before anything takes effect.
///
/// `deny_unknown_fields`, here and on every section, for the reason the config
/// file has it: a misspelled key is a request that silently did nothing, and the
/// only moment to say so is while the caller is still looking.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default, deny_unknown_fields)]
pub struct ConfigPatch {
    pub sync: Option<SyncPatch>,
    pub store: Option<StorePatch>,
    pub intent_relay: Option<RelayPatch>,
    /// Accepted only at the value already in force. See the module docs for why
    /// a setting that cannot change is carried by the patch type at all.
    pub http: Option<HttpSettings>,
    /// Accepted only at the value already in force.
    pub devices_config_path: Option<String>,
}

/// A change to the poll schedule.
///
/// The two keys compose in three ways, and the middle one is the reason
/// [`interval_secs`](Self::interval_secs) is worth having on its own:
///
/// * `fields` given — that list *is* the new schedule. An entry naming no
///   interval inherits `interval_secs`, exactly as in the config file.
/// * `fields` absent, `interval_secs` given — every field currently scheduled is
///   re-timed to it. "Poll everything every sixty seconds" is one key, not a
///   restatement of the whole list.
/// * both absent — the schedule is unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default, deny_unknown_fields)]
pub struct SyncPatch {
    /// The interval unpinned fields inherit, `0` being never.
    pub interval_secs: Option<u64>,
    /// The whole schedule, replacing what is running.
    pub fields: Option<Vec<FieldSettings>>,
}

/// A change to what the store keeps. Each key carries its own unit, as in the
/// config file; see [`StoreSettings`] for the accepted spellings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default, deny_unknown_fields)]
pub struct StorePatch {
    pub retain: Option<String>,
    pub cleanup_interval: Option<String>,
    pub max_memory: Option<String>,
}

/// A change to how the write queue is drained.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default, deny_unknown_fields)]
pub struct RelayPatch {
    pub poll_ms: Option<u64>,
    pub max_attempts: Option<u32>,
}

impl ConfigDocument {
    /// This document as the patch that would produce it.
    ///
    /// The identity of the whole scope, in one function: applying the result to
    /// the server the document came from changes nothing. It exists because that
    /// property is worth *testing* rather than asserting in prose — the two
    /// types are written separately and the round trip is what pairs them — and
    /// because a caller holding a document (from a `GET`, or from version
    /// control) can send it back without restating it field by field.
    #[must_use]
    pub fn as_patch(&self) -> ConfigPatch {
        ConfigPatch {
            sync: Some(SyncPatch {
                interval_secs: Some(self.sync.interval_secs),
                fields: Some(self.sync.fields.clone()),
            }),
            store: Some(StorePatch {
                retain: Some(self.store.retain.clone()),
                cleanup_interval: Some(self.store.cleanup_interval.clone()),
                max_memory: Some(self.store.max_memory.clone()),
            }),
            intent_relay: Some(RelayPatch {
                poll_ms: Some(self.intent_relay.poll_ms),
                max_attempts: Some(self.intent_relay.max_attempts),
            }),
            http: Some(self.http.clone()),
            devices_config_path: Some(self.devices_config_path.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> ConfigDocument {
        ConfigDocument {
            sync: SyncSettings {
                interval_secs: 30,
                fields: vec![FieldSettings {
                    name: "RUNNING_STATE".to_owned(),
                    interval_secs: Some(5),
                }],
            },
            store: StoreSettings {
                retain: "1day".to_owned(),
                cleanup_interval: "5m".to_owned(),
                max_memory: "256MiB".to_owned(),
            },
            intent_relay: RelaySettings {
                poll_ms: 250,
                max_attempts: 3,
            },
            http: HttpSettings {
                host: "127.0.0.1".to_owned(),
                port: 8080,
            },
            devices_config_path: "/etc/sismatic/devices.toml".to_owned(),
        }
    }

    /// The property the scope rests on, stated on the wire rather than in the
    /// types: the bytes a `GET` returns deserialize as a `PATCH` body. Whether
    /// applying it is a no-op is the server's half, tested there.
    #[test]
    fn a_document_deserializes_as_a_patch() {
        let json = serde_json::to_string(&document()).expect("serializing the document");

        let patch: ConfigPatch = serde_json::from_str(&json).expect("the document is a patch");

        assert_eq!(patch, document().as_patch());
    }

    /// A misspelled key is refused rather than silently ignored — the one thing
    /// a caller cannot notice on its own, since the response is the settings as
    /// they stand and would look exactly like a change that did not take.
    #[test]
    fn a_misspelled_key_is_refused() {
        let err = serde_json::from_str::<ConfigPatch>(r#"{"sync":{"interval_sec":30}}"#)
            .expect_err("an unknown key should not deserialize");

        assert!(err.to_string().contains("interval_sec"), "got: {err}");
    }

    /// An empty patch is a patch. It is what a caller sends to change nothing,
    /// and — since every section is `default` — what every partial body is built
    /// out of.
    #[test]
    fn an_empty_patch_names_nothing() {
        let patch: ConfigPatch = serde_json::from_str("{}").expect("an empty patch");

        assert_eq!(patch, ConfigPatch::default());
    }

    /// `0` is the disabled spelling, and it has to arrive as itself: read as
    /// *absent* it would mean "inherit", which is the opposite of switched off.
    #[test]
    fn a_disabled_field_keeps_its_zero() {
        let json = r#"{"sync":{"fields":[{"name":"FIRMWARE","interval_secs":0}]}}"#;

        let patch: ConfigPatch = serde_json::from_str(json).expect("a patch");

        let fields = patch.sync.expect("a sync section").fields.expect("fields");
        assert_eq!(fields[0].interval_secs, Some(0));
    }

    /// ...and a field that names no interval at all is the other case, which
    /// only survives while the two are distinguishable in the type.
    #[test]
    fn a_field_may_name_no_interval() {
        let json = r#"{"sync":{"fields":[{"name":"FIRMWARE"}]}}"#;

        let patch: ConfigPatch = serde_json::from_str(json).expect("a patch");

        let fields = patch.sync.expect("a sync section").fields.expect("fields");
        assert_eq!(fields[0].interval_secs, None);
    }
}
