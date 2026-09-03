//! `/v1/writings/groups/{id}/…` — asking a whole device group to do something,
//! and reading back what each member was asked.
//!
//! Two things make this half worth having rather than a synonym for the device
//! half. One request expands into one writing per member, under a rendezvous
//! when the verb needs unison; and the two status reads answer questions the
//! device half *cannot* answer for a group at all — the outbox keys its logs by
//! device, so a group id there used to report an idle device that does not
//! exist.

use sismatic_store::outbox::WritingLog;
use sismatic_store_memory::MemoryOutbox;

use crate::{ANNEX, ATRIUM, GROUP, SCOPE, get, post, put, spawn_over};

/// A two-member device group, so a submission expands into something worth
/// counting.
fn spawn() -> (String, MemoryOutbox) {
    spawn_over(&[ATRIUM, ANNEX])
}

/// Ask the device group to start recording, the way a client does.
async fn start_the_device_group(address: &str) {
    let (status, _, body) = post(address, &format!("/groups/{GROUP}/recording/start")).await;
    assert_eq!(
        status, 202,
        "the device group should have been asked to start: {body}"
    );
}

/// The device ids an acceptance body lists, in order.
fn devices(body: &serde_json::Value) -> Vec<&str> {
    body["writings"]
        .as_array()
        .expect("writings")
        .iter()
        .map(|c| c["device"].as_str().expect("device"))
        .collect()
}

// ---- expansion ------------------------------------------------------------

/// Every write verb is reachable under `/groups` and expands across the
/// members, which is what makes the half worth having.
#[tokio::test]
async fn every_write_verb_is_addressable_under_the_group_space() {
    for (method, tail) in [
        ("post", "/recording/start"),
        ("post", "/recording/stop"),
        ("put", "/metadata/title"),
        ("put", "/settings/timezone"),
    ] {
        // A fresh server per verb: `stop` needs an idle group to refuse and
        // `start` needs one to accept, and this test is about routing and
        // expansion rather than about the admission table, which
        // `each_lifecycle_verb_is_refused_by_the_state_that_contradicts_it`
        // already covers on the device half.
        let (address, _outbox) = spawn();
        let path = format!("/groups/{GROUP}{tail}");
        let (status, _, body) = match method {
            "post" => post(&address, &path).await,
            _ => put(&address, &path, "Week 4").await,
        };

        assert!(
            status == 202 || status == 409,
            "{method} {path} answered {status}: {body}"
        );
        if status == 202 {
            // One writing per member, in configured order — reached through a
            // URL that says what it is doing.
            assert_eq!(devices(&body), [ATRIUM, ANNEX], "for {method} {path}");
        }
    }
}

/// The expansion in full: one request, one row per member, all under one batch,
/// and no `Location` — a group produced several writings, and a header naming
/// an arbitrary one of them would be worse than none.
#[tokio::test]
async fn a_group_start_expands_into_one_writing_per_member() {
    let (address, _outbox) = spawn();

    let (status, location, body) =
        post(&address, &format!("/groups/{GROUP}/recording/start")).await;

    assert_eq!(status, 202, "got {body}");
    assert_eq!(location, None);
    assert!(
        body["batch"].as_str().is_some(),
        "a lifecycle verb over a group needs a rendezvous: {body}"
    );
    assert_eq!(
        devices(&body),
        [ATRIUM, ANNEX],
        "one row per member: {body}"
    );
}

/// The three lifecycle verbs need the members to act together, so a group start
/// is expanded under a rendezvous and every row carries the batch. A *write*
/// gains nothing from unison: setting the same title on two recorders is the
/// same result whenever each one happens, so making them wait on each other
/// would only expose them to a barrier they have no use for.
#[tokio::test]
async fn a_group_start_is_batched_and_a_metadata_write_is_not() {
    let (address, _outbox) = spawn();

    // The title first, while both members are idle and metadata is writable —
    // otherwise the write is refused and the body is an `ApiError`, whose
    // missing `batch` would read as `null` and prove nothing.
    let (status, _, titled) = put(
        &address,
        &format!("/groups/{GROUP}/metadata/title"),
        "Week 4",
    )
    .await;
    assert_eq!(status, 202);
    assert!(
        titled["batch"].is_null(),
        "a write needs no rendezvous, got {titled}"
    );
    assert_eq!(titled["writings"].as_array().expect("writings").len(), 2);

    let (status, _, started) = post(&address, &format!("/groups/{GROUP}/recording/start")).await;
    assert_eq!(status, 202);
    assert!(
        started["batch"].is_string(),
        "a lifecycle verb needs a rendezvous, got {started}"
    );
}

