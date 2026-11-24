//! Bounded live run events (`POST/GET /v1/runs/{id}/events` with JSON & SSE), cockpit
//! `steer`/`cancel` controls (`POST /v1/runs/{id}/steer`, `POST /v1/runs/{id}/cancel`), and
//! sweeper retention pruning (`docs/SCHEDULER_DESIGN.md`, Sections 5.2 & 5.3).

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use board_proto::{RUN_EVENT_SUMMARY_CHARS_MAX, RUN_EVENTS_BATCH_MAX, RUN_EVENTS_CAP};
use chrono::TimeDelta;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{REPO, T};

const WORKER: &str = "t3:worker-ev";
const OUTSIDER: &str = "t3:outsider-ev";
const DISPATCH_MAC: &str = "dispatch:mac";

async fn create_device(t: &T, name: &str) -> String {
    let (s, v) = t
        .as_user("POST", "/v1/admin/devices", Some(json!({ "name": name })))
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    v["token"].as_str().unwrap().to_owned()
}

async fn send_as_device(
    t: &T,
    token: &str,
    method: &str,
    path: &str,
    agent: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let bearer = format!("Bearer {token}");
    let mut headers = vec![("authorization", bearer.as_str())];
    if let Some(a) = agent {
        headers.push(("x-board-agent", a));
    }
    let (s, v) = t.send(method, path, &headers, body).await;
    (s.as_u16(), v)
}

async fn read_sse_events(
    body: &mut (impl futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin),
    buffer: &mut String,
    count: usize,
) -> Vec<(i64, Value)> {
    let mut out = Vec::new();
    while out.len() < count {
        while let Some(end) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..end + 2).collect();
            let id = frame
                .lines()
                .find_map(|l| l.strip_prefix("id:"))
                .map(|v| v.trim().parse::<i64>().unwrap());
            let data = frame
                .lines()
                .find_map(|l| l.strip_prefix("data:"))
                .map(|v| serde_json::from_str::<Value>(v.trim()).unwrap());
            if let (Some(id), Some(data)) = (id, data) {
                out.push((id, data));
            }
        }
        if out.len() >= count {
            break;
        }
        let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("SSE event within 5s")
            .expect("SSE stream open")
            .unwrap();
        buffer.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    out
}

