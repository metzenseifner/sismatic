//! The fleet readings route — every device's latest values, filtered and paged.
//!
//! ```text
//! GET /devices?fields=&devices=&group=&where=&limit=&after=
//! ```
//!
//! One route, and the only one in the scope that answers about more than one
//! addressable thing at a time. [`readings`](crate::handlers::readings) answers
//! for a device, [`group_readings`](crate::handlers::group_readings) for a
//! configured device group; this answers for *the fleet*, which is not an
//! addressable thing at all — it is whatever is left after the filters run.
//!
//! # Why it needs the catalog, unlike the device routes beside it
//!
//! [`ReadStore`] is keyed by device: every method takes an id, and there is no
//! method that enumerates them. That is not an oversight in the port, it is the
//! same seam the rest of the read side rests on — the store holds what the sync
//! side *wrote*, so it has no way to distinguish a device that exists from one
//! that has been polled, and a `devices()` on it would be answering a question
//! about configuration from a table of observations.
//!
//! So the fleet comes from the [`DeviceCatalog`] and the values come from the
//! store, exactly as the group routes fan out over configured members. What that
//! buys is the row this page exists to show: a device that is configured and has
//! answered nothing is present with `"latest": []`. Sourced from the store it
//! would simply be missing, and "the recorder we installed last week has never
//! reported" is the single most useful line on a fleet page.
//!
//! It also settles the id-space question the device routes have to hedge on. An
//! unknown id under `/devices/{id}/fields` is an empty list, because the store
//! cannot tell a typo from silence. Here the catalog can, so a `?devices=` naming
//! something unconfigured is a `404` that names it — a filter that silently
//! matched nothing would report a typo as a healthy, empty fleet.
//!
//! # Two filters, on two different axes
//!
//! `fields` picks columns; `where` picks rows. They compose rather than
//! competing, and the order is what makes the useful query expressible:
//! `?fields=FIRMWARE&where=RUNNING_STATE:stopped` is "the firmware of every
//! stopped recorder", so the predicate is evaluated against a device's *whole*
//! snapshot and only then are the requested columns kept. Evaluated the other way
//! around — project first, then filter — that query would match nothing, because
//! the column the predicate reads would already have been dropped.
//!
//! A predicate's value is compared with [`satisfies`], the same function the
//! group routes use to decide whether a member did what it was told. It reads the
//! caller's text *in the shape the device answered in*, so `8080` matches a
//! `Port`, `true` matches a `Flag` and `stopped` matches a `State`, with no
//! per-field type knowledge on this side of the seam. That is the whole reason
//! `where` costs no new comparison logic: the question "does this reading hold
//! that value" was already answered once, for a different caller.
//!
//! # Paging by device, with the id as the cursor
//!
//! A page is a whole number of devices. The alternative — paging the flat list
//! of readings — would let one device's snapshot straddle a boundary, so a client
//! rendering a row would have to buffer across pages to find out whether it had
//! all of it.
//!
//! The cursor is the last device id on the page, and the page order is the
//! catalog's own id order. Both halves matter. An id cursor is only stable if the
//! order it indexes is total and independent of the filters, which is why
//! [`candidates`] narrows the catalog's ordered list by set membership rather
//! than iterating a filter's own sequence — a device group hands back its members
//! in *configured* order, and paging over that with an id cursor would skip and
//! repeat devices.

use std::collections::BTreeSet;

use actix_web::web;
// `ApiError` is named only by the `#[utoipa::path]` response attributes below.
use sismatic_api_types::{
    ApiError, DeviceId, DeviceReadings, FieldName, FleetQuery, FleetReadings, Reading, ReadingValue,
};
use sismatic_store::ReadStore;
use sismatic_store::catalog::DeviceCatalog;
use sismatic_store::group::satisfies;

use crate::handlers::error::ApiFailure;
use crate::handlers::readings::normalize_field;
use crate::handlers::target::{READINGS, group_members};

