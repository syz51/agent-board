//! Device RPC broker & route tests: auth boundaries (user-only call vs device-self poll/reply),
//! long-poll wakeup, reply delivery, timeout cleanup, validation, and graceful shutdown.

use serde_json::json;

use super::{T, USER};

#[tokio::test]
async fn device_rpc_enforces_user_call_and_device_self_poll_reply_auth() {
    let t = T::new();

    // Ordinary device token (`mac`) cannot call POST /v1/devices/mac/rpc -> 403 forbidden.
    let (code, err, v) = t
        .fail(
            "POST",
            "/v1/devices/mac/rpc",
            None,
            Some(json!({ "method": "t3.projects.list", "params": {} })),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // Another device's `--user` token (`phone`) cannot poll or reply for `mac` -> 403 forbidden.
    let (s, v) = t
        .as_user("GET", "/v1/devices/mac/rpc/poll?wait_secs=0", None)
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (403, Some("forbidden")),
        "{v}"
    );
    let (s, v) = t
        .as_user(
            "POST",
            "/v1/devices/mac/rpc/rpc-abc/reply",
            Some(json!({ "ok": true, "result": {} })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (403, Some("forbidden")),
        "{v}"
    );

    // Tailscale browser user cannot poll or reply -> 403 forbidden.
    let (s, v) = t
        .send(
            "GET",
            "/v1/devices/mac/rpc/poll?wait_secs=0",
            &[("tailscale-user-login", USER)],
            None,
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (403, Some("forbidden")),
        "{v}"
    );

    // Worker agent header on `mac`'s own token cannot hijack RPC polling -> 403 forbidden.
    let (code, err, v) = t
        .fail(
            "GET",
            "/v1/devices/mac/rpc/poll?wait_secs=0",
            Some("t3:worker-1"),
            None,
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // Unknown or revoked device on POST /rpc -> 404 not_found.
    let (s, v) = t
        .as_user(
            "POST",
            "/v1/devices/nonexistent/rpc",
            Some(json!({ "method": "t3.projects.list", "params": {} })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (404, Some("not_found")),
        "{v}"
    );

    // Own device token (`mac`) without X-Board-Agent can poll immediately with wait_secs=0 -> [].
    let empty = t
        .ok("GET", "/v1/devices/mac/rpc/poll?wait_secs=0", None, None)
        .await;
    assert_eq!(empty, json!([]));

    // Own device token (`mac`) with `dispatch:mac` can also poll -> [].
    let empty_dispatch = t
        .ok(
            "GET",
            "/v1/devices/mac/rpc/poll?wait_secs=0",
            Some("dispatch:mac"),
            None,
        )
        .await;
    assert_eq!(empty_dispatch, json!([]));
}

#[tokio::test]
async fn device_rpc_round_trip_long_poll_and_error_reply() {
    let t = T::new();

    // Start long-poll concurrently with the caller so Notify wakes the waiting poller.
    let (polled, call_res) = tokio::join!(
        async {
            let polled = t
                .ok("GET", "/v1/devices/mac/rpc/poll?wait_secs=5", None, None)
                .await;
            let calls = polled.as_array().unwrap();
            assert_eq!(calls.len(), 1, "{polled}");
            let call_id = calls[0]["id"].as_str().unwrap().to_owned();
            assert_eq!(calls[0]["method"], "t3.projects.list");
            assert_eq!(calls[0]["params"], json!({ "filter": "all" }));

            // Replying with ok=true wakes the caller.
            let reply_path = format!("/v1/devices/mac/rpc/{call_id}/reply");
            let (s, _) = t
                .send(
                    "POST",
                    &reply_path,
                    &[("authorization", &format!("Bearer {}", t.token))],
                    Some(json!({ "ok": true, "result": { "projects": ["agent-board"] } })),
                )
                .await;
            assert_eq!(s.as_u16(), 204);

            // Second reply to the same call id -> 404 not_found.
            let (code, err, v) = t
                .fail(
                    "POST",
                    &reply_path,
                    None,
                    Some(json!({ "ok": true, "result": {} })),
                )
                .await;
            assert_eq!((code, err.as_str()), (404, "not_found"), "{v}");
            polled
        },
        async {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            t.as_user(
                "POST",
                "/v1/devices/mac/rpc",
                Some(json!({
                    "method": "t3.projects.list",
                    "params": { "filter": "all" },
                    "timeout_secs": 5
                })),
            )
            .await
        }
    );
    let call_id = polled[0]["id"].as_str().unwrap();
    let (status, resp) = call_res;
    assert_eq!(status.as_u16(), 200, "{resp}");
    assert_eq!(resp["id"], call_id);
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["result"], json!({ "projects": ["agent-board"] }));

    // Device error reply (`ok: false`) is surfaced to caller cleanly.
    let ((), (s2, resp2)) = tokio::join!(
        async {
            let polled2 = t
                .ok("GET", "/v1/devices/mac/rpc/poll?wait_secs=5", None, None)
                .await;
            let id2 = polled2[0]["id"].as_str().unwrap();
            let (s, _) = t
                .send(
                    "POST",
                    &format!("/v1/devices/mac/rpc/{id2}/reply"),
                    &[("authorization", &format!("Bearer {}", t.token))],
                    Some(json!({ "ok": false, "error": "thread not found" })),
                )
                .await;
            assert_eq!(s.as_u16(), 204);
        },
        async {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            t.as_user(
                "POST",
                "/v1/devices/mac/rpc",
                Some(json!({
                    "method": "t3.thread.read",
                    "params": { "thread_id": "missing" },
                    "timeout_secs": 5
                })),
            )
            .await
        }
    );
    assert_eq!(s2.as_u16(), 200, "{resp2}");
    assert_eq!(resp2["ok"], false);
    assert_eq!(resp2["error"], "thread not found");
}

#[tokio::test]
async fn device_rpc_validates_inputs_and_handles_timeout_and_cleanup() {
    let t = T::new();

    // Validation failures -> 400 invalid.
    for bad in [
        json!({ "method": "", "params": {} }),
        json!({ "method": "bad method", "params": {} }),
        json!({ "method": "t3.projects.list", "params": {}, "timeout_secs": 0 }),
        json!({ "method": "t3.projects.list", "params": {}, "timeout_secs": 61 }),
    ] {
        let (s, v) = t.as_user("POST", "/v1/devices/mac/rpc", Some(bad)).await;
        assert_eq!(
            (s.as_u16(), v["code"].as_str()),
            (400, Some("invalid")),
            "{v}"
        );
    }
    let (code, err, v) = t
        .fail("GET", "/v1/devices/mac/rpc/poll?wait_secs=31", None, None)
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");

    for bad_reply in [
        json!({ "ok": true, "error": "should not be set" }),
        json!({ "ok": false, "error": "   " }),
        json!({ "ok": false }),
    ] {
        let (code, err, v) = t
            .fail(
                "POST",
                "/v1/devices/mac/rpc/rpc-1/reply",
                None,
                Some(bad_reply),
            )
            .await;
        assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");
    }

    // Timeout returns ok=false and removes the expired call from the pending queue.
    let (s, resp) = t
        .as_user(
            "POST",
            "/v1/devices/mac/rpc",
            Some(json!({
                "method": "t3.projects.list",
                "params": {},
                "timeout_secs": 1
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{resp}");
    assert_eq!(resp["ok"], false);
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("did not answer rpc t3.projects.list"),
        "{resp}"
    );
    let after = t
        .ok("GET", "/v1/devices/mac/rpc/poll?wait_secs=0", None, None)
        .await;
    assert_eq!(after, json!([]), "timed-out call must be cleaned up");
}
