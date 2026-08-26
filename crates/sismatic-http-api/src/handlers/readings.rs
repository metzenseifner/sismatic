//! The readings routes — every queryable field of every device, through three
//! handlers.
//!
//! ```text
//! GET /devices/{id}/fields                     every field's latest value
//! GET /devices/{id}/fields/{field}             one field's latest value
//! GET /devices/{id}/fields/{field}/history     one field over a time span
//! ```
//!
//! The bare `GET /devices` above these is a different question — every device at
//! once, filtered and paged — and lives in
//! [`fleet_readings`](crate::handlers::fleet_readings), which has to consult the
//! catalog to know what to enumerate and therefore answers an unknown id
//! differently from the three routes here. It shares [`normalize_field`] with
//! them, so a field is spelled the same way whichever of the four is asked.
//!
//! # Why the field is a parameter and not a route each
//!
//! The obvious alternative is to generate one route per queryable field —
//! `/devices/{id}/firmware`, `/devices/{id}/ssh_port`, … — from core's
//! `Query::ALL`, the same catalog the `'*'` sync schedule expands. It would
//! work, and it is the wrong shape here for two reasons.
//!
//! The first is that it buys nothing. Adding a field to core's catalog already
//! reaches this API without a code change: `'*'` expands to the new field, the
//! sync driver starts a poll loop for it, the loop writes `Reading { field:
//! "NEW_FIELD", .. }`, and the route below serves it because the field name is
//! *data* it passes through rather than a symbol it was compiled against. The
//! drift a generated route table would prevent is drift this design cannot have.
//!
//! The second is that it would cost the seam. This crate cannot see
//! `sismatic-core` — that is the load-bearing rule of the workspace layout, and
//! the reason `FieldName` is a `String` in the contract rather than a mirrored
//! enum. Generating routes from the catalog would mean either a dependency edge
//! from the read side back to the device model, or lifting the catalog into a
//! third crate, in exchange for a URL space that is already reachable.
//!
//! What the route space *cannot* do without the catalog is tell a misspelled
//! field from an unpolled one: both are "no reading here", both are 404. That is
//! a real limitation and it is stated in [`ApiFailure::NotFound`]'s docs rather
//! than papered over.
//!
//! # Field names in a URL
//!
//! Canonical field names are `UPPER_SNAKE` (`RUNNING_STATE`), which is not how
//! anyone types a URL. `normalize_field` upper-cases and rewrites `-` to `_`,
//! so `/fields/running-state`, `/fields/running_state` and `/fields/RUNNING_STATE`
//! all name the same field, and the canonical spelling is what appears in the
//! response body.
//!
//! This is a property of *this URL space*, justified by URL convention, and not
//! a re-implementation of core's instruction parser. It happens to agree with
//! core's `normalize` — both fold case and dashes — but nothing here depends on
//! that agreement: the string is matched against what the sync side stored, and
//! the sync side stores canonical names.
//!
//! # A group id is the one 404 these routes do make
//!
//! An unknown device is an empty list, for the reason each handler gives: the
//! store cannot tell "no such device" from "this one has not answered yet".
//! A *device group* id is different — the catalog holds the configured set, so
//! "this id is a group" is a fact rather than an inference from absence — and it
//! is refused with the `/v1/readings/groups` URL that answers instead. See
//! [`crate::handlers::target`] for why only the positive hit is a claim these
//! routes are entitled to make.

use actix_web::web;
// `ApiError` is named only by the `#[utoipa::path]` response attributes below —
// the handlers never build one, they return `ApiFailure` and let it render.
use sismatic_api_types::{
    ApiError, FieldName, Reading, ReadingList, ReadingQuery, TimeSpan, Timestamp,
};
use sismatic_store::ReadStore;
use sismatic_store::catalog::DeviceCatalog;

use crate::handlers::error::ApiFailure;
use crate::handlers::target::{READINGS, reject_group};

/// The most readings one request can return.
///
/// A history request names a span, and a span can always be widened, so without
/// a ceiling the response size is the client's to choose and a year-wide span
/// over a five-second poll would try to serialize millions of rows. The cap
/// truncates rather than rejecting, because the useful answer to "more than you
/// can have" is the most recent page of it.
const MAX_LIMIT: u32 = 10_000;

/// The page size when a request does not ask for one — large enough that a
/// dashboard's default window comes back whole, small enough to be a bounded
/// response.
const DEFAULT_LIMIT: u32 = 1_000;

