//! Readings — the central artifact of the read (query) side.
//!
//! In the CQRS layout (design note §3) `sismatic-sync` polls devices and writes
//! rows; `sismatic-http-api` reads those rows and serves them as [`Reading`]s.
//! A dashboard only ever sees these, never a live device, which is what makes
//! device load independent of how many frontends are watching.

use serde::{Deserialize, Serialize};

use crate::value::ReadingValue;
use crate::{DeviceId, FieldName, GroupId};

/// An instant on the wire, as an RFC 3339 / ISO 8601 string, e.g.
/// `"2026-07-23T14:03:11Z"`.
///
/// Time is carried as a string on purpose. It keeps `api-types` depending on
/// `serde` alone — no date library leaks into every client and every wheel —
/// and it is the most portable, most debuggable form (a human reads the JSON; a
/// browser does `new Date(str)`). Typed time handling belongs at the edges: the
/// store uses its database's timestamp type, and a Rust client that needs
/// arithmetic parses this with `time`/`chrono` at its own boundary. The
/// invariant "this is valid RFC 3339 in UTC" is asserted where a `Timestamp` is
/// *constructed* (in the store/sync), not re-litigated on every DTO.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(as = "String"))]
// `format: date-time` is the OpenAPI counterpart of the paragraph above: the
// schema still says `string`, which is what the wire carries, and the format
// annotation is what tells a generator and a reader that the string is RFC 3339.
#[cfg_attr(
    feature = "openapi",
    derive(utoipa::ToSchema),
    schema(value_type = String, format = DateTime, example = "2026-07-23T14:03:11Z")
)]
#[serde(transparent)]
pub struct Timestamp(pub String);

impl Timestamp {
    /// Borrow the underlying RFC 3339 string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for Timestamp {
    fn from(s: String) -> Self {
        Timestamp(s)
    }
}

/// One stored reading: device `field` held value `value` as of `at`.
///
/// This is the typed form of the ad-hoc `{ "device", "name", "value" }` object
/// the current web backend emits by hand, plus the timestamp the store adds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Reading {
    /// The device this reading came from.
    // `DeviceId` and `FieldName` are aliases for `String` (see the crate root),
    // but a derive only sees the name it is written with: without this, utoipa
    // emits a `$ref` to a component it invents rather than the `type: string`
    // the field actually is, and every generated client grows a pointless
    // wrapper type. The aliases still document intent at the use site, which is
    // why they stay.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub device: DeviceId,
    /// Which field was read, named by its canonical query name (e.g.
    /// `"RUNNING_STATE"`). Kept a string so `api-types` need not mirror — and
    /// stay in lockstep with — `core`'s instruction catalog.
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "RUNNING_STATE"))]
    pub field: FieldName,
    /// The decoded value.
    pub value: ReadingValue,
    /// When the reading was taken / stored.
    pub at: Timestamp,
}

/// A closed time interval `[start, end]`, grouped as one product type because
/// the two bounds are only ever meaningful together (design note §4). Used to
/// scope a history query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TimeSpan {
    pub start: Timestamp,
    pub end: Timestamp,
}

impl TimeSpan {
    /// True if `at` falls within the closed interval `[start, end]`.
    ///
    /// Timestamps are RFC 3339 in UTC, a format that sorts lexicographically in
    /// chronological order, so the bounds can be compared as plain strings.
    pub fn within(&self, at: &Timestamp) -> bool {
        self.start.0 <= at.0 && at.0 <= self.end.0
    }
}

/// Filters for a readings query, deserialized from the URL query string, e.g.
/// `?field=RUNNING_STATE&start=...&end=...&limit=100`. Every field is optional:
/// omit `field` for all fields, omit the bounds for "latest", omit `limit` for
/// the server's default page size.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
// `IntoParams` as well as `ToSchema`, because this struct is not a body: it is
// the query string itself, and `IntoParams` is what turns it into the four
// `in: query` parameters a route documents. Deriving it here rather than
// restating the four names in the server's route attribute is the same
// single-source-of-truth argument the crate is built on — a field renamed here
// is renamed in the document, instead of the document quietly describing a
// parameter the server stopped reading.
#[cfg_attr(
    feature = "openapi",
    derive(utoipa::ToSchema, utoipa::IntoParams),
    into_params(parameter_in = Query)
)]
pub struct ReadingQuery {
    /// Restrict to one field, by canonical name. Redundant on a route that
    /// already names the field in its path, and rejected there if the two
    /// disagree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "openapi",
        param(value_type = Option<String>, example = "RUNNING_STATE"),
        schema(value_type = Option<String>)
    )]
    pub field: Option<FieldName>,
    /// Inclusive lower bound. Omitted means "from the beginning of recorded
    /// history".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", param(example = "2026-07-23T00:00:00Z"))]
    pub start: Option<Timestamp>,
    /// Inclusive upper bound. Omitted means "up to now".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", param(example = "2026-07-24T00:00:00Z"))]
    pub end: Option<Timestamp>,
    /// Maximum rows to return, counted from the most recent. Omitted means the
    /// server's default page size; the server also caps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", param(example = 100))]
    pub limit: Option<u32>,
}

/// A page of readings, wrapped in an object (rather than a bare array) so the
/// response can grow a `next`/`total` field later without breaking clients.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReadingList {
    pub readings: Vec<Reading>,
}

