//! The readings routes — every queryable field of every device, through three
//! handlers.
//!
//! ```text
//! GET /devices/{id}/fields                     every field's latest value
//! GET /devices/{id}/fields/{field}             one field's latest value
//! GET /devices/{id}/fields/{field}/history     one field over a time span
//! ```
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
//! anyone types a URL. [`normalize_field`] upper-cases and rewrites `-` to `_`,
//! so `/fields/running-state`, `/fields/running_state` and `/fields/RUNNING_STATE`
//! all name the same field, and the canonical spelling is what appears in the
//! response body.
//!
//! This is a property of *this URL space*, justified by URL convention, and not
//! a re-implementation of core's instruction parser. It happens to agree with
//! core's `normalize` — both fold case and dashes — but nothing here depends on
//! that agreement: the string is matched against what the sync side stored, and
//! the sync side stores canonical names.

use actix_web::web;
use sismatic_api_types::{FieldName, Reading, ReadingList, ReadingQuery, TimeSpan, Timestamp};
use sismatic_store::ReadStore;

use crate::routes::error::ApiFailure;

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

/// `GET /devices/{id}/fields` — every field's most recent value for one device.
///
/// An unknown device yields an empty list rather than a 404: the store cannot
/// tell "no such device" from "this device has answered nothing yet", and
/// answering 404 would report a device that is merely unreachable as one that is
/// not configured. The device registry is the authority on existence, and it
/// lives on the write side.
pub async fn list_fields(
    store: web::Data<dyn ReadStore>,
    path: web::Path<String>,
) -> Result<web::Json<ReadingList>, ApiFailure> {
    let device = path.into_inner();
    let readings = store.latest_all(device).await?;
    Ok(web::Json(ReadingList { readings }))
}

/// `GET /devices/{id}/fields/{field}` — one field's most recent value.
///
/// A bare [`Reading`] rather than a one-element list: the response answers a
/// point question, and its `at` is the freshness of *this* value, which is the
/// thing a caller checks before trusting it.
pub async fn read_field(
    store: web::Data<dyn ReadStore>,
    path: web::Path<(String, String)>,
) -> Result<web::Json<Reading>, ApiFailure> {
    let (device, field) = path.into_inner();
    let field = normalize_field(&field);

    store
        .latest(device.clone(), field.clone())
        .await?
        .map(web::Json)
        .ok_or_else(|| {
            ApiFailure::NotFound(format!("no reading of '{field}' for device '{device}'"))
        })
}

/// `GET /devices/{id}/fields/{field}/history?start=&end=&limit=` — one field
/// over time, oldest first.
///
/// Every parameter is optional: omit the bounds for all of recorded history,
/// omit `limit` for [`DEFAULT_LIMIT`]. An empty result is a `200` with an empty
/// list, not a 404 — "nothing happened in that window" is an answer, and the
/// same request over a wider span may well return rows.
pub async fn field_history(
    store: web::Data<dyn ReadStore>,
    path: web::Path<(String, String)>,
    query: web::Query<ReadingQuery>,
) -> Result<web::Json<ReadingList>, ApiFailure> {
    let (device, field) = path.into_inner();
    let field = normalize_field(&field);
    let query = query.into_inner();

    // `ReadingQuery` carries a `field` because it also describes a flat readings
    // query where nothing else names one. Here the path already has, so a
    // *disagreeing* `?field=` is a contradiction. Rejected rather than ignored:
    // silently serving a different field than the one the caller spelled out is
    // the failure mode worth ruling out.
    if let Some(asked) = &query.field {
        let asked = normalize_field(asked);
        if asked != field {
            return Err(ApiFailure::BadRequest(format!(
                "field is taken from the path ('{field}'); \
                 remove the conflicting '?field={asked}'"
            )));
        }
    }

    let span = TimeSpan {
        start: query
            .start
            .unwrap_or_else(|| Timestamp(BEGINNING_OF_TIME.into())),
        end: query.end.unwrap_or_else(|| Timestamp(END_OF_TIME.into())),
    };

    let mut readings = store.between(device, field, span).await?;

    // Truncate from the *front*: the store returns oldest first, and a caller
    // asking for fewer rows than exist wants the recent ones. Chronological
    // order survives, so a plot of a limited response is a plot of its tail
    // rather than of a reversed series.
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    if readings.len() > limit {
        readings.drain(..readings.len() - limit);
    }

    Ok(web::Json(ReadingList { readings }))
}

/// Fold a URL path segment onto the canonical field spelling: upper-case, with
/// `-` rewritten to `_`.
fn normalize_field(raw: &str) -> FieldName {
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