/// The most devices one page can hold.
///
/// Counted in devices rather than readings, so the response is bounded by
/// `MAX_LIMIT × fields-per-device` rather than by a row count. That is the honest
/// shape of the ceiling: a caller narrowing with `?fields=` pays for what it
/// asked for, and one that does not is asking for every field of a thousand
/// recorders and should get a bounded answer rather than a rejection.
const MAX_LIMIT: u32 = 1_000;

/// The page size when a request does not ask for one — a screen's worth of a
/// fleet, rather than the whole of one.
const DEFAULT_LIMIT: u32 = 100;

/// `GET /v1/readings/devices?fields=&devices=&group=&where=&limit=&after=` —
/// every configured device's latest values, one row per device, ordered by id.
///
/// Every filter is optional: omit them all for the whole fleet with every field
/// each device has reported. A device that is configured and has never answered
/// is a row with an empty `latest` rather than an absent one — see the module
/// docs for why that row is the point.
#[utoipa::path(
    get,
    path = "/devices",
    context_path = "/v1/readings",
    tag = "readings",
    params(
        // From `FleetQuery`'s `IntoParams`, so this route documents the struct
        // the handler actually deserializes — as the history routes do.
        FleetQuery,
    ),
    responses(
        (status = 200, description = "A page of the fleet's latest readings, one row \
             per device, ordered by id. `next` carries the `after` value for the \
             following page, or `null` on the last one. A configured device that has \
             never answered is a row with an empty `latest`.",
         body = FleetReadings),
        (status = 400, description = "A malformed `?where=` predicate, or `?limit=0`. \
             The body says which.", body = ApiError),
        (status = 404, description = "A `?devices=` id that names no configured device, \
             or a `?group=` id that names no configured device group. Unlike the \
             per-device readings routes' answer for an unknown id, this is a claim \
             about configuration — the catalog is what this route enumerates.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn list_fleet(
    catalog: web::Data<dyn DeviceCatalog>,
    store: web::Data<dyn ReadStore>,
    query: web::Query<FleetQuery>,
) -> Result<web::Json<FleetReadings>, ApiFailure> {
    let query = query.into_inner();

    let fields = wanted_fields(&query);
    let predicates = predicates_of(&query)?;
    let limit = page_size(query.limit)?;
    let candidates = candidates(&**catalog, &query).await?;

    let mut devices: Vec<DeviceReadings> = Vec::new();
    let mut next = None;

    for device in candidates {
        // The whole snapshot, because a predicate may read a field `?fields=`
        // does not ask for. Projection happens after the row survives.
        let latest = store.latest_all(device.clone()).await?;
        if !predicates.iter().all(|predicate| predicate.holds(&latest)) {
            continue;
        }

        // One match past a full page is what tells "there is more" from "that
        // was the last one", and it is the only device read beyond the page —
        // the scan stops here rather than filtering the whole fleet first, so a
        // `?limit=50` over a thousand recorders costs fifty-one store reads and
        // not a thousand.
        if devices.len() == limit {
            next = devices.last().map(|row| row.device.clone());
            break;
        }

        devices.push(DeviceReadings {
            device,
            latest: project(latest, fields.as_ref()),
        });
    }

    Ok(web::Json(FleetReadings { devices, next }))
}

/// The devices this request addresses, in page order.
///
/// Narrows the catalog's own ordered list by set membership rather than
/// iterating each filter's sequence, which is what keeps the page order — and
/// therefore the id cursor — independent of which filters were given. See the
/// module docs.
async fn candidates(
    catalog: &dyn DeviceCatalog,
    query: &FleetQuery,
) -> Result<Vec<DeviceId>, ApiFailure> {
    let fleet = catalog.devices().await;

    // `None` is "every device", which is not the same set as an empty
    // `BTreeSet` and would be indistinguishable from one if the filters were
    // folded into a single collection built up from nothing.
    let mut keep: Option<BTreeSet<DeviceId>> = None;

    if let Some(group) = &query.group {
        // The same lookup the group routes make, so a device id in `?group=` is
        // refused with the same message they would give it.
        let members = group_members(catalog, group, READINGS, "fields").await?;
        keep = Some(members.into_iter().collect());
    }

    let named = csv(&query.devices);
    if !named.is_empty() {
        let mut requested = BTreeSet::new();
        for id in named {
            if catalog.device(id).await.is_none() {
                return Err(unknown_device(catalog, id).await);
            }
            requested.insert(id.to_owned());
        }
        // Intersected rather than unioned when `?group=` is also given: two
        // filters on one request narrow, and "these two devices, if they are in
        // that group" is a question with an answer. Neither is a contradiction
        // the way a `?field=` disagreeing with a path segment is, so neither is
        // refused.
        keep = Some(match keep {
            Some(group) => group.intersection(&requested).cloned().collect(),
            None => requested,
        });
    }

    let after = query.after.as_deref();
    Ok(fleet
        .into_iter()
        .map(|device| device.id)
        .filter(|id| keep.as_ref().is_none_or(|keep| keep.contains(id)))
        // Exclusive, so `after=<the previous page's next>` resumes rather than
        // repeating its last row.
        .filter(|id| after.is_none_or(|after| id.as_str() > after))
        .collect())
}

/// The `404` for an id in `?devices=` that names no device.
///
/// A group id there is a different mistake from a typo, and the fix is a
/// different *parameter* rather than a different URL — which is why this does not
/// go through [`target::reject_group`], whose whole job is to name the route in
/// the other id space. This route already answers for groups, one parameter over.
///
/// [`target::reject_group`]: crate::handlers::target
async fn unknown_device(catalog: &dyn DeviceCatalog, id: &str) -> ApiFailure {
    if catalog.group(id).await.is_some() {
        return ApiFailure::NotFound(format!(
            "'{id}' is a device group, not a device; ask for its members with ?group={id}"
        ));
    }
    ApiFailure::NotFound(format!("no device '{id}' is configured"))
}

/// One `FIELD:value` row filter.
struct Predicate {
    field: FieldName,
    /// The caller's text, held as [`ReadingValue::Text`] so [`satisfies`] reads
    /// it in whatever shape the device answered in. Nothing here parses the
    /// value, because nothing here knows what shape the field holds — the
    /// reading does.
    want: ReadingValue,
}

impl Predicate {
    fn parse(raw: &str) -> Result<Self, ApiFailure> {
        let (field, want) = raw.split_once(':').ok_or_else(|| {
            ApiFailure::BadRequest(format!(
                "'{raw}' is not a filter; write one as 'FIELD:value', \
                 e.g. 'RUNNING_STATE:stopped'"
            ))
        })?;

        let field = normalize_field(field.trim());
        if field.is_empty() {
            return Err(ApiFailure::BadRequest(format!(
                "'{raw}' names no field; write a filter as 'FIELD:value'"
            )));
        }

        Ok(Predicate {
            field,
            want: ReadingValue::Text(want.to_owned()),
        })
    }

    /// Whether `latest` — one device's whole snapshot — satisfies this filter.
    ///
    /// A device with no reading of the named field holds nothing that could
    /// satisfy it and is excluded. That deliberately does not distinguish "not
    /// stopped" from "never answered"; the unfiltered page shows the second as
    /// an empty row, so the distinction is one request away rather than absent.
    fn holds(&self, latest: &[Reading]) -> bool {
        latest
            .iter()
            .any(|reading| reading.field == self.field && satisfies(&self.want, &reading.value))
    }
}

/// The `?where=` predicates, or an empty list if none were given.
fn predicates_of(query: &FleetQuery) -> Result<Vec<Predicate>, ApiFailure> {
    csv(&query.predicates)
        .into_iter()
        .map(Predicate::parse)
        .collect()
}

/// The fields `?fields=` asked for, or `None` for "every field".
///
/// `None` rather than an empty set, because the two mean opposite things and an
/// empty `?fields=` is the caller declining to narrow rather than asking for
/// nothing.
fn wanted_fields(query: &FleetQuery) -> Option<BTreeSet<FieldName>> {
    let fields: BTreeSet<FieldName> = csv(&query.fields)
        .into_iter()
        .map(normalize_field)
        .collect();
    (!fields.is_empty()).then_some(fields)
}

/// Keep only the requested columns, preserving the store's field ordering.
fn project(latest: Vec<Reading>, fields: Option<&BTreeSet<FieldName>>) -> Vec<Reading> {
    match fields {
        None => latest,
        Some(fields) => latest
            .into_iter()
            .filter(|reading| fields.contains(&reading.field))
            .collect(),
    }
}

/// How many devices this page may hold.
///
/// Over-large is clamped and zero is refused, which is not the inconsistency it
/// looks like. Clamping still yields a usable page *and* a cursor that advances,
/// so truncating to `MAX_LIMIT` costs the caller an extra round trip and nothing
/// else. A page of zero devices has no last row, so it can produce no cursor —
/// a caller that accepted one would loop forever on a `next` that is always
/// `null` while the fleet it never saw sits behind it.
fn page_size(limit: Option<u32>) -> Result<usize, ApiFailure> {
    match limit {
        Some(0) => Err(ApiFailure::BadRequest(
            "?limit=0 asks for a page with no devices on it, which can carry no \
             cursor to the next one; ask for at least one"
                .to_owned(),
        )),
        Some(limit) => Ok(limit.min(MAX_LIMIT) as usize),
        None => Ok(DEFAULT_LIMIT as usize),
    }
}

/// Split a comma-separated filter into its entries, dropping blanks.
///
/// Blank entries are dropped rather than refused so a trailing comma, or the
/// empty string a form submits for an untouched field, reads as "this filter was
/// not given" — which is what the caller meant, and what an absent parameter
/// already means.
fn csv(raw: &Option<String>) -> Vec<&str> {
    raw.iter()
        .flat_map(|list| list.split(','))
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{RecordingState, Timestamp};

    const AT: &str = "2026-08-17T00:00:00.000Z";

    fn reading(field: &str, value: ReadingValue) -> Reading {
        Reading {
            device: "atrium-101".to_owned(),
            field: field.to_owned(),
            value,
            at: Timestamp(AT.to_owned()),
        }
    }

    fn query(predicates: &str) -> FleetQuery {
        FleetQuery {
            predicates: Some(predicates.to_owned()),
            ..FleetQuery::default()
        }
    }

    /// The comparison `where` is built on, reached through the parser a caller
    /// actually goes through: text in the URL, a decoded value in the store.
    #[test]
    fn a_predicate_reads_its_value_in_the_shape_the_device_answered_in() {
        let latest = vec![
            reading(
                "RUNNING_STATE",
                ReadingValue::State(RecordingState::Stopped),
            ),
            reading("HTTP_PORT", ReadingValue::Port(8080)),
            reading("DHCP_MODE", ReadingValue::Flag(true)),
            reading("FIRMWARE", ReadingValue::Version("2.11".to_owned())),
        ];

        for spelling in [
            "RUNNING_STATE:stopped",
            "running-state:STOPPED",
            "HTTP_PORT:8080",
            "DHCP_MODE:true",
            "DHCP_MODE:on",
            "FIRMWARE:2.11",
        ] {
            let predicates = predicates_of(&query(spelling)).expect("a well-formed predicate");
            assert!(
                predicates.iter().all(|p| p.holds(&latest)),
                "{spelling} should have matched"
            );
        }

        for spelling in ["RUNNING_STATE:started", "HTTP_PORT:9090", "DHCP_MODE:no"] {
            let predicates = predicates_of(&query(spelling)).expect("a well-formed predicate");
            assert!(
                !predicates.iter().all(|p| p.holds(&latest)),
                "{spelling} should not have matched"
            );
        }
    }

    /// The case that separates "not stopped" from "never answered", and the
    /// reason it is documented rather than papered over.
    #[test]
    fn a_device_that_never_reported_the_field_does_not_satisfy_a_predicate() {
        let latest = vec![reading(
            "FIRMWARE",
            ReadingValue::Version("2.11".to_owned()),
        )];
        let predicates = predicates_of(&query("RUNNING_STATE:stopped")).expect("well-formed");

        assert!(!predicates[0].holds(&latest));
    }

    #[test]
    fn every_predicate_has_to_hold() {
        let latest = vec![
            reading(
                "RUNNING_STATE",
                ReadingValue::State(RecordingState::Stopped),
            ),
            reading("FIRMWARE", ReadingValue::Version("2.11".to_owned())),
        ];

        let both =
            predicates_of(&query("RUNNING_STATE:stopped,FIRMWARE:2.11")).expect("well-formed");
        assert!(both.iter().all(|p| p.holds(&latest)));

        let one_wrong =
            predicates_of(&query("RUNNING_STATE:stopped,FIRMWARE:2.09")).expect("well-formed");
        assert!(!one_wrong.iter().all(|p| p.holds(&latest)));
    }

    #[test]
    fn a_predicate_without_a_colon_is_refused_rather_than_ignored() {
        // Silently dropping it would answer a wider question than the one asked
        // — every device rather than the stopped ones — which is the failure
        // mode a filter must not have.
        for malformed in ["RUNNING_STATE", ":stopped", " :stopped"] {
            let refused = predicates_of(&query(malformed));
            assert!(
                matches!(refused, Err(ApiFailure::BadRequest(_))),
                "{malformed} should have been refused"
            );
        }
    }

    #[test]
    fn field_selection_folds_case_and_dashes_and_an_empty_filter_means_every_field() {
        let selected = wanted_fields(&FleetQuery {
            fields: Some("running-state, firmware ,".to_owned()),
            ..FleetQuery::default()
        });
        assert_eq!(
            selected,
            Some(BTreeSet::from([
                "RUNNING_STATE".to_owned(),
                "FIRMWARE".to_owned()
            ]))
        );

        // Absent and blank both mean "do not narrow", which is not the same as
        // narrowing to nothing.
        assert_eq!(wanted_fields(&FleetQuery::default()), None);
        assert_eq!(
            wanted_fields(&FleetQuery {
                fields: Some(" , ".to_owned()),
                ..FleetQuery::default()
            }),
            None
        );
    }

    #[test]
    fn projection_keeps_the_requested_columns_in_the_stores_order() {
        let latest = vec![
            reading("FIRMWARE", ReadingValue::Version("2.11".to_owned())),
            reading(
                "RUNNING_STATE",
                ReadingValue::State(RecordingState::Stopped),
            ),
            reading("TIMEZONE", ReadingValue::Text("UTC".to_owned())),
        ];
        let fields = BTreeSet::from(["RUNNING_STATE".to_owned(), "TIMEZONE".to_owned()]);

        let kept = project(latest.clone(), Some(&fields));
        assert_eq!(
            kept.iter().map(|r| r.field.as_str()).collect::<Vec<_>>(),
            ["RUNNING_STATE", "TIMEZONE"]
        );
        // No selection is every column, untouched.
        assert_eq!(project(latest.clone(), None), latest);
    }

    #[test]
    fn the_page_size_is_capped_defaulted_and_refuses_zero() {
        assert_eq!(page_size(None).unwrap(), DEFAULT_LIMIT as usize);
        assert_eq!(page_size(Some(50)).unwrap(), 50);
        assert_eq!(page_size(Some(u32::MAX)).unwrap(), MAX_LIMIT as usize);
        assert!(matches!(page_size(Some(0)), Err(ApiFailure::BadRequest(_))));
    }
}
