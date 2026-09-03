//! tests/instructions.rs — the two scope roots: which names the API accepts.
//!
//! Black-box like every other suite here: the real server on a real socket, and
//! the bodies read back over HTTP.
//!
//! # What this suite can and cannot check
//!
//! The lists these routes serve are `sismatic-core`'s instruction catalogs, and
//! this crate may not name `sismatic-core` — that is the load-bearing rule of the
//! workspace layout, and the reason the catalogs arrive as DTOs the composition
//! root projected. So the property under test here is *transport*: whatever was
//! handed to `run` is what the route publishes, unaltered and in order.
//!
//! Whether what the composition root hands over is core's whole catalog is the
//! other half, and it is checked where both halves are in scope —
//! `sismatic-server`'s `the_published_catalogs_cover_every_instruction_core_has`.
//! Neither crate can check both, and the pair leaves nothing unchecked.
//!
//! # The one route this could have broken
//!
//! `GET /v1/commands` sits on a scope whose root already had a route:
//! `GET /v1/commands/{id}`, which resolves a command id. The two cannot collide
//! — one matches an empty tail and the other exactly one segment — but "cannot"
//! is worth a test rather than an argument, because the failure would be a
//! command lookup quietly answering with a catalog.

use std::sync::Arc;

use sismatic_api_types::{CommandCatalog, FieldCatalog, Intent, Timestamp};
use sismatic_store::DynReadStore;
use sismatic_store::outbox::{CommandSubmit, Submission};
use sismatic_store_memory::MemoryStore;

mod harness;

/// The catalogs are static, so no suite here needs a reading in the store.
fn empty_store() -> DynReadStore {
    Arc::new(MemoryStore::default())
}

/// `GET path`, returning the status and the parsed JSON body.
async fn get(address: &str, path: &str) -> (u16, serde_json::Value) {
    let response = reqwest::get(format!("{address}{path}"))
        .await
        .expect("issuing the request");
    let status = response.status().as_u16();
    let body = response.json().await.expect("parsing the response body");
    (status, body)
}

/// Serve the harness's stated catalogs; return the base URL.
fn spawn() -> String {
    let (address, _) = harness::spawn_with_instructions(
        empty_store(),
        harness::field_catalog(),
        harness::command_catalog(),
    );
    address
}

#[tokio::test]
async fn the_readings_root_lists_every_field_that_can_be_asked_for() {
    let address = spawn();

    let (status, body) = get(&address, "/v1/readings").await;

    assert_eq!(status, 200);
    // Compared whole rather than by key, because the shape is the contract: a
    // client reads `name` to build its next URL and `aliases` to recognise a
    // spelling it already has stored.
    assert_eq!(
        body,
        serde_json::json!({"fields": [
            {
                "name": "RUNNING_STATE",
                "aliases": [],
                "description": "Current recording state.",
            },
            {
                "name": "STREAM_1_NAME",
                "aliases": ["STREAM_NAME_1"],
                "description": "Name of stream 1.",
            },
        ]})
    );
}

