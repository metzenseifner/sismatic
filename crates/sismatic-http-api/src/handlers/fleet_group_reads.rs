//! The group index route — every configured device group's state, filtered and
//! paged.
//!
//! ```text
//! GET /groups?fields=&groups=&where=&sync=&limit=&after=
//! ```
//!
//! What [`fleet_reads`](crate::handlers::fleet_reads) is to
//! [`reads`](crate::handlers::reads), this is to
//! [`group_reads`](crate::handlers::group_reads): the same answer, over every
//! configured subject at once instead of one named in a path. A row here is
//! exactly the body `GET /v1/reads/groups/{id}/fields` returns, assembled by the
//! same function, so the index cannot come to mean something different from the
//! detail view it summarizes.
//!
//! # Why this is the more useful of the two indexes
//!
//! The device index answers "what is every recorder doing". This answers "is
//! every *room* doing what it was told", which is a question the per-device
//! routes cannot express at all — not because they lack a filter, but because
//! the comparison needs an expectation, and an expectation is recorded against
//! a group. `?sync=drifted` is therefore the route's reason for existing: one
//! request that names every device group not doing what it was asked, including
//! the case the device index is blindest to, where every member of a group
//! failed identically and the fleet looks perfectly consistent.
//!
//! # What a predicate means when the subject has members
//!
//! On the device index a predicate holds when *some* read satisfies it — a
//! device has one value per field, so "some" and "every" coincide. A group has
//! one value per field *per member*, and they need not agree, so the two
//! quantifiers come apart and the choice is load-bearing.
//!
//! [`Predicate::matches`] is quantified with **every**: a group satisfies
//! `RUNNING_STATE:stopped` only if every member that has reported
//! `RUNNING_STATE` reports a stopped one. Quantified with "some", a group where
//! four members are stopped and one is recording would answer to
//! `?where=RUNNING_STATE:stopped` — and that group is precisely the one a
//! filter must not hide, because the disagreement is the finding. Silence is
//! not disagreement, though, so a member that has never reported the field
//! neither satisfies nor vetoes: it is missing evidence, which is the same
//! reading [`GroupSyncState`] takes of it.
//!
//! A group no member of which has reported the field cannot satisfy the
//! predicate and is excluded, exactly as a silent device is on the device index.
//!
//! # Selection before projection, twice over
//!
//! `?fields=` picks columns and `?where=`/`?sync=` pick rows, and the row
//! filters run against the group's *whole* field set before any column is
//! dropped — the argument [`fleet_reads`] gives, and it applies once more here
//! because `?sync=` rolls up across fields. `?fields=RUNNING_STATE&sync=drifted`
//! is "the recording state of every device group that has drifted *in any
//! field*", which is the reading that makes the pair useful; rolled up after
//! projection it would silently become "…that has drifted in `RUNNING_STATE`",
//! a different and much narrower question that `?where=` can already ask.
//!
//! # The cost, which is not the device index's
//!
//! A device row is one [`ReadStore`] call. A group row is two per member — the
//! store for what was reported, the [`WriteLog`] for what was accepted — plus
//! one [`GroupState`] call, because a group has no reads of its own. A page of
//! fifty three-member groups is therefore about three hundred and fifty port
//! calls, not fifty.
//!
//! The page size is nevertheless capped by the same [`page_size`] the device
//! index uses rather than a lower one of its own, because the configured group
//! count is bounded by the device count in practice: a group is a partition of
//! the fleet, and there are fewer rooms than recorders. If the per-member log
//! read ever becomes the thing that hurts, the fix is to make
//! `desired_recording_state` opt-in rather than to cap the page differently —
//! the ceiling is not what is expensive here, the fan-out is.

use std::collections::BTreeSet;

use actix_web::web;
// `ApiError` is named only by the `#[utoipa::path]` response attributes below.
use sismatic_api_types::{
    ApiError, FieldName, FleetGroupQuery, FleetGroupReads, GroupFieldState, GroupFieldStateList,
    GroupId, GroupSyncState,
};
use sismatic_store::ReadStore;
use sismatic_store::catalog::DeviceCatalog;
use sismatic_store::group::GroupState;
use sismatic_store::outbox::WriteLog;

use crate::handlers::error::ApiFailure;
use crate::handlers::fleet_reads::{Predicate, csv, page_size, predicates_of, wanted_fields};
use crate::handlers::group_reads::group_state_of;
use crate::handlers::reads::normalize_field;