/// Every row of a group start carries the batch, so a caller polling one
/// writing can tell it is part of a rendezvous rather than a lone request.
#[tokio::test]
async fn every_row_of_a_group_start_carries_the_batch() {
    let (address, _outbox) = spawn();

    let (_, _, body) = post(&address, &format!("/groups/{GROUP}/recording/start")).await;
    let batch = body["batch"].as_str().expect("a batch id").to_owned();

    for writing in body["writings"].as_array().expect("writings") {
        let id = writing["id"].as_str().expect("id");
        // The scope root, where a writing is fetched by its own id.
        let (status, record) = get(&address, &format!("/{id}")).await;
        assert_eq!(status, 200);
        assert_eq!(record["batch"], batch);
        assert_eq!(record["status"]["state"], "pending");
    }
}

/// Admission is across every member at once, so one member's refusal refuses
/// the whole request — and, the part that matters, records nothing for the
/// other member.
#[tokio::test]
async fn a_group_start_is_refused_whole_when_one_member_is_already_recording() {
    let (address, outbox) = spawn();

    // `annex` is started on its own first.
    let (status, ..) = post(&address, &format!("/devices/{ANNEX}/recording/start")).await;
    assert_eq!(status, 202);

    let (status, _, body) = post(&address, &format!("/groups/{GROUP}/recording/start")).await;
    assert_eq!(status, 409);

    let message = body["error"].as_str().expect("an error message");
    assert!(
        message.contains("already_recording") && message.contains(ANNEX),
        "the refusing member must be named: {message}"
    );

    // `atrium` never learned about it — the group was refused as a whole.
    assert!(
        outbox
            .writings_for(ATRIUM.to_owned())
            .await
            .expect("reading the log")
            .is_empty(),
        "a refused group must record nothing for its other members"
    );
}

// ---- an id that names nothing, or the wrong thing -------------------------

/// The check that makes the two halves mean what they say. Without it a device
/// id here would start one recorder and answer `202`.
#[tokio::test]
async fn a_device_id_on_a_group_write_route_is_refused_with_the_device_url() {
    let (address, _outbox) = spawn();

    let (status, _, body) = post(&address, &format!("/groups/{ATRIUM}/recording/start")).await;

    assert_eq!(status, 404);
    let message = body["error"].as_str().expect("error");
    assert!(
        message.contains("is a device")
            && message.contains(&format!("{SCOPE}/devices/{ATRIUM}/recording/start")),
        "the message should name the route that would have worked, got {message}"
    );
    assert_eq!(body["code"], "not_found");
}

/// A group id is addressable here and refused on the device half, which is the
/// pairing the two halves exist for. The refusal names the URL that works.
#[tokio::test]
async fn a_group_id_is_addressable_under_groups_and_refused_under_devices() {
    let (address, _outbox) = spawn();

    let (accepted, ..) = post(&address, &format!("/groups/{GROUP}/recording/start")).await;
    assert_eq!(accepted, 202);

    let (refused, _, body) = post(&address, &format!("/devices/{GROUP}/recording/start")).await;
    assert_eq!(refused, 404);
    let message = body["error"].as_str().expect("an error message");
    assert!(
        message.contains("is a device group")
            && message.contains(&format!("{SCOPE}/groups/{GROUP}/recording/start")),
        "the refusal must name the route that works: {message}"
    );
}

#[tokio::test]
async fn an_unconfigured_group_is_a_404_on_every_group_route() {
    let (address, _outbox) = spawn();

    for (method, tail) in [
        ("post", "/recording/start"),
        ("post", "/recording/stop"),
        ("post", "/recording/pause"),
        ("put", "/metadata/title"),
        ("put", "/settings/timezone"),
        ("get", "/recording"),
        ("get", "/history"),
    ] {
        let path = format!("/groups/typo{tail}");
        let status = match method {
            "post" => post(&address, &path).await.0,
            "put" => put(&address, &path, "x").await.0,
            _ => get(&address, &path).await.0,
        };
        assert_eq!(status, 404, "for {method} {path}");
    }
}

// ---- the two status reads the device half answers wrongly -----------------