/// Catalog order, not sorted — and the harness's fixture is alphabetical by
/// accident, so this states the property against a catalog that is not.
#[tokio::test]
async fn the_field_list_is_served_in_the_order_it_was_given() {
    let fields = FieldCatalog {
        fields: vec![
            harness::instruction("ZULU", &[], "Last in the catalog."),
            harness::instruction("ALPHA", &[], "First in the catalog."),
        ],
    };
    let (address, _) =
        harness::spawn_with_instructions(empty_store(), fields, harness::command_catalog());

    let (_, body) = get(&address, "/v1/readings").await;

    let names: Vec<&str> = body["fields"]
        .as_array()
        .expect("fields is an array")
        .iter()
        .map(|f| f["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        names,
        ["ZULU", "ALPHA"],
        "the route sorted a list whose order is the catalog's"
    );
}

/// The alias half is the part no normalization rule derives: a caller can fold
/// case and `-`/`_` on its own, and cannot guess that `STREAM_NAME_1` is the
/// same field as `STREAM_1_NAME`.
#[tokio::test]
async fn a_fields_synonyms_are_published_beside_its_canonical_name() {
    let address = spawn();

    let (_, body) = get(&address, "/v1/readings").await;

    let stream = &body["fields"][1];
    assert_eq!(stream["name"], "STREAM_1_NAME");
    assert_eq!(stream["aliases"], serde_json::json!(["STREAM_NAME_1"]));
    // The canonical name is not repeated in its own synonym list, which is what
    // the projection drops it for.
    assert!(
        !stream["aliases"]
            .as_array()
            .expect("aliases is an array")
            .contains(&serde_json::json!("STREAM_1_NAME")),
        "the canonical spelling was listed as an alias of itself: {stream}"
    );
}

#[tokio::test]
async fn the_commands_root_lists_the_three_kinds_of_write_apart() {
    let address = spawn();

    let (status, body) = get(&address, "/v1/commands").await;

    assert_eq!(status, 200);
    // Three keys, not one flat list: `TITLE` is refused by the settings route
    // and `TIMEZONE` by the metadata route, so a merged list would advertise
    // names half of which each write route rejects.
    assert_eq!(
        body,
        serde_json::json!({
            "commands": [
                {
                    "name": "STARTRECORDING",
                    "aliases": ["START"],
                    "description": "Start recording.",
                },
            ],
            "metadata": [
                {"name": "TITLE", "aliases": [], "description": "Recording title."},
            ],
            "settings": [
                {"name": "TIMEZONE", "aliases": [], "description": "Configured timezone."},
            ],
        })
    );
}

/// The collision that cannot happen, pinned anyway: `/v1/commands` and
/// `/v1/commands/{id}` share a scope root, and the failure — a command lookup
/// answering with a catalog, or the catalog answering `404 no command
/// 'commands'` — would be silent in both directions.
#[tokio::test]
async fn the_command_catalog_does_not_shadow_a_command_id() {
    let (address, outbox) = harness::spawn_with_instructions(
        empty_store(),
        harness::field_catalog(),
        harness::command_catalog(),
    );
    // Through the port rather than over HTTP: this suite is about routing, and
    // seeding through a route would make it depend on the routing it checks.
    outbox
        .submit(Submission {
            ids: vec!["cmd-1".to_owned()],
            targets: vec![harness::DEVICE.to_owned()],
            group: None,
            batch: None,
            barrier: None,
            intent: Intent::SetSetting {
                field: "TIMEZONE".to_owned(),
                value: "UTC".to_owned(),
            },
            at: Timestamp(harness::AT.to_owned()),
            idempotency_key: None,
        })
        .await
        .expect("seeding the outbox");

    let (status, command) = get(&address, "/v1/commands/cmd-1").await;
    assert_eq!(status, 200);
    assert_eq!(command["id"], "cmd-1");

    // ...and the other direction: the catalog is not reached by anything that
    // looks like an id.
    let (status, body) = get(&address, "/v1/commands/no-such-command").await;
    assert_eq!(status, 404, "got {body}");
    assert!(
        body["fields"].is_null() && body["settings"].is_null(),
        "an unknown command id was answered with a catalog: {body}"
    );
}

/// An empty catalog is an empty list, for the same reason an empty device index
/// is: the server is stating what it knows, and "nothing" is a statement.
#[tokio::test]
async fn empty_catalogs_are_empty_lists_not_errors() {
    let (address, _) = harness::spawn_with_instructions(
        empty_store(),
        FieldCatalog::default(),
        CommandCatalog::default(),
    );

    assert_eq!(
        get(&address, "/v1/readings").await,
        (200, serde_json::json!({"fields": []}))
    );
    assert_eq!(
        get(&address, "/v1/commands").await,
        (
            200,
            serde_json::json!({"commands": [], "metadata": [], "settings": []})
        )
    );
}

/// Both roots are exact paths and neither folds a trailing slash, which is the
/// rule everywhere in this application except `/api` — see `startup`'s note on
/// why the fix there is one redirect rather than `NormalizePath` over
/// everything.
#[tokio::test]
async fn the_roots_are_exact_paths() {
    let address = spawn();

    for path in ["/v1/readings/", "/v1/commands/"] {
        let status = reqwest::get(format!("{address}{path}"))
            .await
            .expect("requesting a slashed root")
            .status()
            .as_u16();
        assert_eq!(status, 404, "{path} should not have been normalized");
    }
}

/// Registered as a `resource` with the method on the `route`, so a write to a
/// read-only route is `405` with an `Allow` header rather than `404` — the
/// answer that tells a caller what to change.
#[tokio::test]
async fn the_roots_are_read_only() {
    let address = spawn();

    let response = reqwest::Client::new()
        .post(format!("{address}/v1/commands"))
        .send()
        .await
        .expect("posting to the catalog");

    assert_eq!(response.status().as_u16(), 405);
    assert!(
        response
            .headers()
            .get("allow")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|allow| allow.contains("GET")),
        "a 405 should name the method that works, got {:?}",
        response.headers()
    );
}