/// `GET /v1/reads/groups?fields=&groups=&where=&sync=&limit=&after=` — every
/// configured device group's state, one row per group, ordered by id.
///
/// Every filter is optional: omit them all for every group with every field it
/// knows about. A configured group no member of which has ever answered is a row
/// with an empty `fields` rather than an absent one, for the reason the device
/// index lists a silent device.
#[utoipa::path(
    get,
    path = "/groups",
    context_path = "/v1/reads",
    tag = "reads",
    params(
        // From `FleetGroupQuery`'s `IntoParams`, so this route documents the
        // struct the handler actually deserializes.
        FleetGroupQuery,
    ),
    responses(
        (status = 200, description = "A page of the configured device groups' state, \
             one row per group, ordered by id. Each row is the body \
             `GET /v1/reads/groups/{id}/fields` returns, including its rolled-up \
             `sync` — which describes the whole group, so with `?fields=` a row can \
             read `drifted` while every field it carries reads `unknown` — and its \
             `desired_recording_state`, which is what tells a `?sync=unknown` page \
             the resting device groups from the ones that were told to record and \
             have answered nothing. `next` carries the `after` value for the \
             following page, or `null` on the last one.", body = FleetGroupReads),
        (status = 400, description = "A malformed `?where=` predicate, an unrecognized \
             `?sync=` value, or `?limit=0`. The body says which.", body = ApiError),
        (status = 404, description = "A `?groups=` id that names no configured device \
             group. Unlike the device reads routes' answer for an unknown id, this is \
             a claim about configuration — the catalog is what this route enumerates.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn list_fleet_groups(
    catalog: web::Data<dyn DeviceCatalog>,
    store: web::Data<dyn ReadStore>,
    state: web::Data<dyn GroupState>,
    log: web::Data<dyn WriteLog>,
    query: web::Query<FleetGroupQuery>,
) -> Result<web::Json<FleetGroupReads>, ApiFailure> {
    let query = query.into_inner();

    let fields = wanted_fields(&query.fields);
    let predicates = predicates_of(&query.predicates)?;
    let sync = wanted_sync(&query.sync)?;
    let limit = page_size(query.limit)?;
    let candidates = candidates(&**catalog, &query).await?;

    let mut groups: Vec<GroupFieldStateList> = Vec::new();
    let mut next = None;

    for group in candidates {
        // The whole state, because both row filters may read a field
        // `?fields=` does not ask for, and `?sync=` rolls up across all of
        // them. Projection happens after the row survives.
        let row = group_state_of(&**catalog, &**store, &**state, &**log, group).await?;

        if !predicates.iter().all(|p| holds(p, &row.fields)) {
            continue;
        }
        // The verdict the row itself carries, computed over the whole field set
        // by the assembly both group routes share — so `?sync=` cannot select
        // on one rule while the response reports another.
        if sync.is_some_and(|wanted| row.sync != wanted) {
            continue;
        }

        // One match past a full page is what tells "there is more" from "that
        // was the last one" — see the device index for why the scan stops here
        // rather than filtering everything and slicing.
        if groups.len() == limit {
            next = groups.last().map(|row| row.group.clone());
            break;
        }

        groups.push(GroupFieldStateList {
            group: row.group,
            // Deliberately the whole-group verdict rather than one recomputed
            // over the projected columns: see `GroupFieldStateList::sync`, and
            // the module note on selection before projection.
            sync: row.sync,
            // Unaffected by `?fields=` for the same reason, and doubly so: it
            // is not a field of the group at all, so there is no column for a
            // projection to drop.
            desired_recording_state: row.desired_recording_state,
            fields: project(row.fields, fields.as_ref()),
        });
    }

    Ok(web::Json(FleetGroupReads { groups, next }))
}

/// The device groups this request addresses, in page order.
///
/// Narrows the catalog's own ordered list by set membership, for the reason the
/// device index does: the id cursor is only correct over an order that is total
/// and independent of the filters.
async fn candidates(
    catalog: &dyn DeviceCatalog,
    query: &FleetGroupQuery,
) -> Result<Vec<GroupId>, ApiFailure> {
    let configured = catalog.groups().await;

    // `None` is "every group", which is not the same set as an empty one.
    let mut keep: Option<BTreeSet<GroupId>> = None;

    let named = csv(&query.groups);
    if !named.is_empty() {
        let mut requested = BTreeSet::new();
        for id in named {
            if catalog.group(id).await.is_none() {
                return Err(unknown_group(catalog, id).await);
            }
            requested.insert(id.to_owned());
        }
        keep = Some(requested);
    }

    let after = query.after.as_deref();
    Ok(configured
        .into_iter()
        .map(|group| group.id)
        .filter(|id| keep.as_ref().is_none_or(|keep| keep.contains(id)))
        // Exclusive, so `after=<the previous page's next>` resumes rather than
        // repeating its last row.
        .filter(|id| after.is_none_or(|after| id.as_str() > after))
        .collect())
}

/// The `404` for an id in `?groups=` that names no group.
///
/// The mirror of the device index's message: a *device* id here is not a typo
/// but a caller on the wrong index, and the fix is the other route's
/// `?devices=` parameter rather than a different spelling of this one.
async fn unknown_group(catalog: &dyn DeviceCatalog, id: &str) -> ApiFailure {
    if catalog.device(id).await.is_some() {
        return ApiFailure::NotFound(format!(
            "'{id}' is a device, not a device group; \
             ask for it with /v1/reads/devices?devices={id}"
        ));
    }
    ApiFailure::NotFound(format!("no device group '{id}' is configured"))
}

/// Whether a group's state satisfies one `?where=` predicate.
///
/// Quantified with *every reporting member* rather than *some* — see the module
/// docs for why that difference is the whole point of the filter on this route.
fn holds(predicate: &Predicate, fields: &[GroupFieldState]) -> bool {
    let Some(state) = fields.iter().find(|f| f.field == predicate.field) else {
        return false;
    };

    let mut reported = state
        .members
        .iter()
        .filter_map(|m| m.read.as_ref())
        .peekable();
    // A group nobody has reported this field for holds nothing that could
    // satisfy the predicate, so "every" is not allowed to be vacuously true.
    if reported.peek().is_none() {
        return false;
    }
    reported.all(|read| predicate.matches(read))
}

/// The `?sync=` verdict asked for, or `None` for "do not narrow".
///
/// Parsed here rather than by deriving `Deserialize` on the parameter, so an
/// unrecognized value is an [`ApiFailure`] naming the three that are accepted —
/// a `web::Query` rejection would be actix's own plain-text 400, outside the
/// error envelope every other refusal on this route uses.
fn wanted_sync(raw: &Option<String>) -> Result<Option<GroupSyncState>, ApiFailure> {
    let Some(raw) = raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    // Folded exactly as a field name is, so `in-sync` and `IN_SYNC` are the one
    // value a caller means by either.
    match normalize_field(raw).as_str() {
        "IN_SYNC" => Ok(Some(GroupSyncState::InSync)),
        "DRIFTED" => Ok(Some(GroupSyncState::Drifted)),
        "UNKNOWN" => Ok(Some(GroupSyncState::Unknown)),
        _ => Err(ApiFailure::BadRequest(format!(
            "'{raw}' is not a sync state; ask for one of \
             'in_sync', 'drifted' or 'unknown'"
        ))),
    }
}

/// Keep only the requested columns, preserving the assembled field ordering.
fn project(
    fields: Vec<GroupFieldState>,
    wanted: Option<&BTreeSet<FieldName>>,
) -> Vec<GroupFieldState> {
    match wanted {
        None => fields,
        Some(wanted) => fields
            .into_iter()
            .filter(|state| wanted.contains(&state.field))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{
        GroupExpectation, MemberState, Read, ReadValue, RecordingState, Timestamp,
    };

    const AT: &str = "2026-08-17T00:00:00.000Z";
    const GROUP: &str = "atrium-room";

    fn member(device: &str, field: &str, value: Option<ReadValue>) -> MemberState {
        MemberState {
            device: device.to_owned(),
            read: value.map(|value| Read {
                device: device.to_owned(),
                field: field.to_owned(),
                value,
                at: Timestamp(AT.to_owned()),
            }),
            // Not what any assertion below is about; the roll-up tests state
            // `GroupFieldState::sync` directly.
            sync: GroupSyncState::Unknown,
        }
    }

    fn field(name: &str, sync: GroupSyncState, members: Vec<MemberState>) -> GroupFieldState {
        GroupFieldState {
            group: GROUP.to_owned(),
            field: name.to_owned(),
            expected: Some(GroupExpectation {
                field: name.to_owned(),
                value: ReadValue::State(RecordingState::Started),
                since: Timestamp(AT.to_owned()),
            }),
            sync,
            uniform: true,
            members,
        }
    }

    fn predicate(raw: &str) -> Predicate {
        predicates_of(&Some(raw.to_owned()))
            .expect("a well-formed predicate")
            .pop()
            .expect("one predicate")
    }

    fn state(value: RecordingState) -> Option<ReadValue> {
        Some(ReadValue::State(value))
    }

    /// The quantifier that separates this route's `where` from the device
    /// index's, and the case it exists to keep visible.
    #[test]
    fn a_predicate_needs_every_reporting_member_to_agree() {
        let all_stopped = vec![field(
            "RUNNING_STATE",
            GroupSyncState::Unknown,
            vec![
                member("atrium", "RUNNING_STATE", state(RecordingState::Stopped)),
                member("annex", "RUNNING_STATE", state(RecordingState::Stopped)),
            ],
        )];
        assert!(holds(&predicate("RUNNING_STATE:stopped"), &all_stopped));

        // Four stopped and one recording is not a stopped device group. A
        // filter that said otherwise would hide exactly the group worth seeing.
        let one_disagrees = vec![field(
            "RUNNING_STATE",
            GroupSyncState::Unknown,
            vec![
                member("atrium", "RUNNING_STATE", state(RecordingState::Stopped)),
                member("annex", "RUNNING_STATE", state(RecordingState::Started)),
            ],
        )];
        assert!(!holds(&predicate("RUNNING_STATE:stopped"), &one_disagrees));
    }

    /// Silence is missing evidence rather than contrary evidence — the same
    /// reading `GroupSyncState` takes of a member that has never reported.
    #[test]
    fn a_silent_member_neither_satisfies_nor_vetoes_a_predicate() {
        let one_silent = vec![field(
            "RUNNING_STATE",
            GroupSyncState::Unknown,
            vec![
                member("atrium", "RUNNING_STATE", state(RecordingState::Stopped)),
                member("annex", "RUNNING_STATE", None),
            ],
        )];
        assert!(holds(&predicate("RUNNING_STATE:stopped"), &one_silent));

        // ...but a group nobody has reported for cannot satisfy it, so "every"
        // is not vacuously true.
        let all_silent = vec![field(
            "RUNNING_STATE",
            GroupSyncState::Unknown,
            vec![member("atrium", "RUNNING_STATE", None)],
        )];
        assert!(!holds(&predicate("RUNNING_STATE:stopped"), &all_silent));
    }

    #[test]
    fn a_predicate_on_a_field_the_group_knows_nothing_about_excludes_it() {
        let other_field = vec![field(
            "FIRMWARE",
            GroupSyncState::Unknown,
            vec![member(
                "atrium",
                "FIRMWARE",
                Some(ReadValue::Version("2.11".to_owned())),
            )],
        )];

        assert!(!holds(&predicate("RUNNING_STATE:stopped"), &other_field));
    }

    #[test]
    fn the_sync_filter_folds_case_and_dashes_and_refuses_anything_else() {
        for spelling in ["drifted", "DRIFTED", " drifted "] {
            assert_eq!(
                wanted_sync(&Some(spelling.to_owned())).unwrap(),
                Some(GroupSyncState::Drifted),
                "{spelling} should have been read as drifted"
            );
        }
        for spelling in ["in_sync", "in-sync", "IN-SYNC"] {
            assert_eq!(
                wanted_sync(&Some(spelling.to_owned())).unwrap(),
                Some(GroupSyncState::InSync),
                "{spelling} should have been read as in_sync"
            );
        }
        assert_eq!(
            wanted_sync(&Some("unknown".to_owned())).unwrap(),
            Some(GroupSyncState::Unknown)
        );

        // Absent and blank both mean "do not narrow" — the same reading every
        // other filter takes of an untouched form field.
        assert_eq!(wanted_sync(&None).unwrap(), None);
        assert_eq!(wanted_sync(&Some("  ".to_owned())).unwrap(), None);

        // Refused rather than ignored, because ignoring it would answer a wider
        // question than the one asked.
        assert!(matches!(
            wanted_sync(&Some("sideways".to_owned())),
            Err(ApiFailure::BadRequest(_))
        ));
    }

    #[test]
    fn projection_keeps_the_requested_columns_in_the_assembled_order() {
        let fields = vec![
            field("FIRMWARE", GroupSyncState::Unknown, Vec::new()),
            field("RUNNING_STATE", GroupSyncState::InSync, Vec::new()),
            field("TIMEZONE", GroupSyncState::Unknown, Vec::new()),
        ];
        let wanted = BTreeSet::from(["RUNNING_STATE".to_owned(), "TIMEZONE".to_owned()]);

        let kept = project(fields.clone(), Some(&wanted));
        assert_eq!(
            kept.iter().map(|f| f.field.as_str()).collect::<Vec<_>>(),
            ["RUNNING_STATE", "TIMEZONE"]
        );
        // No selection is every column, untouched.
        assert_eq!(project(fields.clone(), None), fields);
    }
}