#[tokio::test]
async fn push_and_list_run_events_enforce_auth_validation_and_200_event_cap() {
    let t = T::new();
    let other_token = create_device(&t, "linux-runner").await;
    t.board("factory", &[REPO]).await;
    t.register(WORKER, Some(REPO), Some("main")).await;
    t.register(OUTSIDER, Some(REPO), Some("main")).await;
    let task_id = t.task(WORKER, "factory", "Stream live run events").await;

    let run = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "task_id": task_id,
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "agent_id": WORKER,
                "state": "running",
            })),
        )
        .await;
    let run_id = run["id"].as_str().unwrap().to_owned();
    let path = format!("/v1/runs/{run_id}/events");

    // Another device cannot push events for mac's run -> 403 forbidden.
    let (code, v) = send_as_device(
        &t,
        &other_token,
        "POST",
        &path,
        None,
        Some(json!({
            "events": [{ "seq": 1, "kind": "phase", "summary": "spoofed" }]
        })),
    )
    .await;
    assert_eq!((code, v["code"].as_str()), (403, Some("forbidden")), "{v}");

    // An unrelated worker agent on the same device cannot push events for another managed run.
    let (code, err, v) = t
        .fail(
            "POST",
            &path,
            Some(OUTSIDER),
            Some(json!({
                "events": [{ "seq": 1, "kind": "phase", "summary": "outsider" }]
            })),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // Validation: unknown kind, seq <= 0, overlong summary, control chars, and oversized batch.
    let overlong_summary = "x".repeat(RUN_EVENT_SUMMARY_CHARS_MAX + 1);
    for bad in [
        json!({ "events": [{ "seq": 1, "kind": "mystery", "summary": "ok" }] }),
        json!({ "events": [{ "seq": 0, "kind": "phase", "summary": "ok" }] }),
        json!({ "events": [{ "seq": -3, "kind": "phase", "summary": "ok" }] }),
        json!({ "events": [{ "seq": 1, "kind": "phase", "summary": overlong_summary }] }),
        json!({ "events": [{ "seq": 1, "kind": "phase", "summary": "line1\nline2" }] }),
    ] {
        let (code, err, v) = t.fail("POST", &path, Some(DISPATCH_MAC), Some(bad)).await;
        assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");
    }
    let oversized: Vec<Value> = (1..=(RUN_EVENTS_BATCH_MAX + 1))
        .map(|i| json!({ "seq": i, "kind": "note", "summary": format!("e{i}") }))
        .collect();
    let (code, err, v) = t
        .fail(
            "POST",
            &path,
            Some(DISPATCH_MAC),
            Some(json!({ "events": oversized })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");

    // Device daemon, the run's own agent, and the user can all push events.
    let first = t
        .ok(
            "POST",
            &path,
            Some(DISPATCH_MAC),
            Some(json!({
                "events": [
                    { "seq": 1, "kind": "phase", "summary": "starting", "meta": { "step": 1 } },
                    { "seq": 2, "kind": "tool", "summary": "read_file" },
                ]
            })),
        )
        .await;
    assert_eq!(first.as_array().unwrap().len(), 2);
    assert_eq!(first[0]["meta"]["step"], 1);

    // Duplicate seq is idempotent (ignored without error).
    let dup = t
        .ok(
            "POST",
            &path,
            Some(WORKER),
            Some(json!({
                "events": [
                    { "seq": 2, "kind": "tool", "summary": "duplicate" },
                    { "seq": 3, "kind": "message", "summary": "steered" },
                ]
            })),
        )
        .await;
    assert_eq!(dup.as_array().unwrap().len(), 1);
    assert_eq!(dup[0]["seq"], 3);
    assert_eq!(dup[0]["kind"], "message");

    // Push past the 200-event cap across batches: oldest rows are pruned atomically, keeping
    // exactly the latest 200 (seqs 51..=250).
    let batch_a: Vec<Value> = (4..=150)
        .map(|i| json!({ "seq": i, "kind": "tool", "summary": format!("step {i}") }))
        .collect();
    t.ok(
        "POST",
        &path,
        Some(DISPATCH_MAC),
        Some(json!({ "events": batch_a })),
    )
    .await;

    let batch_b: Vec<Value> = (151..=250)
        .map(|i| json!({ "seq": i, "kind": "note", "summary": format!("step {i}") }))
        .collect();
    let (s, _) = t
        .as_user("POST", &path, Some(json!({ "events": batch_b })))
        .await;
    assert_eq!(s.as_u16(), 200);

    let all = t.ok("GET", &path, None, None).await;
    let arr = all.as_array().unwrap();
    assert_eq!(arr.len(), RUN_EVENTS_CAP);
    assert_eq!(arr.first().unwrap()["seq"], 51);
    assert_eq!(arr.last().unwrap()["seq"], 250);

    // Pagination with after_seq + limit.
    let page = t
        .ok("GET", &format!("{path}?after_seq=245&limit=3"), None, None)
        .await;
    let seqs: Vec<i64> = page
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(seqs, vec![246, 247, 248]);

    // Unknown run -> 404 not_found.
    let (code, err, _) = t
        .fail("GET", "/v1/runs/run-missing/events", None, None)
        .await;
    assert_eq!((code, err.as_str()), (404, "not_found"));
}

#[tokio::test]
async fn run_events_sse_streams_backlog_resumes_from_last_event_id_and_delivers_live() {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    let run = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "board": "factory",
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "state": "running",
            })),
        )
        .await;
    let run_id = run["id"].as_str().unwrap().to_owned();
    let other_run = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "board": "factory",
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "state": "running",
            })),
        )
        .await;
    let other_run_id = other_run["id"].as_str().unwrap().to_owned();

    t.ok(
        "POST",
        &format!("/v1/runs/{run_id}/events"),
        Some(DISPATCH_MAC),
        Some(json!({
            "events": [
                { "seq": 1, "kind": "phase", "summary": "init" },
                { "seq": 2, "kind": "tool", "summary": "grep" },
                { "seq": 3, "kind": "note", "summary": "found bug" },
            ]
        })),
    )
    .await;

    // Invalid Last-Event-ID -> 400 invalid.
    let (s, v) = t
        .send(
            "GET",
            &format!("/v1/runs/{run_id}/events"),
            &[
                ("authorization", &format!("Bearer {}", t.token)),
                ("accept", "text/event-stream"),
                ("last-event-id", "not-a-seq"),
            ],
            None,
        )
        .await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (400, Some("invalid")));

    // Open SSE stream with Last-Event-ID: 1 (wins over after_seq=0).
    let req = Request::get(format!("/v1/runs/{run_id}/events?after_seq=0"))
        .header("authorization", format!("Bearer {}", t.token))
        .header("accept", "text/event-stream")
        .header("last-event-id", "1")
        .body(Body::empty())
        .unwrap();
    let resp = t.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let mut body = resp.into_body().into_data_stream();
    let mut buf = String::new();

    let backlog = read_sse_events(&mut body, &mut buf, 2).await;
    assert_eq!(
        backlog.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(backlog[0].1["summary"], "grep");
    assert_eq!(backlog[1].1["summary"], "found bug");

    // Events on another run must NOT leak into this stream.
    t.ok(
        "POST",
        &format!("/v1/runs/{other_run_id}/events"),
        Some(DISPATCH_MAC),
        Some(json!({
            "events": [{ "seq": 1, "kind": "phase", "summary": "other run noise" }]
        })),
    )
    .await;

    // Push live events for this run and verify instant delivery.
    t.ok(
        "POST",
        &format!("/v1/runs/{run_id}/events"),
        Some(DISPATCH_MAC),
        Some(json!({
            "events": [
                { "seq": 4, "kind": "pr", "summary": "opened PR #42" },
                { "seq": 5, "kind": "done", "summary": "finished" },
            ]
        })),
    )
    .await;
    let live = read_sse_events(&mut body, &mut buf, 2).await;
    assert_eq!(
        live.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![4, 5]
    );
    assert_eq!(live[0].1["kind"], "pr");
    assert_eq!(live[1].1["kind"], "done");

    // Server shutdown cleanly terminates the SSE stream.
    t.state.shutdown();
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(chunk) = body.next().await {
            chunk.unwrap();
        }
    })
    .await;
    assert!(ended.is_ok(), "SSE stream terminates on server shutdown");
}