/// The bug this route exists for. The outbox keys its logs by device, so
/// `GET /v1/writings/devices/{group-id}/recording` used to report an idle device
/// that does not exist. It now refuses the id and names this route instead.
#[tokio::test]
async fn the_group_phase_route_reports_members_and_the_device_route_refuses_the_id() {
    let (address, _outbox) = spawn();
    start_the_device_group(&address).await;

    // The device half no longer answers for a group id at all.
    let (status, refusal) = get(&address, &format!("/devices/{GROUP}/recording")).await;
    assert_eq!(status, 404);
    assert!(
        refusal["error"]
            .as_str()
            .expect("error")
            .contains(&format!("{SCOPE}/groups/{GROUP}/recording")),
        "the refusal must name this route, got {refusal}"
    );

    // What the group route says.
    let (status, body) = get(&address, &format!("/groups/{GROUP}/recording")).await;
    assert_eq!(status, 200, "got {body}");
    assert_eq!(body["group"], GROUP);
    assert_eq!(
        body["phase"], "recording",
        "every member was admitted, so they agree"
    );
    let members = body["members"].as_array().expect("members");
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["device"], ATRIUM);
    assert_eq!(members[0]["phase"], "recording");
    assert_eq!(
        members[0]["epoch"], 1,
        "each member opened its own first take"
    );
    assert_eq!(members[1]["device"], ANNEX);
}

/// A device group has no phase of its own, so `null` when the members have
/// diverged — which is what a start that reached only some of them looks like.
#[tokio::test]
async fn a_divided_group_reports_no_shared_phase() {
    let (address, _outbox) = spawn();
    // One member started on its own, through the device half.
    let (status, ..) = post(&address, &format!("/devices/{ATRIUM}/recording/start")).await;
    assert_eq!(status, 202);

    let (_, body) = get(&address, &format!("/groups/{GROUP}/recording")).await;

    assert!(
        body["phase"].is_null(),
        "the members disagree, so there is no group phase: {body}"
    );
    assert_eq!(body["members"][0]["phase"], "recording");
    assert_eq!(body["members"][1]["phase"], "idle");
}

#[tokio::test]
async fn a_group_nothing_was_written_to_is_idle_on_every_member() {
    let (address, _outbox) = spawn();

    let (_, body) = get(&address, &format!("/groups/{GROUP}/recording")).await;

    assert_eq!(body["phase"], "idle");
    for member in body["members"].as_array().expect("members") {
        assert_eq!(member["phase"], "idle");
        assert_eq!(member["epoch"], 0);
    }
}

/// The other route that used to answer wrongly: an empty list for a group whose
/// members each have a queue. Refused now, and answered here.
#[tokio::test]
async fn the_group_writing_list_is_partitioned_by_member() {
    let (address, _outbox) = spawn();
    start_the_device_group(&address).await;

    let (refused, _) = get(&address, &format!("/devices/{GROUP}/history")).await;
    assert_eq!(refused, 404);

    let (status, body) = get(&address, &format!("/groups/{GROUP}/history")).await;
    assert_eq!(status, 200, "got {body}");

    assert_eq!(body["group"], GROUP);
    let members = body["members"].as_array().expect("members");
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["device"], ATRIUM);
    assert_eq!(members[1]["device"], ANNEX);
    for member in members {
        let writings = member["writings"].as_array().expect("writings");
        assert_eq!(writings.len(), 1, "one row per member");
        assert_eq!(writings[0]["intent"]["kind"], "start_recording");
        // The batch is what ties one group-addressed request back together.
        assert!(writings[0]["batch"].is_string());
    }
    // Both rows share it.
    assert_eq!(
        members[0]["writings"][0]["batch"],
        members[1]["writings"][0]["batch"]
    );
}

#[tokio::test]
async fn a_member_that_has_been_asked_nothing_is_an_empty_list_not_an_omission() {
    let (address, _outbox) = spawn();
    let (status, ..) = post(&address, &format!("/devices/{ATRIUM}/recording/start")).await;
    assert_eq!(status, 202);

    let (_, body) = get(&address, &format!("/groups/{GROUP}/history")).await;

    assert_eq!(body["members"].as_array().expect("members").len(), 2);
    assert_eq!(body["members"][0]["writings"].as_array().unwrap().len(), 1);
    assert_eq!(body["members"][1]["device"], ANNEX);
    assert_eq!(body["members"][1]["writings"].as_array().unwrap().len(), 0);
}