/// The lexicographic floor and ceiling of an RFC 3339 UTC timestamp, standing in
/// for an unbounded history request.
///
/// Not magic dates: [`TimeSpan::within`] compares timestamps as plain strings,
/// which is sound precisely because the format sorts lexicographically in
/// chronological order (see its docs). These two are therefore below and above
/// every timestamp the store can hold, which is what "no bound was given" means.
const BEGINNING_OF_TIME: &str = "0000-01-01T00:00:00Z";
const END_OF_TIME: &str = "9999-12-31T23:59:59Z";

/// The span a history query asks for, with each omitted bound read as
/// "unbounded on that side".
///
/// `pub(crate)` for the same reason [`normalize_field`] is: the group history
/// route asks the same question of the same store, and two copies of "an
/// omitted `start` means the beginning of history" are how the two routes would
/// come to disagree about what an unfiltered request means.
pub(crate) fn span_of(query: &ReadingQuery) -> TimeSpan {
    TimeSpan {
        start: query
            .start
            .clone()
            .unwrap_or_else(|| Timestamp(BEGINNING_OF_TIME.into())),
        end: query
            .end
            .clone()
            .unwrap_or_else(|| Timestamp(END_OF_TIME.into())),
    }
}

/// Cut `readings` down to the requested page, keeping its most recent rows.
///
/// Truncates from the *front*: the store returns oldest first, and a caller
/// asking for fewer rows than exist wants the recent ones. Chronological order
/// survives, so a plot of a limited response is a plot of its tail rather than
/// of a reversed series.
///
/// Shared with the group history route, where it is applied *per member* — see
/// that handler for why the ceiling is a per-series one.
pub(crate) fn truncate(readings: &mut Vec<Reading>, limit: Option<u32>) {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    if readings.len() > limit {
        readings.drain(..readings.len() - limit);
    }
}

/// Reject a `?field=` that contradicts the field named in the path.
///
/// [`ReadingQuery`] carries a `field` because it also describes a flat readings
/// query where nothing else names one. On a route whose path already has one, a
/// *disagreeing* `?field=` is a contradiction — rejected rather than ignored,
/// since silently serving a different field than the one the caller spelled out
/// is the failure mode worth ruling out.
///
/// Shared by both history routes so neither can start tolerating what the other
/// refuses.
pub(crate) fn reject_conflicting_field(
    query: &ReadingQuery,
    field: &str,
) -> Result<(), ApiFailure> {
    if let Some(asked) = &query.field {
        let asked = normalize_field(asked);
        if asked != field {
            return Err(ApiFailure::BadRequest(format!(
                "field is taken from the path ('{field}'); \
                 remove the conflicting '?field={asked}'"
            )));
        }
    }
    Ok(())
}

