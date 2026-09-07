//! `/v1/reads/groups` — the group index, as a client meets it.
//!
//! The group counterpart of [`fleet`](crate::fleet), and the suite carries the
//! two things that route does which the device index cannot: a `?where=`
//! quantified over members, and `?sync=`, which is the whole reason a fleet-wide
//! view of groups is worth having.
//!
//! Every expectation below is created the way a client creates one — by POSTing
//! to a group write route — for the reason [`groups`](crate::groups) gives: the
//! rule that an expectation exists exactly when a group request was admitted
//! lives inside the outbox's admission critical section, and a stub that
//! recorded expectations on demand would let these tests assert drift detection
//! over a state the server can never actually be in.
//!
//! The catalog is built here rather than taken from the harness, because that
//! helper builds exactly one group and every question this route answers is
//! about telling several apart.

use std::net::TcpListener;
use std::sync::Arc;

use sismatic_api_types::{
    Barrier, ConnectionStatus, DeviceSummary, GroupSummary, Read, ReadValue, RecordingState,
};
use sismatic_store::WriteStore;
use sismatic_store_memory::{MemoryCatalog, MemoryStore};

use crate::{SCOPE, get, read_at};

/// The three device groups, in id order — which is the order the index promises
/// and the order its cursor indexes.
const ATRIUM_ROOM: &str = "atrium-room";
const BEACON_HALL: &str = "beacon-hall";
const WEST_WING: &str = "west-wing";

/// `atrium-room`'s two members, written `[atrium, annex]` — deliberately not
/// alphabetical, so a member list served in id order rather than configured
/// order fails visibly.
const ATRIUM: &str = "atrium";
const ANNEX: &str = "annex";
/// `beacon-hall`'s only member.
const BEACON: &str = "beacon";
/// `west-wing`'s only member, which has never reported anything. It is what
/// makes the empty row testable without an empty group.
const DORMANT: &str = "dormant";

const AT: &str = "2026-07-23T14:00:00Z";

/// The fleet: four devices in three groups, no group containing all of them.
fn catalog() -> MemoryCatalog {
    let device = |id: &str| DeviceSummary {
        id: id.to_owned(),
        host: "10.0.0.7".to_owned(),
        port: 22023,
        eager: false,
        status: ConnectionStatus::Unknown,
    };
    let group = |id: &str, members: &[&str]| GroupSummary {
        id: id.to_owned(),
        members: members.iter().map(|m| (*m).to_owned()).collect(),
        barrier_timeout_secs: 15,
        barrier: Barrier::FailBatch,
    };

    MemoryCatalog::new(
        vec![
            device(ATRIUM),
            device(ANNEX),
            device(BEACON),
            device(DORMANT),
        ],
        vec![
            group(ATRIUM_ROOM, &[ATRIUM, ANNEX]),
            group(BEACON_HALL, &[BEACON]),
            group(WEST_WING, &[DORMANT]),
        ],
    )
}

/// A `RUNNING_STATE` read for `device`.
fn state(device: &str, value: RecordingState) -> Read {
    read_at(device, "RUNNING_STATE", ReadValue::State(value), AT)
}

/// A `FIRMWARE` read for `device`.
fn firmware(device: &str, value: &str) -> Read {
    read_at(device, "FIRMWARE", ReadValue::Version(value.into()), AT)
}

/// The reads every test runs against unless it says otherwise.
///
/// `atrium-room`'s members disagree about `RUNNING_STATE` and agree about
/// `FIRMWARE`; `beacon-hall`'s one member is the other way round against them
/// both; `west-wing`'s has said nothing at all. That is enough to separate every
/// filter on the route.
fn seeded() -> Vec<Read> {
    vec![
        state(ATRIUM, RecordingState::Started),
        firmware(ATRIUM, "2.11"),
        state(ANNEX, RecordingState::Stopped),
        firmware(ANNEX, "2.11"),
        state(BEACON, RecordingState::Started),
        firmware(BEACON, "2.09"),
    ]
}

