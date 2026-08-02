//! Readings — the central artifact of the read (query) side.
//!
//! In the CQRS layout (design note §3) `sismatic-sync` polls devices and writes
//! rows; `sismatic-http-api` reads those rows and serves them as [`Reading`]s.
//! A dashboard only ever sees these, never a live device, which is what makes
//! device load independent of how many frontends are watching.

use serde::{Deserialize, Serialize};

use crate::value::ReadingValue;
use crate::{DeviceId, FieldName};

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
