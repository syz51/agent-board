//! The client repeats a read the reverse proxy failed while the server restarts, never a
//! write, and says what to do when the failure outlasts its repeats.

mod common;

use std::time::Duration;

use board::client::{Client, SnapshotQuery};
use board_proto::{SnapshotFormat, SnapshotKind, TaskId};
use common::*;

/// Two repeats without waiting, so the tests never sleep.
const NO_PAUSE: &[Duration] = &[Duration::ZERO, Duration::ZERO];

fn client(stub: &Stub) -> Client {
    Client::new(
        &stub.url,
        TOKEN.into(),
        Some("t3:reader".parse().unwrap()),
        None,
    )
    .unwrap()
    .with_read_retries(NO_PAUSE)
}

fn snapshot_query() -> SnapshotQuery {
    SnapshotQuery {
        agent: "t3:reader".parse().unwrap(),
        kind: SnapshotKind::Delta,
        since: None,
        format: SnapshotFormat::Lines,
    }
}

#[tokio::test]
async fn a_read_the_proxy_failed_once_returns_the_next_answer() {
    let stub = Stub::start();
    stub.on("GET /v1/boards", [board_fixture("factory", &[])])
        .once_error("GET /v1/boards", 502, "internal", "bad gateway");

    let boards = client(&stub).boards().await.unwrap();

    assert_eq!(boards.len(), 1);
    assert_eq!(stub.requests_to("GET /v1/boards").len(), 2);
}

#[tokio::test]
async fn a_read_that_keeps_failing_says_the_server_may_be_restarting() {
    let stub = Stub::start();
    stub.on_status("GET /v1/boards", 503, "<html>Service Unavailable</html>");

    let err = client(&stub).boards().await.unwrap_err();

    let text = format!("{err:#}");
    assert!(
        text.contains(
            "the board server may be restarting (it answered 503 Service Unavailable 3 times)"
        ) && text.contains("safe to repeat"),
        "{text}"
    );
    assert_eq!(stub.requests_to("GET /v1/boards").len(), 3);
}

#[tokio::test]
async fn a_read_repeats_after_a_gateway_timeout() {
    let stub = Stub::start();
    stub.on("GET /v1/boards", [board_fixture("factory", &[])])
        .once_error("GET /v1/boards", 504, "internal", "gateway timeout");

    client(&stub).boards().await.unwrap();

    assert_eq!(stub.requests_to("GET /v1/boards").len(), 2);
}

#[tokio::test]
async fn a_snapshot_repeats_only_when_the_server_was_not_reached() {
    let stub = Stub::start();
    stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Delta, "ok", 7),
    )
    .once_error("GET /v1/snapshot", 502, "internal", "bad gateway");
    let client = client(&stub);

    client.snapshot(&snapshot_query()).await.unwrap();
    assert_eq!(stub.requests_to("GET /v1/snapshot").len(), 2);

    // A 504's answer may be lost after the server moved the alert cursor: never repeated.
    stub.clear();
    stub.once_error("GET /v1/snapshot", 504, "internal", "gateway timeout");
    let err = client.snapshot(&snapshot_query()).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("may be restarting (it answered 504 Gateway Timeout)"),
        "{err:#}"
    );
    assert_eq!(stub.requests_to("GET /v1/snapshot").len(), 1);
}

#[tokio::test]
async fn a_write_the_proxy_failed_is_not_repeated() {
    let stub = Stub::start();
    stub.on_status(
        "POST /v1/tasks/factory-1/claim",
        502,
        "<html>Bad Gateway</html>",
    );
    let id: TaskId = "factory-1".parse().unwrap();

    let err = client(&stub).task_action(&id, "claim").await.unwrap_err();

    let text = format!("{err:#}");
    assert!(
        text.contains("the board server may be restarting (it answered 502 Bad Gateway)")
            && text.contains("check with a read whether this change took effect"),
        "{text}"
    );
    assert_eq!(stub.requests_to("POST /v1/tasks/factory-1/claim").len(), 1);
}

#[tokio::test]
async fn a_client_without_read_retries_sends_each_read_once() {
    let stub = Stub::start();
    stub.on_status("GET /v1/boards", 502, "<html>Bad Gateway</html>");
    let client = Client::new(&stub.url, TOKEN.into(), None, None).unwrap();

    client.boards().await.unwrap_err();

    assert_eq!(stub.requests_to("GET /v1/boards").len(), 1);
}

#[tokio::test]
async fn other_server_errors_are_not_repeated() {
    let stub = Stub::start();
    stub.on_error(
        "GET /v1/boards",
        500,
        "internal",
        "database is locked",
        "retry",
    );

    let err = client(&stub).boards().await.unwrap_err();

    assert!(!format!("{err:#}").contains("restarting"), "{err:#}");
    assert_eq!(stub.requests_to("GET /v1/boards").len(), 1);
}