/// `GET /v1/readings/devices/{id}/fields` — every field's most recent value for
/// one device.
///
/// An unknown device yields an empty list rather than a 404: the store cannot
/// tell "no such device" from "this device has answered nothing yet", and
/// answering 404 would report a device that is merely unreachable as one that is
/// not configured. The device registry is the authority on existence, and it
/// lives on the write side.
#[utoipa::path(
    get,
    path = "/devices/{id}/fields",
    context_path = "/v1/readings",
    tag = "readings",
    params(("id" = String, Path, description = "Device id, as configured on the write side.")),
    responses(
        (status = 200, description = "Every field's latest value. An unknown device \
             yields an empty list rather than a 404 — see the handler's docs.",
         body = ReadingList),
        (status = 404, description = "This id names a device group, not a device. \
             The body carries the `/v1/readings/groups` URL that answers instead.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn list_fields(
    catalog: web::Data<dyn DeviceCatalog>,
    store: web::Data<dyn ReadStore>,
    path: web::Path<String>,
) -> Result<web::Json<ReadingList>, ApiFailure> {
    let device = path.into_inner();
    reject_group(&**catalog, &device, READINGS, "fields").await?;
    let readings = store.latest_all(device).await?;
    Ok(web::Json(ReadingList { readings }))
}

/// `GET /v1/readings/devices/{id}/fields/{field}` — one field's most recent
/// value.
///
/// A bare [`Reading`] rather than a one-element list: the response answers a
/// point question, and its `at` is the freshness of *this* value, which is the
/// thing a caller checks before trusting it.
#[utoipa::path(
    get,
    path = "/devices/{id}/fields/{field}",
    context_path = "/v1/readings",
    tag = "readings",
    params(
        ("id" = String, Path, description = "Device id, as configured on the write side."),
        ("field" = String, Path,
         description = "Field name. Case-insensitive, and `-` is read as `_`, so \
             `running-state` and `RUNNING_STATE` name the same field.",
         example = "RUNNING_STATE"),
    ),
    responses(
        (status = 200, description = "The field's most recent value.", body = Reading),
        (status = 404, description = "Nothing is stored for this device and field, \
             or the id names a device group. The first is not a claim that either \
             does not exist — an unknown device, an unpolled field and a device that \
             has not answered yet are one answer from here — while the second is, and \
             carries the `/v1/readings/groups` URL that answers instead.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn read_field(
    catalog: web::Data<dyn DeviceCatalog>,
    store: web::Data<dyn ReadStore>,
    path: web::Path<(String, String)>,
) -> Result<web::Json<Reading>, ApiFailure> {
    let (device, field) = path.into_inner();
    let field = normalize_field(&field);
    reject_group(&**catalog, &device, READINGS, &format!("fields/{field}")).await?;

    store
        .latest(device.clone(), field.clone())
        .await?
        .map(web::Json)
        .ok_or_else(|| {
            ApiFailure::NotFound(format!("no reading of '{field}' for device '{device}'"))
        })
}

/// `GET /v1/readings/devices/{id}/fields/{field}/history?start=&end=&limit=` —
/// one field over time, oldest first.
///
/// Every parameter is optional: omit the bounds for all of recorded history,
/// omit `limit` for `DEFAULT_LIMIT`. An empty result is a `200` with an empty
/// list, not a 404 — "nothing happened in that window" is an answer, and the
/// same request over a wider span may well return rows.
#[utoipa::path(
    get,
    path = "/devices/{id}/fields/{field}/history",
    context_path = "/v1/readings",
    tag = "readings",
    params(
        ("id" = String, Path, description = "Device id, as configured on the write side."),
        ("field" = String, Path,
         description = "Field name, normalized as on the latest-value route.",
         example = "RUNNING_STATE"),
        // The four query parameters come from `ReadingQuery`'s `IntoParams`
        // rather than being restated here, so this route documents the struct
        // the handler actually deserializes.
        ReadingQuery,
    ),
    responses(
        (status = 200, description = "The field's readings within the span, oldest \
             first. Empty is a valid answer, not a 404. Over-long results are \
             truncated to their most recent rows.",
         body = ReadingList),
        (status = 400, description = "A `?field=` that contradicts the field in the \
             path. Rejected rather than ignored, so a caller can never be served a \
             different field than the one it spelled out.",
         body = ApiError),
        (status = 404, description = "This id names a device group, not a device.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn field_history(
    catalog: web::Data<dyn DeviceCatalog>,
    store: web::Data<dyn ReadStore>,
    path: web::Path<(String, String)>,
    query: web::Query<ReadingQuery>,
) -> Result<web::Json<ReadingList>, ApiFailure> {
    let (device, field) = path.into_inner();
    let field = normalize_field(&field);
    let query = query.into_inner();
    reject_conflicting_field(&query, &field)?;
    reject_group(
        &**catalog,
        &device,
        READINGS,
        &format!("fields/{field}/history"),
    )
    .await?;

    let mut readings = store.between(device, field, span_of(&query)).await?;
    truncate(&mut readings, query.limit);

    Ok(web::Json(ReadingList { readings }))
}

/// Fold a URL path segment onto the canonical field spelling: upper-case, with
/// `-` rewritten to `_`.
///
/// `pub(crate)` because the write routes need the same fold: a metadata
/// register written as `metadata/title` and read back as `fields/TITLE` has to
/// be one field, and two copies of this rule are how they would stop being one.
pub(crate) fn normalize_field(raw: &str) -> FieldName {
    raw.replace('-', "_").to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_folds_case_and_dashes_onto_the_canonical_spelling() {
        for spelling in [
            "RUNNING_STATE",
            "running_state",
            "running-state",
            "Running-State",
        ] {
            assert_eq!(normalize_field(spelling), "RUNNING_STATE");
        }
    }

    #[test]
    fn normalization_leaves_a_canonical_name_alone() {
        // The identity case is the one that matters: the sync side writes
        // canonical names, so anything this function does to one of those is
        // damage.
        for name in ["FIRMWARE", "SNMP_PRIVATE_COMMUNITY_STRING", "MAC_ADDRESS"] {
            assert_eq!(normalize_field(name), name);
        }
    }
}