/// Start the application over a store pre-loaded with `reads` and the catalog
/// above; return its base URL.
async fn spawn(reads: impl IntoIterator<Item = Read>) -> String {
    let store = MemoryStore::default();
    for r in reads {
        store.upsert_latest(r).await.expect("seeding the store");
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    crate::harness::serve_with(listener, Arc::new(store), catalog());

    format!("http://127.0.0.1:{port}")
}

/// Ask a device group to start recording, the way a client does — through the
/// write scope, which is where the expectation this route reads is filed.
async fn start(address: &str, group: &str) {
    let status = reqwest::Client::new()
        .post(format!(
            "{address}/v1/writes/groups/{group}/recording/start"
        ))
        .send()
        .await
        .expect("starting the device group")
        .status()
        .as_u16();
    assert_eq!(status, 202, "{group} should have been asked to start");
}

/// The group ids of a page, in the order they were served.
fn ids(body: &serde_json::Value) -> Vec<String> {
    body["groups"]
        .as_array()
        .expect("groups is an array")
        .iter()
        .map(|g| g["group"].as_str().expect("group is a string").to_owned())
        .collect()
}

/// The field names one row carries, in the order they were served.
fn fields(row: &serde_json::Value) -> Vec<String> {
    row["fields"]
        .as_array()
        .expect("fields is an array")
        .iter()
        .map(|f| f["field"].as_str().expect("field is a string").to_owned())
        .collect()
}

/// `GET SCOPE/groups?query`, asserting `200`.
async fn index(address: &str, query: &str) -> serde_json::Value {
    let (status, body) = get(format!("{address}{SCOPE}/groups?{query}")).await;
    assert_eq!(status, 200, "?{query} answered {status}: {body}");
    body
}

#[tokio::test]
async fn the_whole_index_comes_back_one_row_per_group_ordered_by_id() {
    let address = spawn(seeded()).await;

    let (status, body) = get(format!("{address}{SCOPE}/groups")).await;

    assert_eq!(status, 200);
    assert_eq!(ids(&body), [ATRIUM_ROOM, BEACON_HALL, WEST_WING]);
    // The last page of a walk carries no cursor.
    assert_eq!(body["next"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_group_no_member_of_which_has_answered_is_a_row_with_no_fields() {
    // The row the index exists to show, and the group analogue of the device
    // index's empty `latest`: sourced from the store, `west-wing` would simply
    // be missing, and "nobody in that wing has reported anything" is the
    // finding a fleet page is read for.
    let address = spawn(seeded()).await;

    let body = index(&address, "").await;

    let west = &body["groups"][2];
    assert_eq!(west["group"], WEST_WING);
    assert_eq!(west["fields"], serde_json::json!([]));
}

#[tokio::test]
async fn each_row_is_the_body_the_per_group_route_returns() {
    // The property that makes the index and the detail view one answer rather
    // than two traversals that can disagree about which fields exist or what
    // `sync` means. Asserted whole, not field by field.
    let address = spawn(seeded()).await;
    start(&address, ATRIUM_ROOM).await;

    let from_index = index(&address, &format!("groups={ATRIUM_ROOM}")).await;
    let (status, from_detail) = get(format!("{address}{SCOPE}/groups/{ATRIUM_ROOM}/fields")).await;

    assert_eq!(status, 200);
    assert_eq!(from_index["groups"][0], from_detail);
}

#[tokio::test]
async fn fields_selects_which_columns_each_row_carries() {
    let address = spawn(seeded()).await;

    let body = index(&address, &format!("groups={ATRIUM_ROOM}&fields=firmware")).await;

    // Normalized exactly as a path segment is, and `RUNNING_STATE` is gone.
    assert_eq!(fields(&body["groups"][0]), ["FIRMWARE"]);
}

#[tokio::test]
async fn where_needs_every_reporting_member_to_agree() {
    // The quantifier that separates this route's `where` from the device
    // index's. `atrium` is started and `annex` is stopped, so `atrium-room` is
    // not a started device group — and a filter that said it was would hide
    // precisely the group worth looking at.
    let address = spawn(seeded()).await;

    let started = index(&address, "where=RUNNING_STATE:started").await;
    assert_eq!(ids(&started), [BEACON_HALL]);

    // ...and where they do agree, the group answers.
    let uniform = index(&address, "where=FIRMWARE:2.11").await;
    assert_eq!(ids(&uniform), [ATRIUM_ROOM]);
}

#[tokio::test]
async fn a_group_that_has_reported_nothing_satisfies_no_predicate() {
    // "Every reporting member agrees" must not be vacuously true for a group
    // with no reporting members, or a silent wing would answer to every filter.
    let address = spawn(seeded()).await;

    let body = index(&address, "where=RUNNING_STATE:started").await;

    assert!(
        !ids(&body).contains(&WEST_WING.to_owned()),
        "a silent group should not have matched, got {:?}",
        ids(&body)
    );
}

#[tokio::test]
async fn sync_names_the_groups_by_what_they_were_told_and_whether_they_did_it() {
    // The question no per-device route can ask: the comparison needs an
    // expectation, and an expectation is recorded against a group.
    let address = spawn(seeded()).await;
    start(&address, ATRIUM_ROOM).await;
    start(&address, BEACON_HALL).await;

    // `annex` was told to start and is stopped.
    let drifted = index(&address, "sync=drifted").await;
    assert_eq!(ids(&drifted), [ATRIUM_ROOM]);

    // `beacon` was told to start and did.
    let in_sync = index(&address, "sync=in_sync").await;
    assert_eq!(ids(&in_sync), [BEACON_HALL]);

    // `west-wing` was told nothing, so there is nothing to agree with —
    // `unknown`, never `in_sync`.
    let unknown = index(&address, "sync=unknown").await;
    assert_eq!(ids(&unknown), [WEST_WING]);
}

#[tokio::test]
async fn every_row_carries_the_verdict_the_filter_selects_on() {
    // The filter and the field are one value, so a client can render a status
    // light from the row it was handed rather than re-deriving the precedence
    // and risking a different answer from the server's.
    let address = spawn(seeded()).await;
    start(&address, ATRIUM_ROOM).await;
    start(&address, BEACON_HALL).await;

    let body = index(&address, "").await;

    assert_eq!(
        body["groups"]
            .as_array()
            .expect("groups is an array")
            .iter()
            .map(|g| (
                g["group"].as_str().expect("group"),
                g["sync"].as_str().expect("sync")
            ))
            .collect::<Vec<_>>(),
        [
            (ATRIUM_ROOM, "drifted"),
            (BEACON_HALL, "in_sync"),
            (WEST_WING, "unknown"),
        ]
    );
}

#[tokio::test]
async fn the_verdict_describes_the_group_not_the_projected_columns() {
    // The consequence of rolling up before projecting, and the reason it is
    // worth serving: the row says "this group drifted" while the only column
    // asked for says "unknown" — meaning it drifted somewhere the caller did
    // not ask to see. A verdict recomputed over the projected columns could
    // only ever say `unknown` here, and would contradict the `?sync=` that
    // selected the row.
    let address = spawn(seeded()).await;
    start(&address, ATRIUM_ROOM).await;

    let body = index(&address, "fields=FIRMWARE&sync=drifted").await;

    let row = &body["groups"][0];
    assert_eq!(row["group"], ATRIUM_ROOM);
    assert_eq!(row["sync"], "drifted");
    assert_eq!(fields(row), ["FIRMWARE"]);
    assert_eq!(row["fields"][0]["sync"], "unknown");
}

#[tokio::test]
async fn the_verdict_rolls_up_across_every_field_not_only_the_projected_ones() {
    // `?fields=FIRMWARE&sync=drifted` is "the firmware of every device group
    // that has drifted in any field". Rolled up after projection it would
    // quietly become "...that has drifted in FIRMWARE", a different and much
    // narrower question — and `atrium-room`, whose members agree about
    // firmware and disagree about recording, would vanish from the answer.
    let address = spawn(seeded()).await;
    start(&address, ATRIUM_ROOM).await;

    let body = index(&address, "fields=FIRMWARE&sync=drifted").await;

    assert_eq!(ids(&body), [ATRIUM_ROOM]);
    assert_eq!(fields(&body["groups"][0]), ["FIRMWARE"]);
}

#[tokio::test]
async fn groups_narrows_the_page_by_id() {
    let address = spawn(seeded()).await;

    let body = index(&address, &format!("groups={WEST_WING},{ATRIUM_ROOM}")).await;

    // Still id order, not the order the filter named them in.
    assert_eq!(ids(&body), [ATRIUM_ROOM, WEST_WING]);
}

#[tokio::test]
async fn an_unknown_group_in_the_filter_is_a_404_that_names_it() {
    let address = spawn(seeded()).await;

    let (status, body) = get(format!("{address}{SCOPE}/groups?groups=nowhere")).await;

    assert_eq!(status, 404);
    assert_eq!(body["code"], "not_found");
    assert!(
        body["error"]
            .as_str()
            .expect("error is a string")
            .contains("nowhere"),
        "the message should name the id that is not configured, got {body}"
    );
}

#[tokio::test]
async fn a_device_id_in_the_group_filter_points_at_the_device_index() {
    // The mirror of the device index's message for a group id: not a typo but
    // a caller on the wrong index, so the fix names the other route.
    let address = spawn(seeded()).await;

    let (status, body) = get(format!("{address}{SCOPE}/groups?groups={ATRIUM}")).await;

    assert_eq!(status, 404);
    let message = body["error"].as_str().expect("error is a string");
    assert!(
        message.contains(&format!("/v1/reads/devices?devices={ATRIUM}")),
        "the message should name the index that answers, got {message}"
    );
}

#[tokio::test]
async fn an_unrecognized_sync_value_is_refused_rather_than_ignored() {
    // Ignoring it would answer a wider question than the one asked — every
    // group rather than the drifted ones.
    let address = spawn(seeded()).await;

    let (status, body) = get(format!("{address}{SCOPE}/groups?sync=sideways")).await;

    assert_eq!(status, 400);
    assert_eq!(body["code"], "bad_instruction");
    let message = body["error"].as_str().expect("error is a string");
    assert!(
        message.contains("drifted"),
        "the message should name what is accepted, got {message}"
    );
}

#[tokio::test]
async fn a_malformed_predicate_is_refused_rather_than_ignored() {
    let address = spawn(seeded()).await;

    let (status, body) = get(format!("{address}{SCOPE}/groups?where=RUNNING_STATE")).await;

    assert_eq!(status, 400);
    assert_eq!(body["code"], "bad_instruction");
}

#[tokio::test]
async fn a_walk_covers_every_group_without_repeating_or_skipping_one() {
    let address = spawn(seeded()).await;

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    // Bounded so a cursor that fails to advance fails the test rather than
    // hanging it.
    for _ in 0..5 {
        let query = match &cursor {
            Some(after) => format!("limit=1&after={after}"),
            None => "limit=1".to_owned(),
        };
        let body = index(&address, &query).await;

        seen.extend(ids(&body));
        match body["next"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }

    assert_eq!(seen, [ATRIUM_ROOM, BEACON_HALL, WEST_WING]);
}

#[tokio::test]
async fn a_cursor_walks_the_filtered_set_rather_than_every_group() {
    // Pagination composes with the filters: the cursor advances over what
    // survived them, so a page boundary cannot resurrect a group `?sync=`
    // excluded. The two drifted groups are the first and last in id order with
    // an in-sync one between them, which is what makes "stepped over" visible
    // rather than merely consistent with the answer.
    //
    // `dormant` has to report something for `west-wing` to drift: told to start
    // and silent is `unknown`, not `drifted` — missing evidence rather than
    // contrary evidence.
    let mut reads = seeded();
    reads.push(state(DORMANT, RecordingState::Stopped));
    let address = spawn(reads).await;

    start(&address, ATRIUM_ROOM).await;
    start(&address, BEACON_HALL).await;
    start(&address, WEST_WING).await;

    let first = index(&address, "sync=drifted&limit=1").await;
    assert_eq!(ids(&first), [ATRIUM_ROOM]);
    assert_eq!(first["next"], ATRIUM_ROOM);

    let second = index(
        &address,
        &format!("sync=drifted&limit=1&after={ATRIUM_ROOM}"),
    )
    .await;
    // `beacon-hall` is in sync and sits between them in id order; the walk
    // steps over it rather than spending a page on it.
    assert_eq!(ids(&second), [WEST_WING]);
    assert_eq!(second["next"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_full_page_with_nothing_after_it_carries_no_cursor() {
    let address = spawn(seeded()).await;

    let body = index(&address, "limit=3").await;

    assert_eq!(ids(&body), [ATRIUM_ROOM, BEACON_HALL, WEST_WING]);
    assert_eq!(body["next"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_page_of_no_groups_is_refused() {
    let address = spawn(seeded()).await;

    let (status, body) = get(format!("{address}{SCOPE}/groups?limit=0")).await;

    assert_eq!(status, 400);
    assert_eq!(body["code"], "bad_instruction");
}

#[tokio::test]
async fn an_over_large_limit_is_capped_rather_than_refused() {
    let address = spawn(seeded()).await;

    let body = index(&address, "limit=4294967295").await;

    assert_eq!(ids(&body), [ATRIUM_ROOM, BEACON_HALL, WEST_WING]);
}

#[tokio::test]
async fn a_store_failure_is_a_500_and_not_an_index_of_empty_groups() {
    // Every field on every row comes from a store read, so a backend outage
    // could plausibly render as an index of groups that all know nothing —
    // a fleet that looks configured and silent rather than one nobody can read.
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();
    crate::harness::serve_with(listener, Arc::new(crate::FailingStore), catalog());
    let address = format!("http://127.0.0.1:{port}");

    let (status, body) = get(format!("{address}{SCOPE}/groups")).await;

    assert_eq!(status, 500);
    assert_eq!(body["code"], "internal");
}

#[tokio::test]
async fn the_group_index_route_is_a_get() {
    let address = spawn(seeded()).await;

    let response = reqwest::Client::new()
        .post(format!("{address}{SCOPE}/groups"))
        .send()
        .await
        .expect("posting to the group index");

    assert_eq!(response.status().as_u16(), 405);
    assert_eq!(
        response.headers().get("allow").map(|v| v.as_bytes()),
        Some(&b"GET"[..])
    );
}
