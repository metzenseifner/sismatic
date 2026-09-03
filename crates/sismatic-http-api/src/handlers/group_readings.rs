//! The group readings routes — the same three questions as
//! [`readings`](crate::handlers::readings), asked of a device group.
//!
//! ```text
//! GET /groups/{id}/fields                     every field, every member
//! GET /groups/{id}/fields/{field}             one field, every member
//! GET /groups/{id}/fields/{field}/history     one field over time, per member
//! ```
//!
//! Field names are normalized exactly as on the device routes (`running-state`,
//! `running_state` and `RUNNING_STATE` are one field), the history filters are
//! the same [`ReadingQuery`], and the store call under each is the same one the
//! device route makes — run once per member. That is deliberate: a group route
//! is not a new way to read a device, it is the same read fanned out and
//! answered as one object.
//!
//! The write side has its own `/v1/writings/groups` space, in
//! [`writings`](crate::handlers::writings). Both spaces resolve an id through
//! [`target`](crate::handlers::target), so a device id is refused identically
//! wherever it appears under either.
//!
//! # What these add, and why it needs three ports
//!
//! A group has no readings of its own; it has members, and a claim about what
//! they were supposed to be doing. Answering therefore takes three sources, and
//! each one contributes something the others cannot:
//!
//! - the [`DeviceCatalog`] says which devices the id addresses, in configured
//!   order — without it there is no list to fan out over;
//! - the [`ReadStore`] says what each member last reported;
//! - the [`GroupState`] says what the device group was last *told*, which is
//!   the piece that makes drift detectable at all.
//!
//! # Two comparisons, and why both
//!
//! [`SyncState`] compares each member against the expectation. It catches the
//! failure the expectation exists for: a device group that was told to record
//! and is not, including the case where *every* member failed and the fleet
//! therefore looks perfectly consistent.
//!
//! [`GroupFieldState::uniform`] compares the members against each other. It
//! needs no expectation, so it catches drift on fields nobody writes — one
//! recorder on last year's firmware, one in the wrong timezone — which no
//! expectation will ever see because none was ever recorded.
//!
//! Neither subsumes the other, so both are reported, and the client is not
//! asked to derive either from the member list.
//!
//! # An unknown group is a 404 here, unlike an unknown device
//!
//! The device readings routes answer an unknown id with an empty list, because
//! the store cannot tell "no such device" from "has not answered yet" and a
//! `404` would report an unreachable device as an unconfigured one.
//!
//! These routes have no such excuse. They cannot answer *at all* without asking
//! the catalog which devices the id addresses, and the catalog is the
//! configured set — so by the time a group could be answered for, its existence
//! has already been settled. Reporting `{"members": []}` for a typo would be
//! inventing an empty device group.
//!
//! The one thing that is *not* a 404 is a field no member has reported: the
//! group exists, the members are known, and "these five have all said nothing
//! about `TIMEZONE`" is a real answer that names the five. That is the opposite
//! of the device route's choice, and for the opposite reason — there, the same
//! response would be an empty shell carrying no information.

use actix_web::web;
// `ApiError` is named only by the `#[utoipa::path]` response attributes below.
use sismatic_api_types::{
    ApiError, DeviceId, FieldName, GroupExpectation, GroupFieldState, GroupFieldStateList,
    GroupHistory, MemberHistory, MemberState, Reading, ReadingQuery, SyncState,
};
use sismatic_store::ReadStore;
use sismatic_store::catalog::DeviceCatalog;
use sismatic_store::group::{GroupState, satisfies};

use crate::handlers::error::ApiFailure;
use crate::handlers::readings::{normalize_field, reject_conflicting_field, span_of, truncate};
use crate::handlers::target::{READINGS, group_members};

