//! Changing the fleet: what a caller states to add or replace a device, and
//! what it gets back when one leaves.
//!
//! Every other DTO describing a device is a *projection* of configuration —
//! [`DeviceSummary`] deliberately has no credential field at all, so there is
//! nothing to redact. This module is the other direction, and it is the one
//! place in the whole wire contract where a secret travels: a device cannot be
//! added without the credentials to reach it.
//!
//! # How that is kept safe
//!
//! Three things, none of which is "remember to be careful".
//!
//! * [`DeviceWrite`] is **inbound only**. No response body in this crate
//!   contains it, and the type an add or replace *answers* with is
//!   [`DeviceSummary`], which has no field a password could be written to.
//! * Its `Debug` is hand-written to redact, so a handler that logs its request
//!   body — or a `#[instrument]` that records its arguments — cannot leak one.
//!   The derived impl is the failure mode this exists to remove.
//! * `Serialize` is deliberately **not** derived. This type is deserialized from
//!   a request and never rendered, and an impl that could render it is the
//!   affordance that would eventually be used.
//!
//! [`DeviceSummary`]: crate::DeviceSummary

use serde::Deserialize;

use crate::write::Barrier;
use crate::{DeviceId, FieldName};

/// A device as a caller states it, for `POST /v1/inventory/devices` and
/// `PUT /v1/inventory/devices/{id}`.
///
/// The writable mirror of the devices file's `[[device]]` table, with the same
/// key names and the same meanings — so an operator moving a device between the
/// file and the API is not translating, which is the agreement
/// [`ConfigDocument`] already keeps for settings.
///
/// Every optional key means *the server's default applies*, not *leave it as it
/// was*. That is the difference between this and a patch, and it is why the
/// replace route is a `PUT` rather than a `PATCH`: a device is immutable, so
/// changing one produces a whole new device rather than an edited one, and a
/// body that stated only the delta would be describing something this system
/// has no representation for.
///
/// [`ConfigDocument`]: crate::ConfigDocument
#[derive(Clone, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct DeviceWrite {
    /// The id this device is addressed by, and the id its reads are filed
    /// under. Shares one namespace with group ids.
    ///
    /// Absent on a `PUT`, where the URL already names it; stating it there and
    /// disagreeing with the path is refused rather than silently resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(example = "atrium-101"))]
    pub id: Option<String>,
    #[cfg_attr(feature = "openapi", schema(example = "10.0.0.7"))]
    pub host: String,
    /// The SIS-over-SSH port. Omitted means the server's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(example = 22023))]
    pub port: Option<u16>,
    /// Omitted means the devices file's `[defaults]` supplies it — and a device
    /// the defaults cannot supply either is refused, naming the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Write-only. Never echoed by any route, never rendered by `Debug`, and
    /// never serialized — see the module docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange_secs: Option<u64>,
    /// Connect at startup and keep the connection warm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eager: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sis_keepalive_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eager_retry_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_backoff_secs: Option<u64>,
    /// Fields this device will not be asked for, by any accepted spelling.
    ///
    /// Canonicalized by the server, so `stream-name-2` and `STREAM_2_NAME` are
    /// the same entry and the summary reports the canonical form. A name that
    /// matches nothing is refused rather than ignored — a veto that silently
    /// vetoes nothing is worse than no veto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Vec<String>>))]
    pub disabled_fields: Option<Vec<FieldName>>,
    /// Consecutive refusals that take a field out of the schedule by
    /// themselves. `0` switches the inference off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_disable_after: Option<u32>,
    /// How often to retry a field the server auto-disabled. `0` is never, which
    /// is the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_heal_secs: Option<u64>,
}

/// Redacts the credential. Hand-written for exactly that: the derived impl would
/// print the password, and the whole point of a write-only field is that no
/// ordinary handling of the value can reveal it.
///
/// The same guarantee `sismatic_core::devices::config::Password` gives at the
/// other end of the same value's life, stated again here because this type is
/// what carries it across the wire and the two are different structs.
impl std::fmt::Debug for DeviceWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceWrite")
            .field("id", &self.id)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("eager", &self.eager)
            .field("disabled_fields", &self.disabled_fields)
            .finish_non_exhaustive()
    }
}

