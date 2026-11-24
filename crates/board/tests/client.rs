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

#[tokio::test]
async fn client_calls_device_registry_and_usage_bucket_endpoints() {
    use board_proto::{
        AdminUpdateDevice, DeviceCapacity, DeviceHeartbeat, DeviceRole, DeviceStatus,
        PutUsageBucket, PutUsageBucketReserve, Taint, TaintEffect, UsageWindow,
    };
    use serde_json::json;

    let stub = Stub::start();
    let now: board_proto::Timestamp = "2026-11-24T08:00:00Z".parse().unwrap();
    let entry = json!({
        "name": "mac-pro",
        "role": "runner",
        "status": "online",
        "drain": false,
        "os": "macos",
        "arch": "arm64",
        "capacity": { "weight_units": 12, "mem_gib": 48, "cpu_cores": 14 },
        "free_units": 8,
        "capabilities": ["os:macos"],
        "runners": ["acp"],
        "repos": ["github.com/example/app"],
        "taints": [],
        "preference": 10,
        "protocol_features": ["v1-json"],
        "heartbeat_at": now,
    });
    let bucket = json!({
        "provider": "antigravity",
        "account": "default",
        "five_hour": { "percent": 15.0 },
        "read_at": now,
        "device": "mac-pro",
        "reserve": 5.5,
    });
    stub.on("PUT /v1/devices/mac-pro/heartbeat", entry.clone())
        .on("GET /v1/devices", json!([entry.clone()]))
        .on("PATCH /v1/admin/devices/mac-pro", entry)
        .on("PUT /v1/usage/antigravity/default", bucket.clone())
        .on("PUT /v1/usage/antigravity/default/reserve", bucket.clone())
        .on("GET /v1/usage/buckets", json!([bucket]));

    let c = client(&stub);
    let dev: board_proto::DeviceName = "mac-pro".parse().unwrap();
    let hb = c
        .device_heartbeat(
            &dev,
            &DeviceHeartbeat {
                os: Some("macos".into()),
                arch: Some("arm64".into()),
                capacity: Some(DeviceCapacity {
                    weight_units: 12,
                    mem_gib: Some(48),
                    cpu_cores: Some(14),
                }),
                free_units: Some(8),
                capabilities: vec!["os:macos".into()],
                runners: vec!["acp".into()],
                repos: vec!["github.com/example/app".into()],
                protocol_features: vec!["v1-json".into()],
            },
        )
        .await
        .unwrap();
    assert_eq!(hb.status, DeviceStatus::Online);
    assert_eq!(c.list_devices().await.unwrap().len(), 1);
    let admin = c
        .admin_update_device(
            &dev,
            &AdminUpdateDevice {
                role: Some(DeviceRole::Runner),
                taints: Some(vec![Taint {
                    key: "weight".into(),
                    value: Some("heavy".into()),
                    effect: TaintEffect::PreferNoSchedule,
                }]),
                preference: Some(10),
                drain: Some(false),
            },
        )
        .await
        .unwrap();
    assert_eq!(admin.name, dev);

    let b = c
        .put_usage_bucket(
            "antigravity",
            "default",
            &PutUsageBucket {
                five_hour: Some(UsageWindow {
                    percent: 15.0,
                    resets_at: None,
                }),
                seven_day: None,
                read_at: now,
            },
        )
        .await
        .unwrap();
    assert_eq!(b.provider, "antigravity");
    let r = c
        .put_usage_bucket_reserve(
            "antigravity",
            "default",
            &PutUsageBucketReserve { reserve: Some(5.5) },
        )
        .await
        .unwrap();
    assert_eq!(r.reserve, Some(5.5));
    assert_eq!(c.list_usage_buckets().await.unwrap().len(), 1);
}