/// `GET /v1/readings/groups/{id}/fields` — every field any member has reported
/// or the group has been told about, with each member's latest value.
///
/// The field set is the *union* of the two: a field only some members have
/// reported still appears (with `null` for the silent ones, which is the point
/// — a member that stopped reporting is the finding), and so does a field the
/// group was told to set but no member has answered on yet, which is what a
/// write that reached nobody looks like.
#[utoipa::path(
    get,
    path = "/groups/{id}/fields",
    context_path = "/v1/readings",
    tag = "readings",
    params(("id" = String, Path, description = "Group id, as written in the devices file.")),
    responses(
        (status = 200, description = "Every field known for this group, ordered by \
             field name, each with what the group was told and what every member \
             reports.", body = GroupFieldStateList),
        (status = 404, description = "No group has this id. Unlike the device readings \
             routes' answer for an unknown device, this is a claim about \
             configuration — these routes cannot answer without the catalog.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn list_group_fields(
    catalog: web::Data<dyn DeviceCatalog>,
    store: web::Data<dyn ReadStore>,
    state: web::Data<dyn GroupState>,
    path: web::Path<String>,
) -> Result<web::Json<GroupFieldStateList>, ApiFailure> {
    let group = path.into_inner();
    let members = group_members(&**catalog, &group, READINGS, "fields").await?;

    // One store read per member rather than one per (member, field): the port
    // answers "everything known about this device" in a single call, and the
    // alternative would need a field catalog this crate has no way to obtain.
    let mut readings = Vec::with_capacity(members.len());
    for device in &members {
        readings.push(store.latest_all(device.clone()).await?);
    }
    let expectations = state.expected_all(group.clone()).await?;

    // The union of both sources, sorted and de-duplicated by `BTreeMap`'s key
    // order, which is the field ordering the response promises.
    let mut fields: std::collections::BTreeSet<FieldName> = expectations
        .iter()
        .map(|expectation| expectation.field.clone())
        .collect();
    for device in &readings {
        fields.extend(device.iter().map(|reading| reading.field.clone()));
    }

    let fields = fields
        .into_iter()
        .map(|field| {
            let expected = expectations
                .iter()
                .find(|expectation| expectation.field == field)
                .cloned();
            let observed: Vec<_> = members
                .iter()
                .zip(&readings)
                .map(|(device, latest)| {
                    (
                        device.clone(),
                        latest.iter().find(|r| r.field == field).cloned(),
                    )
                })
                .collect();
            assemble(group.clone(), field, expected, observed.into_iter())
        })
        .collect();

    Ok(web::Json(GroupFieldStateList { group, fields }))
}

/// `GET /v1/readings/groups/{id}/fields/{field}` — one field across the whole
/// group.
///
/// The route a dashboard polls to answer "is this device group recording?", and
/// the one place the two comparisons are both in view: `sync` against what was
/// asked, `uniform` against each other.
#[utoipa::path(
    get,
    path = "/groups/{id}/fields/{field}",
    context_path = "/v1/readings",
    tag = "readings",
    params(
        ("id" = String, Path, description = "Group id, as written in the devices file."),
        ("field" = String, Path,
         description = "Field name. Case-insensitive, and `-` is read as `_`, exactly \
             as on the device readings routes.",
         example = "RUNNING_STATE"),
    ),
    responses(
        (status = 200, description = "The field across the group. A member that has \
             never reported it carries a `null` reading rather than being omitted, \
             and a field no member has reported is still a 200 — the group exists \
             and the silence is the answer.", body = GroupFieldState),
        (status = 404, description = "No group has this id.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn read_group_field(
    catalog: web::Data<dyn DeviceCatalog>,
    store: web::Data<dyn ReadStore>,
    state: web::Data<dyn GroupState>,
    path: web::Path<(String, String)>,
) -> Result<web::Json<GroupFieldState>, ApiFailure> {
    let (group, field) = path.into_inner();
    let field = normalize_field(&field);
    let members = group_members(&**catalog, &group, READINGS, "fields").await?;

    let expected = state.expected(group.clone(), field.clone()).await?;

    let mut observed = Vec::with_capacity(members.len());
    for device in members {
        let reading = store.latest(device.clone(), field.clone()).await?;
        observed.push((device, reading));
    }

    Ok(web::Json(assemble(
        group,
        field,
        expected,
        observed.into_iter(),
    )))
}

/// `GET /v1/readings/groups/{id}/fields/{field}/history?start=&end=&limit=` —
/// one field over time, one series per member.
///
/// `limit` is **per member**, not per response: a caller asking for the last
/// hundred points of `RUNNING_STATE` in a five-member device group wants a
/// hundred points each, and a shared budget would return whichever member the
/// loop reached first. The response is therefore bounded by `limit × members`
/// rather than by `limit`, which is the honest cost of asking one question of
/// five devices.
#[utoipa::path(
    get,
    path = "/groups/{id}/fields/{field}/history",
    context_path = "/v1/readings",
    tag = "readings",
    params(
        ("id" = String, Path, description = "Group id, as written in the devices file."),
        ("field" = String, Path,
         description = "Field name, normalized as on the latest-value route.",
         example = "RUNNING_STATE"),
        // From `ReadingQuery`'s `IntoParams`, so this route documents the
        // struct the handler actually deserializes — as on the device route.
        ReadingQuery,
    ),
    responses(
        (status = 200, description = "One series per member, oldest first, each \
             truncated to its own most recent `limit` rows. A member with nothing in \
             the span carries an empty list rather than being omitted.",
         body = GroupHistory),
        (status = 400, description = "A `?field=` that contradicts the field in the \
             path, refused for the reason the device history route refuses it.",
         body = ApiError),
        (status = 404, description = "No group has this id.", body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn group_field_history(
    catalog: web::Data<dyn DeviceCatalog>,
    store: web::Data<dyn ReadStore>,
    state: web::Data<dyn GroupState>,
    path: web::Path<(String, String)>,
    query: web::Query<ReadingQuery>,
) -> Result<web::Json<GroupHistory>, ApiFailure> {
    let (group, field) = path.into_inner();
    let field = normalize_field(&field);
    let query = query.into_inner();
    reject_conflicting_field(&query, &field)?;

    let member_ids = group_members(&**catalog, &group, READINGS, "fields").await?;
    let span = span_of(&query);

    let mut members = Vec::with_capacity(member_ids.len());
    for device in member_ids {
        let mut readings = store
            .between(device.clone(), field.clone(), span.clone())
            .await?;
        truncate(&mut readings, query.limit);
        members.push(MemberHistory { device, readings });
    }

    // The expectation describes *now*, not the window — see `GroupHistory`.
    // Read after the series rather than before, so a slow store cannot make the
    // two describe wildly different instants in the one direction that would
    // mislead: an expectation newer than the readings is the honest ordering
    // for "what it should be, and what it has been".
    let expected = state.expected(group.clone(), field.clone()).await?;

    Ok(web::Json(GroupHistory {
        group,
        field,
        expected,
        members,
    }))
}

/// Fold one field's members and expectation into the answer, computing both
/// comparisons in one pass.
///
/// A single function rather than one per route, because the two latest-value
/// routes differ only in how they *obtained* the readings — and a `sync` that
/// meant something different on the index than on the detail view would be a
/// contract that cannot be read.
fn assemble(
    group: String,
    field: FieldName,
    expected: Option<GroupExpectation>,
    observed: impl Iterator<Item = (DeviceId, Option<Reading>)>,
) -> GroupFieldState {
    let members: Vec<MemberState> = observed
        .map(|(device, reading)| {
            let sync = match (&expected, &reading) {
                // Nothing was asked of this device group, or this member has
                // said nothing: `Unknown` rather than `InSync`, because
                // agreement with nothing is not agreement.
                (None, _) | (_, None) => SyncState::Unknown,
                (Some(expected), Some(reading)) => {
                    if satisfies(&expected.value, &reading.value) {
                        SyncState::InSync
                    } else {
                        SyncState::Drifted
                    }
                }
            };
            MemberState {
                device,
                reading,
                sync,
            }
        })
        .collect();

    // Every member that has reported holds the same value. Vacuously true for
    // fewer than two reporters, which is a fact about the answer rather than an
    // absence of one — hence a `bool` where `sync` is a tri-state.
    let mut reported = members.iter().filter_map(|m| m.reading.as_ref());
    let uniform = match reported.next() {
        None => true,
        Some(first) => reported.all(|r| r.value == first.value),
    };

    // Drift wins over agreement: a device group where four members started and
    // one did not needs attention, not one that is four-fifths fine.
    let sync = if members.iter().any(|m| m.sync == SyncState::Drifted) {
        SyncState::Drifted
    } else if members.iter().any(|m| m.sync == SyncState::InSync) {
        SyncState::InSync
    } else {
        SyncState::Unknown
    };

    GroupFieldState {
        group,
        field,
        expected,
        sync,
        uniform,
        members,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{ReadingValue, RecordingState, Timestamp};

    const AT: &str = "2026-08-17T00:00:00.000Z";

    fn reading(device: &str, value: ReadingValue) -> (DeviceId, Option<Reading>) {
        (
            device.to_owned(),
            Some(Reading {
                device: device.to_owned(),
                field: "RUNNING_STATE".to_owned(),
                value,
                at: Timestamp(AT.to_owned()),
            }),
        )
    }

    fn silent(device: &str) -> (DeviceId, Option<Reading>) {
        (device.to_owned(), None)
    }

    fn started() -> GroupExpectation {
        GroupExpectation {
            field: "RUNNING_STATE".to_owned(),
            value: ReadingValue::State(RecordingState::Started),
            since: Timestamp(AT.to_owned()),
        }
    }

    fn state(
        expected: Option<GroupExpectation>,
        observed: Vec<(DeviceId, Option<Reading>)>,
    ) -> GroupFieldState {
        assemble(
            "atrium-room".to_owned(),
            "RUNNING_STATE".to_owned(),
            expected,
            observed.into_iter(),
        )
    }

    fn state_value(value: RecordingState) -> ReadingValue {
        ReadingValue::State(value)
    }

    #[test]
    fn a_device_group_that_did_what_it_was_told_is_in_sync_and_uniform() {
        let answer = state(
            Some(started()),
            vec![
                reading("atrium", state_value(RecordingState::Started)),
                reading("annex", state_value(RecordingState::Started)),
            ],
        );

        assert_eq!(answer.sync, SyncState::InSync);
        assert!(answer.uniform);
        assert!(answer.members.iter().all(|m| m.sync == SyncState::InSync));
    }

    /// The finding the routes exist for: four started, one did not. The
    /// roll-up reports the device group as drifted rather than mostly fine,
    /// and the member list says which one.
    #[test]
    fn one_member_that_did_not_start_drifts_the_whole_device_group() {
        let answer = state(
            Some(started()),
            vec![
                reading("atrium", state_value(RecordingState::Started)),
                reading("annex", state_value(RecordingState::Stopped)),
            ],
        );

        assert_eq!(answer.sync, SyncState::Drifted);
        assert!(!answer.uniform);
        assert_eq!(answer.members[0].sync, SyncState::InSync);
        assert_eq!(answer.members[1].sync, SyncState::Drifted);
    }

    /// The case member-versus-member comparison cannot see, and the whole
    /// reason the expectation is stored: every recorder agrees, and every one
    /// of them is wrong.
    #[test]
    fn a_device_group_that_uniformly_ignored_the_request_is_uniform_and_drifted() {
        let answer = state(
            Some(started()),
            vec![
                reading("atrium", state_value(RecordingState::Stopped)),
                reading("annex", state_value(RecordingState::Stopped)),
            ],
        );

        assert!(answer.uniform, "the members do agree with each other");
        assert_eq!(
            answer.sync,
            SyncState::Drifted,
            "...and none of them agrees with what was asked"
        );
    }

    /// The case the expectation cannot see, and the whole reason `uniform` is
    /// reported beside it: nothing was ever written to this field.
    #[test]
    fn members_that_disagree_about_an_unwritten_field_are_not_uniform() {
        let answer = state(
            None,
            vec![
                reading("atrium", ReadingValue::Version("2.11".into())),
                reading("annex", ReadingValue::Version("2.09".into())),
            ],
        );

        assert!(!answer.uniform);
        assert_eq!(
            answer.sync,
            SyncState::Unknown,
            "nothing was asked, so there is nothing to agree with"
        );
    }

    #[test]
    fn a_member_that_has_never_reported_is_listed_and_unknown() {
        let answer = state(
            Some(started()),
            vec![
                reading("atrium", state_value(RecordingState::Started)),
                silent("annex"),
            ],
        );

        assert_eq!(answer.members[1].device, "annex");
        assert_eq!(answer.members[1].reading, None);
        assert_eq!(answer.members[1].sync, SyncState::Unknown);
        // One member agrees and none disagrees, so the device group reads in
        // sync — a silent member is missing evidence, not contrary evidence.
        assert_eq!(answer.sync, SyncState::InSync);
        assert!(answer.uniform, "one reporter agrees with itself");
    }

    #[test]
    fn a_group_nothing_was_written_to_reports_unknown_rather_than_in_sync() {
        let answer = state(
            None,
            vec![reading("atrium", state_value(RecordingState::Started))],
        );

        assert_eq!(answer.sync, SyncState::Unknown);
        assert_eq!(answer.members[0].sync, SyncState::Unknown);
        assert_eq!(answer.expected, None);
    }

    /// A setting write carries the caller's text and the device answers a
    /// decoded value; the comparison reconciles them, so a correctly applied
    /// port setting does not read as permanent drift.
    #[test]
    fn a_text_expectation_agrees_with_the_devices_decoded_value() {
        let expectation = GroupExpectation {
            field: "HTTP_PORT".to_owned(),
            value: ReadingValue::Text("8080".to_owned()),
            since: Timestamp(AT.to_owned()),
        };
        let answer = state(
            Some(expectation),
            vec![reading("atrium", ReadingValue::Port(8080))],
        );

        assert_eq!(answer.sync, SyncState::InSync);
    }

    #[test]
    fn an_empty_group_is_uniform_and_unknown() {
        let answer = state(Some(started()), Vec::new());

        assert!(answer.members.is_empty());
        assert!(answer.uniform, "no member disagrees with no member");
        assert_eq!(answer.sync, SyncState::Unknown);
    }
}
