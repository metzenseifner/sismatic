//! tests/health_check.rs — the health check as its callers meet it.
//!
//! Black-box: every test here starts the real server on a real socket and talks
//! to it with an HTTP client, rather than driving handlers in-process through
//! actix's test harness. The endpoint's whole purpose is to be probed from
//! outside by something that is not this program, so what is worth pinning is
//! the status code that reaches the wire — not that a function returned a value
//! that would have become one. Nothing below names a handler, so the routes may
//! be re-organized freely and only a change in *behaviour* breaks a test.
//!
//! [`spawn_app`] binds port 0 and asks the kernel which port it got, so the
//! suite needs no reserved port and its tests cannot collide with each other or
//! with anything else on the machine.

use std::net::TcpListener;
use std::sync::Arc;

use sismatic_api_types::{DeviceId, Reading, TimeSpan};
use sismatic_store::{DynReadStore, ReadError, ReadStore};

/// A store that fails the test if it is read at all.
///
/// The health check must be answerable without consulting anything — see
/// `the_health_check_does_not_consult_the_store`.
struct UnreachableStore;

#[async_trait::async_trait]
impl ReadStore for UnreachableStore {
    async fn latest(&self, _dev: DeviceId) -> Result<Option<Reading>, ReadError> {
        panic!("the health check must not read from the store")
    }

    async fn between(&self, _dev: DeviceId, _span: TimeSpan) -> Result<Vec<Reading>, ReadError> {
        panic!("the health check must not read from the store")
    }
}

/// Start the application on an ephemeral port and return its base URL.
///
/// The listener is bound *here* rather than inside `run` so the port the kernel
/// chose is knowable: that is the reason `run` takes a [`TcpListener`] at all.
///
/// The store it is given is [`UnreachableStore`], since no test in this file is
/// about reading data; one that is would pass its own.
fn spawn_app() -> String {
    let store: DynReadStore = Arc::new(UnreachableStore);

    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral port");
    let port = listener
        .local_addr()
        .expect("reading the bound address")
        .port();

    let server = sismatic_http_api::run(listener, store).expect("building the server");
    // Detached deliberately: dropping a `JoinHandle` leaves the task running,
    // so the server lives exactly as long as the test's runtime does and no
    // test has to remember to stop it.
    drop(tokio::spawn(server));

    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn the_health_check_answers_200_with_no_body() {
    let address = spawn_app();

    let response = reqwest::get(format!("{address}/health_check"))
        .await
        .expect("requesting the health check");

    assert!(response.status().is_success());
    // Empty, not merely short: the status is the whole answer, so a probe has
    // nothing to parse and no shape that could later need versioning.
    assert_eq!(response.content_length(), Some(0));
}

#[tokio::test]
async fn the_health_check_does_not_consult_the_store() {
    // The store is wired in and reachable from any handler — `spawn_app` hands
    // the application one whose every method panics. A health check that grew a
    // dependency on it would report a store outage as *this process is dead*,
    // so the independence is worth a test rather than a comment.
    let address = spawn_app();

    let response = reqwest::get(format!("{address}/health_check"))
        .await
        .expect("requesting the health check");

    assert!(response.status().is_success());
}

#[tokio::test]
async fn the_health_check_is_a_get() {
    // Registered by method, not just by path: a probe misconfigured to POST
    // gets 405 and says so, rather than a 200 that would report the server
    // healthy for a reason unrelated to the check.
    let address = spawn_app();

    let response = reqwest::Client::new()
        .post(format!("{address}/health_check"))
        .send()
        .await
        .expect("posting to the health check");

    assert_eq!(response.status().as_u16(), 405);
    // ...and names the method that would have worked, which is the difference
    // between this and a bare 404 (see `startup::run`).
    assert_eq!(
        response.headers().get("allow").map(|v| v.as_bytes()),
        Some(&b"GET"[..])
    );
}

#[tokio::test]
async fn an_unknown_path_is_404() {
    // The complement of the check above: a 200 from `/health_check` means
    // something only if the server is not answering 200 to everything.
    let address = spawn_app();

    let response = reqwest::get(format!("{address}/health"))
        .await
        .expect("requesting an unrouted path");

    assert_eq!(response.status().as_u16(), 404);
}