/// A device group as a caller states it, for `POST /v1/inventory/groups` and
/// `PUT /v1/inventory/groups/{id}`.
///
/// The writable mirror of the devices file's `[[group]]` table. A group is only
/// a name over devices plus a policy for what happens when they cannot act
/// together — there is nothing else to state, which is why this is four keys
/// where [`DeviceWrite`] is fourteen.
///
/// Every member must name a device that exists. A group is refused otherwise
/// rather than created empty and filled in later: a group whose members are
/// unresolvable is one a write can be addressed to and never dispatched from.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct GroupWrite {
    /// The id this group is addressed by. Shares one namespace with device ids,
    /// so a group may not take an id a device already has.
    ///
    /// Absent on a `PUT`, where the URL already names it; stating it there and
    /// disagreeing with the path is refused rather than silently resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(example = "atrium-room"))]
    pub id: Option<String>,
    /// The member device ids, in the order the group should address them.
    ///
    /// Order is preserved rather than sorted: an operator writes `[atrium,
    /// annex]` deliberately, and the read routes promise that sequence back.
    #[cfg_attr(feature = "openapi", schema(value_type = Vec<String>))]
    pub devices: Vec<DeviceId>,
    /// How long a group write waits for every member to reach the head of its
    /// queue. Omitted means the server derives it from the slowest member's
    /// connect plus exchange timeouts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(example = 15))]
    pub barrier_timeout_secs: Option<u64>,
    /// What to do when that wait runs out. Omitted means `fail_batch`.
    ///
    /// The same [`Barrier`] the read routes report, spelled the same way the
    /// devices file spells it — the three agree, so a group moves between the
    /// file, a `PUT` and a `GET` without anyone translating. A typed enum rather
    /// than a string, so an unaccepted value is refused by the extractor with
    /// the accepted ones named, before any handler runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barrier: Option<Barrier>,
}

/// What a `DELETE /v1/inventory/devices/{id}` did.
///
/// Reported rather than answered with a bare `204`, because removing a device
/// has consequences a caller cannot otherwise discover: writes it submitted are
/// now terminal, and history it could previously read may be gone. Both counts
/// are here so an operator can tell "the recorder was idle" from "eleven queued
/// writes just became moot".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Removed {
    /// The device that left.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub device: crate::DeviceId,
    /// Queued writes that were cancelled. They remain readable at
    /// `GET /v1/writes/{id}`, reporting `canceled` — purging them would turn a
    /// caller's poll into a `404` and destroy the evidence this created.
    ///
    /// Can exceed this device's own queue: a cancelled row takes its whole
    /// batch with it, so a group write counts every member's row.
    pub writes_canceled: u64,
    /// Recorded reads that were dropped, or `null` when the deployment's
    /// `store.cleanup_on_remove` is off and the history was kept.
    ///
    /// `null` rather than `0` because the two are different facts: nothing was
    /// dropped *because the policy says keep*, versus nothing was dropped
    /// because there was nothing to drop.
    pub reads_dropped: Option<u64>,
}

/// How an exported document is spelled.
///
/// The three the devices file itself accepts, so an export can be saved under
/// the extension the loader dispatches on and read back unchanged. That round
/// trip is the point of the route: it is how a fleet edited at runtime becomes
/// a file a deployment can put in version control.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// The format the devices file is written in by default.
    #[default]
    Toml,
    Yaml,
    Json,
}

impl ExportFormat {
    /// The media type an export of this format is served as.
    #[must_use]
    pub const fn content_type(self) -> &'static str {
        match self {
            // No registered type for TOML that is worth claiming; `text/plain`
            // is what makes a browser show it rather than download it, which is
            // what an operator checking an export wants.
            ExportFormat::Toml => "text/plain; charset=utf-8",
            ExportFormat::Yaml => "application/yaml",
            ExportFormat::Json => "application/json",
        }
    }

    /// The extension an exported document should be saved under, so the
    /// devices-file loader dispatches on it correctly.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            ExportFormat::Toml => "toml",
            ExportFormat::Yaml => "yaml",
            ExportFormat::Json => "json",
        }
    }
}

/// What `GET /v1/inventory/devices/export` should produce.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "openapi",
    derive(utoipa::ToSchema, utoipa::IntoParams),
    into_params(parameter_in = Query)
)]
#[serde(deny_unknown_fields)]
pub struct ExportQuery {
    /// `toml` (the default), `yaml` or `json`.
    #[serde(default)]
    pub format: ExportFormat,
    /// Fold each device's *inferred* vetoes into its `disabled_fields`.
    ///
    /// The discovery loop in one request: the fleet works out which fields a
    /// recorder refuses, this writes those findings into the document as
    /// declared vetoes, and committing the result makes them permanent — and
    /// free, since a declared veto costs no poll loop at all where an inferred
    /// one costs a timer tick.
    ///
    /// Only fields actually *disabled* are promoted. One still being watched —
    /// refused once against a threshold of two — is evidence of nothing yet, and
    /// writing it down would turn a transient into a permanent fact.
    #[serde(default)]
    pub promote_auto_disabled_fields_to_disabled_fields: bool,
    /// Include device passwords in the exported document.
    ///
    /// Off by default, and the default is the one to keep. An export without
    /// credentials is not directly loadable — the credentials come back from
    /// `[defaults]`, an environment variable, or a secret store — and that is
    /// the trade being made deliberately: an export lands in shell history,
    /// ticket attachments and CI logs, and a plaintext recorder password in any
    /// of those outlives every process that could have rotated it.
    ///
    /// Turn it on only for an export going straight to a file a secret manager
    /// owns.
    #[serde(default)]
    pub include_secrets: bool,
}
