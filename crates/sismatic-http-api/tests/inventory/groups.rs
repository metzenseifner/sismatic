//! `/v1/inventory/groups…` — the group index and one group's membership.

use sismatic_api_types::{Barrier, GroupSummary};
use sismatic_store_memory::MemoryCatalog;

use crate::{DEVICE, GROUP, SCOPE, get, harness, spawn_with, summary};

#[tokio::test]
async fn the_group_index_lists_members_in_configured_order() {
    let catalog = MemoryCatalog::new(
        vec![
            summary("atrium", "10.0.0.1", false),
            summary("annex", "10.0.0.2", false),
        ],
        vec![GroupSummary {
            id: "room".to_owned(),
            // Not alphabetical: the operator wrote this sequence and a fan-out
            // has to address the device group the way it reads.
            members: vec!["atrium".to_owned(), "annex".to_owned()],
            barrier_timeout_secs: 15,
            barrier: Barrier::FailBatch,
        }],
    );
    let address = spawn_with(catalog);

    let (status, body) = get(&address, "/groups").await;

    assert_eq!(status, 200);
    assert_eq!(
        body,
        serde_json::json!({"groups": [{
            "id": "room",
            "members": ["atrium", "annex"],
            "barrier_timeout_secs": 15,
            "barrier": "fail_batch",
        }]})
    );
}

#[tokio::test]
async fn one_group_resolves_by_id() {
    let address = spawn_with(harness::catalog());

    let (status, body) = get(&address, &format!("/groups/{GROUP}")).await;

    assert_eq!(status, 200);
    assert_eq!(
        body,
        serde_json::json!({
            "id": GROUP,
            "members": [DEVICE],
            "barrier_timeout_secs": 15,
            "barrier": "fail_batch",
        })
    );
}

/// Devices and groups share one id namespace, so naming a device on the group
/// route is a different mistake from naming nothing — and the fix is a
/// different URL rather than a different id. The URL named has to be one *in
/// this scope*, or the hint sends the caller to a second 404.
#[tokio::test]
async fn a_device_id_on_the_group_route_says_which_route_to_use() {
    let address = spawn_with(harness::catalog());

    let (status, body) = get(&address, &format!("/groups/{DEVICE}")).await;

    assert_eq!(status, 404);
    let message = body["error"].as_str().expect("an error message");
    assert!(
        message.contains("is a device") && message.contains(&format!("{SCOPE}/devices/{DEVICE}")),
        "got: {message}"
    );
}

/// The barrier policy reaches a client, because it is the one configured number
/// that changes what a `202` from the writings scope means: a fifteen-second
/// barrier can leave a writing pending that long before anything reaches a
/// device. This scope is the only one that reports it.
#[tokio::test]
async fn a_groups_barrier_policy_is_visible_on_the_inventory_route() {
    let catalog = MemoryCatalog::new(
        vec![summary("front", "10.0.0.1", false)],
        vec![GroupSummary {
            id: "hall".to_owned(),
            members: vec!["front".to_owned()],
            barrier_timeout_secs: 30,
            barrier: Barrier::DispatchReady,
        }],
    );
    let address = spawn_with(catalog);

    let (status, body) = get(&address, "/groups/hall").await;

    assert_eq!(status, 200);
    assert_eq!(body["barrier_timeout_secs"], 30);
    assert_eq!(body["barrier"], "dispatch_ready");
}