#[tokio::test]
async fn steer_and_cancel_endpoints_enqueue_sticky_wakes_and_cancel_queued_immediately() {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    t.register(WORKER, Some(REPO), Some("main")).await;
    let task_id = t.task(WORKER, "factory", "Steer and cancel run").await;

    // 1. Queued run: steer is refused (not active yet), cancel transitions immediately to
    //    `cancelled`, records `run_outcomes`, and emits Trail Rule `run_cancelled`.
    let queued = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "task_id": task_id,
                "kind": "command",
                "device": "mac",
                "runner": "deterministic",
                "state": "queued",
            })),
        )
        .await;
    let queued_id = queued["id"].as_str().unwrap().to_owned();

    let (code, err, _) = t
        .fail(
            "POST",
            &format!("/v1/runs/{queued_id}/steer"),
            None,
            Some(json!({ "message": "focus on tests" })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid_transition"));

    let (s, cancelled_queued) = t
        .as_user(
            "POST",
            &format!("/v1/runs/{queued_id}/cancel"),
            Some(json!({ "reason": "superseded by newer plan" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{cancelled_queued}");
    assert_eq!(cancelled_queued["state"], "cancelled");
    assert!(cancelled_queued.get("wake_id").is_none());

    let fetched_queued = t
        .ok("GET", &format!("/v1/runs/{queued_id}"), None, None)
        .await;
    assert_eq!(fetched_queued["state"], "cancelled");
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM run_outcomes WHERE run_id = '{queued_id}' AND terminal_state = 'cancelled'"
        )),
        1
    );

    // Cancelling an already-terminal run -> 400 invalid_transition.
    let (code, err, _) = t
        .fail(
            "POST",
            &format!("/v1/runs/{queued_id}/cancel"),
            None,
            Some(json!({ "reason": "twice" })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid_transition"));

    // 2. Active run: steer validates message, enqueues `kind = "steer"` sticky wake, emits
    //    Trail Rule `run_steered`; cancel enqueues `kind = "cancel"` sticky wake and emits
    //    Trail Rule `run_cancelled`.
    let active = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "task_id": task_id,
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "agent_id": WORKER,
                "state": "running",
                "conflict_keys": [{ "key": "repo:app:test", "max": 1 }],
            })),
        )
        .await;
    let active_id = active["id"].as_str().unwrap().to_owned();

    // Empty steer message -> 400 invalid.
    let (code, err, _) = t
        .fail(
            "POST",
            &format!("/v1/runs/{active_id}/steer"),
            None,
            Some(json!({ "message": "   " })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid"));

    let (s, steered) = t
        .as_user(
            "POST",
            &format!("/v1/runs/{active_id}/steer"),
            Some(json!({ "message": "please add regression tests for SSE resume" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{steered}");
    assert_eq!(steered["run_id"], active_id);
    assert_eq!(steered["device"], "mac");
    let steer_wake_id = steered["wake_id"].as_i64().unwrap();

    let (s, cancel_req) = t
        .as_user(
            "POST",
            &format!("/v1/runs/{active_id}/cancel"),
            Some(json!({ "reason": "operator abort" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{cancel_req}");
    assert_eq!(cancel_req["run_id"], active_id);
    assert_eq!(cancel_req["device"], "mac");
    assert_eq!(cancel_req["state"], "running");
    let cancel_wake_id = cancel_req["wake_id"].as_i64().unwrap();

    // Both sticky wakes appear on `GET /v1/devices/mac/wakes` in FIFO order.
    let wakes = t
        .ok("GET", "/v1/devices/mac/wakes", Some(DISPATCH_MAC), None)
        .await;
    let wake_arr = wakes.as_array().unwrap();
    assert_eq!(wake_arr.len(), 2);
    assert_eq!(wake_arr[0]["id"], steer_wake_id);
    assert_eq!(wake_arr[0]["kind"], "steer");
    assert_eq!(
        wake_arr[0]["payload"]["message"],
        "please add regression tests for SSE resume"
    );
    assert_eq!(wake_arr[1]["id"], cancel_wake_id);
    assert_eq!(wake_arr[1]["kind"], "cancel");
    assert_eq!(wake_arr[1]["payload"]["reason"], "operator abort");

    // Verify Trail Rule events (`run_steered` and `run_cancelled`) were emitted.
    let events = t.ok("GET", "/v1/events?board=factory", None, None).await;
    let evs = events["events"].as_array().unwrap();
    let steered_ev = evs
        .iter()
        .find(|e| e["type"] == "run_steered" && e["payload"]["run_id"] == active_id)
        .expect("run_steered Trail Rule event");
    assert_eq!(steered_ev["payload"]["wake_id"], steer_wake_id);
    assert_eq!(
        steered_ev["payload"]["message"],
        "please add regression tests for SSE resume"
    );

    let cancel_ev = evs
        .iter()
        .find(|e| e["type"] == "run_cancelled" && e["payload"]["run_id"] == active_id)
        .expect("run_cancelled Trail Rule event");
    assert_eq!(cancel_ev["payload"]["wake_id"], cancel_wake_id);
    assert_eq!(cancel_ev["payload"]["reason"], "operator abort");
    assert_eq!(cancel_ev["actor"], "user");
}

#[tokio::test]
async fn sweeper_prunes_run_events_after_archive_grace_and_terminal_retention() {
    let t = T::new();
    t.board("factory", &[REPO]).await;

    // 1. Active run (never pruned).
    let active = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "board": "factory",
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "state": "running",
            })),
        )
        .await;
    let active_id = active["id"].as_str().unwrap().to_owned();

    // 2. Terminal archived run (pruned 1 hour after transcript_uri recorded).
    let archived = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "board": "factory",
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "state": "running",
            })),
        )
        .await;
    let archived_id = archived["id"].as_str().unwrap().to_owned();

    // 3. Terminal unarchived run (pruned 7 days after ended_at).
    let unarchived = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "board": "factory",
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "state": "running",
            })),
        )
        .await;
    let unarchived_id = unarchived["id"].as_str().unwrap().to_owned();

    for rid in [&active_id, &archived_id, &unarchived_id] {
        t.ok(
            "POST",
            &format!("/v1/runs/{rid}/events"),
            Some(DISPATCH_MAC),
            Some(json!({
                "events": [{ "seq": 1, "kind": "phase", "summary": "ran" }]
            })),
        )
        .await;
    }

    t.ok(
        "PATCH",
        &format!("/v1/runs/{archived_id}"),
        Some(DISPATCH_MAC),
        Some(json!({
            "state": "succeeded",
            "transcript_uri": "s3://archive-bucket/runs/factory/archived.jsonl",
            "transcript_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        })),
    )
    .await;
    t.ok(
        "PATCH",
        &format!("/v1/runs/{unarchived_id}"),
        Some(DISPATCH_MAC),
        Some(json!({ "state": "failed" })),
    )
    .await;

    // 30 minutes later: nothing pruned yet.
    t.advance(TimeDelta::minutes(30));
    let removed = t
        .state
        .write(|cx| crate::ops::runs::prune_run_events(cx))
        .await
        .unwrap();
    assert_eq!(removed, 0);

    // 1 hour + 1 second after archival: archived run's events are pruned; active & unarchived remain.
    t.advance(TimeDelta::minutes(31));
    let removed = t
        .state
        .write(|cx| crate::ops::runs::prune_run_events(cx))
        .await
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(
        t.ok("GET", &format!("/v1/runs/{archived_id}/events"), None, None)
            .await
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        t.ok(
            "GET",
            &format!("/v1/runs/{unarchived_id}/events"),
            None,
            None
        )
        .await
        .as_array()
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        t.ok("GET", &format!("/v1/runs/{active_id}/events"), None, None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // 7 days later: unarchived terminal run's events are pruned; active run's events still remain.
    t.advance(TimeDelta::days(7));
    let removed = t
        .state
        .write(|cx| crate::ops::runs::prune_run_events(cx))
        .await
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(
        t.ok(
            "GET",
            &format!("/v1/runs/{unarchived_id}/events"),
            None,
            None
        )
        .await
        .as_array()
        .unwrap()
        .len(),
        0
    );
    assert_eq!(
        t.ok("GET", &format!("/v1/runs/{active_id}/events"), None, None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