/// One device's latest-value snapshot, as the fleet index reports it.
///
/// Deliberately *not* [`DeviceDetail`], which carries a whole
/// [`DeviceSummary`] — host, port, `eager`, live connection status. That shape
/// answers an *inventory* question and needs the status port to fill in; this
/// one answers a *readings* question, where the id is all of the device a
/// reading is filed under. Keeping them apart is what lets the fleet route be
/// answered from the store plus a membership list, and keeps the scope split
/// (`/v1/readings` reads what was written, `/v1/inventory` reads what was
/// configured) from blurring at its widest route.
///
/// [`DeviceDetail`]: crate::device::DeviceDetail
/// [`DeviceSummary`]: crate::device::DeviceSummary
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceReadings {
    // See `Reading::device` for why the alias is spelled out for utoipa.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub device: DeviceId,
    /// The latest reading of each field, ordered by field name.
    ///
    /// Empty for a device that is configured and has never answered. That is a
    /// finding rather than an omission — "this recorder has told us nothing" is
    /// the row worth seeing on a fleet page — so the device is still listed.
    pub latest: Vec<Reading>,
}

/// A page of the fleet's latest readings, one row per device.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FleetReadings {
    /// The devices on this page, ordered by id.
    pub devices: Vec<DeviceReadings>,
    /// The `after` value that fetches the next page, or `null` when this page is
    /// the last one.
    ///
    /// Always present, so a client can loop on `while next != null` without
    /// having to tell "no more pages" from "the server does not paginate". It is
    /// the id of the last device on this page rather than an opaque token,
    /// because the page order *is* id order — an opaque token would hide a
    /// cursor a caller can already construct, and would have to be decoded
    /// somewhere to mean anything.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub next: Option<DeviceId>,
}

/// Filters for the fleet readings route, deserialized from the query string,
/// e.g. `?fields=RUNNING_STATE,FIRMWARE&where=RUNNING_STATE:stopped&limit=50`.
///
/// Separate from [`ReadingQuery`] rather than an extension of it, because the
/// two describe different questions. `ReadingQuery` scopes *one series* — one
/// field of one device, over a time span — so it carries `start`/`end` and a
/// singular `field`. This one scopes *a set of devices at one instant*, so it
/// carries no span at all and every filter it does carry is plural. Folding them
/// into one struct would document four parameters on each route that the route
/// ignores.
///
/// Every field is optional, and an omitted filter means "do not narrow on this
/// axis".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
// `IntoParams` for the same reason `ReadingQuery` derives it: the route
// documents the struct the handler actually deserializes, so a renamed
// parameter is renamed in the document rather than leaving the document
// describing one the server stopped reading.
#[cfg_attr(
    feature = "openapi",
    derive(utoipa::ToSchema, utoipa::IntoParams),
    into_params(parameter_in = Query)
)]
pub struct FleetQuery {
    /// Which fields to report, comma-separated and by canonical name — e.g.
    /// `RUNNING_STATE,FIRMWARE`. Omitted means every field each device has
    /// reported.
    ///
    /// Names are normalized as they are in a path segment: case-insensitive,
    /// and `-` read as `_`. A field nothing has reported is not an error — the
    /// store cannot tell a misspelling from an unpolled field — it simply
    /// contributes nothing to any row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "openapi",
        param(value_type = Option<String>, example = "RUNNING_STATE,FIRMWARE"),
        schema(value_type = Option<String>)
    )]
    pub fields: Option<String>,
    /// Which devices to report, comma-separated by id. Omitted means the whole
    /// configured fleet.
    ///
    /// Every id must name a configured device; one that does not is a `404`
    /// naming it, rather than a page that silently excludes it. Intersected with
    /// `group` when both are given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "openapi",
        param(value_type = Option<String>, example = "atrium-101,annex-7"),
        schema(value_type = Option<String>)
    )]
    pub devices: Option<String>,
    /// Restrict to one device group's members, by group id. Intersected with
    /// `devices` when both are given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "openapi",
        param(value_type = Option<String>, example = "atrium-room"),
        schema(value_type = Option<String>)
    )]
    pub group: Option<GroupId>,
    /// Keep only devices whose readings satisfy every `FIELD:value` predicate,
    /// comma-separated — e.g. `RUNNING_STATE:stopped,FIRMWARE:2.11`.
    ///
    /// A predicate selects *devices*, not readings: a row that satisfies it
    /// still carries every field `fields` asked for, so
    /// `?fields=FIRMWARE&where=RUNNING_STATE:stopped` reads "the firmware of
    /// every stopped recorder". The value is compared in whatever shape the
    /// device answered in, so `8080` matches a port and `stopped` matches a
    /// recording state without the caller spelling out a type.
    ///
    /// A device that has never reported the named field cannot satisfy the
    /// predicate and is excluded — which does not distinguish "not stopped"
    /// from "never answered". A row-less device is exactly what the unfiltered
    /// page shows, so that distinction is one request away.
    ///
    /// The separator is `:` so a value needs no percent-encoding, and the list
    /// separator is `,` — which a value therefore cannot contain.
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "openapi",
        param(value_type = Option<String>, example = "RUNNING_STATE:stopped"),
        schema(value_type = Option<String>)
    )]
    pub predicates: Option<String>,
    /// Maximum devices to return. Omitted means the server's default page size;
    /// the server also caps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", param(example = 50))]
    pub limit: Option<u32>,
    /// Resume after this device id, exclusive — the `next` of the previous page.
    /// Omitted starts at the first device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "openapi",
        param(value_type = Option<String>, example = "atrium-101"),
        schema(value_type = Option<String>)
    )]
    pub after: Option<DeviceId>,
}
