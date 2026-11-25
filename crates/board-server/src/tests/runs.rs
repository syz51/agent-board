//! Unified runs, state machine transitions, atomic `conflict_locks`, injectable-clock
//! lease-expiry orphan sweeper, cross-device dispatcher-agnostic judge/merger rights, and
//! standing `review: pass` verification (`docs/SCHEDULER_DESIGN.md`, Section 3.3).

use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{REPO, T, check};

const WORKER: &str = "t3:worker";
const REVIEWER: &str = "t3:reviewer";
const MERGER: &str = "t3:merger";
const OUTSIDER: &str = "t3:outsider";
const DISPATCH_MAC: &str = "dispatch:mac";
const DISPATCH_LINUX: &str = "dispatch:linux-runner";

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

#[tokio::test]
async fn runs_crud_auth_filtering_and_open_enum_validation() {
    let t = T::new();
    let linux_token = create_device(&t, "linux-runner").await;
    t.board("factory", &[REPO]).await;
    t.board("other", &[]).await;
    t.register(WORKER, Some(REPO), Some("main")).await;
    let worker_handle = t
        .ok("GET", &format!("/v1/agents/{WORKER}"), None, None)
        .await["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    let task_id = t.task(WORKER, "factory", "Implement unified runs").await;

    // Device token (`mac`) cannot create a run for another device (`linux-runner`).
    let (code, err, v) = t
        .fail(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "kind": "worker",
                "device": "linux-runner",
                "runner": "acp",
            })),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // Mismatched task_id and board -> 400 invalid.
    let (code, err, v) = t
        .fail(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "task_id": task_id,
                "board": "other",
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
            })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");

    // Unknown open enums are refused with 400 invalid on create, filter, and patch.
    for bad in [
        json!({ "kind": "quantum", "device": "mac", "runner": "acp" }),
        json!({ "kind": "worker", "origin": "cron", "device": "mac", "runner": "acp" }),
        json!({ "kind": "worker", "state": "teleporting", "device": "mac", "runner": "acp" }),
        json!({ "kind": "worker", "state": "succeeded", "device": "mac", "runner": "acp" }),
        json!({ "kind": "worker", "state": "queued", "device": "mac", "runner": "acp", "lease_ttl_secs": 60 }),
    ] {
        let (code, err, v) = t.fail("POST", "/v1/runs", None, Some(bad)).await;
        assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");
    }
    for query in [
        "/v1/runs?kind=quantum",
        "/v1/runs?origin=cron",
        "/v1/runs?state=teleporting",
    ] {
        let (code, err, v) = t.fail("GET", query, None, None).await;
        assert_eq!((code, err.as_str()), (400, "invalid"), "{query}: {v}");
    }

    // An ordinary worker agent cannot create a managed run (prevents self-escalation to
    // managed judge/merger rights), while an interactive run is allowed.
    let (code, err, v) = t
        .fail(
            "POST",
            "/v1/runs",
            Some(WORKER),
            Some(json!({
                "task_id": task_id,
                "kind": "merge",
                "origin": "managed",
                "device": "mac",
                "runner": "acp",
                "agent_id": WORKER,
            })),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // Create a managed worker run on `mac` as `dispatch:mac` (board derived automatically
    // from task_id, agent_id resolved from short handle, policy_version trimmed, default 3m lease).
    let run1 = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "task_id": task_id,
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "agent_id": worker_handle,
                "worktree_branch": "task/factory-1",
                "cwd": "/tmp/wt",
                "title": "Worker run 1",
                "cost_units": 2,
                "policy_version": " v1\n"
            })),
        )
        .await;
    let run1_id = run1["id"].as_str().unwrap().to_owned();
    assert!(run1_id.starts_with("run-"), "{run1_id}");
    assert_eq!(run1["board"], "factory");
    assert_eq!(run1["task_id"], task_id);
    assert_eq!(run1["origin"], "managed");
    assert_eq!(run1["state"], "leased");
    assert_eq!(run1["agent_id"], WORKER);
    assert_eq!(run1["cost_units"], 2);
    assert_eq!(run1["policy_version"], "v1");
    assert!(run1["lease_expires_at"].is_string());
    assert!(run1.get("started_at").is_none());

    // An ordinary worker agent cannot PATCH a managed run even on the same device.
    let (code, err, v) = t
        .fail(
            "PATCH",
            &format!("/v1/runs/{run1_id}"),
            Some(WORKER),
            Some(json!({ "state": "starting" })),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // Advance clock 1s and create a second interactive run on `linux-runner` via user token.
    t.advance(TimeDelta::seconds(1));
    let (s, run2) = t
        .as_user(
            "POST",
            "/v1/runs",
            Some(json!({
                "board": "other",
                "kind": "interactive",
                "origin": "interactive",
                "device": "linux-runner",
                "runner": "tmux",
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{run2}");
    let run2_id = run2["id"].as_str().unwrap().to_owned();
    assert_eq!(run2["state"], "running");
    assert!(run2.get("lease_expires_at").is_none());
    assert!(run2["started_at"].is_string());

    // GET /v1/runs/{id}
    let fetched = t
        .ok("GET", &format!("/v1/runs/{run1_id}"), None, None)
        .await;
    assert_eq!(fetched, run1);

    // GET /v1/runs lists newest first and supports all query filters.
    let all = t.ok("GET", "/v1/runs", None, None).await;
    let ids: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [run2_id.as_str(), run1_id.as_str()]);

    let by_board = t.ok("GET", "/v1/runs?board=factory", None, None).await;
    assert_eq!(by_board.as_array().unwrap().len(), 1);
    assert_eq!(by_board[0]["id"], run1_id);

    let by_task = t
        .ok("GET", &format!("/v1/runs?task={task_id}"), None, None)
        .await;
    assert_eq!(by_task.as_array().unwrap().len(), 1);
    assert_eq!(by_task[0]["id"], run1_id);

    let by_device = t
        .ok("GET", "/v1/runs?device=linux-runner", None, None)
        .await;
    assert_eq!(by_device.as_array().unwrap().len(), 1);
    assert_eq!(by_device[0]["id"], run2_id);

    let by_handle = t
        .ok(
            "GET",
            &format!("/v1/runs?agent={worker_handle}"),
            None,
            None,
        )
        .await;
    assert_eq!(by_handle.as_array().unwrap().len(), 1);
    assert_eq!(by_handle[0]["id"], run1_id);

    let by_origin = t
        .ok("GET", "/v1/runs?origin=interactive&active=true", None, None)
        .await;
    assert_eq!(by_origin.as_array().unwrap().len(), 1);
    assert_eq!(by_origin[0]["id"], run2_id);

    let limited = t.ok("GET", "/v1/runs?limit=1", None, None).await;
    assert_eq!(limited.as_array().unwrap().len(), 1);
    assert_eq!(limited[0]["id"], run2_id);

    // Device `linux-runner` cannot PATCH `mac`'s run (403 forbidden).
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "PATCH",
        &format!("/v1/runs/{run1_id}"),
        None,
        Some(json!({ "state": "starting" })),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");

    // Unknown state on PATCH -> 400 invalid.
    let (code, err, v) = t
        .fail(
            "PATCH",
            &format!("/v1/runs/{run1_id}"),
            None,
            Some(json!({ "state": "future_state" })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");
}

#[tokio::test]
async fn run_state_machine_transitions_archival_and_rollback_safety() {
    let t = T::new();
    t.board("factory", &[REPO]).await;

    // Queued -> cannot jump directly to running; can go queued -> leased -> starting ->
    // running <-> waiting_external -> succeeded.
    let q = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "board": "factory",
                "kind": "command",
                "device": "mac",
                "runner": "deterministic",
                "state": "queued"
            })),
        )
        .await;
    let id = q["id"].as_str().unwrap().to_owned();
    assert_eq!(q["state"], "queued");
    assert!(q.get("lease_expires_at").is_none());

    let (code, err, v) = t
        .fail(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid_transition"), "{v}");

    // queued -> leased grants default 3m lease.
    let leased = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "state": "leased" })),
        )
        .await;
    assert_eq!(leased["state"], "leased");
    assert!(leased["lease_expires_at"].is_string());

    // leased -> succeeded is forbidden.
    let (code, err, _) = t
        .fail(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid_transition"));

    // leased -> starting -> running -> waiting_external -> running -> succeeded.
    let starting = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "state": "starting" })),
        )
        .await;
    assert_eq!(starting["state"], "starting");
    assert!(starting.get("started_at").is_none());

    // starting -> succeeded is forbidden.
    let (code, err, _) = t
        .fail(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid_transition"));

    let running = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await;
    assert_eq!(running["state"], "running");
    assert!(running["started_at"].is_string());

    let waiting = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "state": "waiting_external" })),
        )
        .await;
    assert_eq!(waiting["state"], "waiting_external");

    let resumed = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await;
    assert_eq!(resumed["state"], "running");

    // Pure lease renewal does not spam the event log.
    let seq_before = t.count("SELECT MAX(seq) FROM events");
    t.advance(TimeDelta::seconds(30));
    let renewed = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "lease_ttl_secs": 240 })),
        )
        .await;
    assert_ne!(renewed["lease_expires_at"], resumed["lease_expires_at"]);
    assert_eq!(t.count("SELECT MAX(seq) FROM events"), seq_before);

    let done = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({
                "state": "succeeded",
                "outcome": { "exit_code": 0 }
            })),
        )
        .await;
    assert_eq!(done["state"], "succeeded");
    assert!(done["ended_at"].is_string());
    assert!(done.get("lease_expires_at").is_none());
    assert_eq!(done["outcome"], json!({ "exit_code": 0 }));

    // active=true excludes terminal runs; active=false includes them.
    assert!(
        t.ok("GET", "/v1/runs?active=true", None, None)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        t.ok("GET", "/v1/runs?active=false", None, None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Terminal run cannot transition or renew lease.
    for bad in [
        json!({ "state": "running" }),
        json!({ "state": "failed" }),
        json!({ "state": "succeeded" }),
        json!({ "lease_ttl_secs": 120 }),
    ] {
        let (code, err, v) = t
            .fail("PATCH", &format!("/v1/runs/{id}"), None, Some(bad))
            .await;
        assert_eq!((code, err.as_str()), (400, "invalid_transition"), "{v}");
    }

    // Post-terminal transcript archival upload succeeds without altering terminal state
    // and stores trimmed token values.
    let archived = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({
                "transcript_uri": "s3://archive-bucket/runs/factory/run-1.tar.zst",
                "transcript_sha256": "\tdeadbeef0123456789\r\n"
            })),
        )
        .await;
    assert_eq!(archived["state"], "succeeded");
    assert_eq!(
        archived["transcript_uri"],
        "s3://archive-bucket/runs/factory/run-1.tar.zst"
    );
    assert_eq!(archived["transcript_sha256"], "deadbeef0123456789");

    // Rollback safety: an unknown future state stored in SQLite is preserved on partial
    // metadata PATCH and refused on state transition, lease renewal, or lock acquisition.
    t.exec(&format!(
        "UPDATE runs SET state = 'draining_future' WHERE id = '{id}'"
    ));
    for bad in [
        json!({ "state": "cancelled" }),
        json!({ "lease_ttl_secs": 120 }),
        json!({ "conflict_keys": [{ "key": "repo:example/app:write", "max": 1 }] }),
    ] {
        let (code, err, v) = t
            .fail("PATCH", &format!("/v1/runs/{id}"), None, Some(bad))
            .await;
        assert_eq!((code, err.as_str()), (400, "invalid_transition"), "{v}");
    }
    assert_eq!(t.count("SELECT COUNT(*) FROM conflict_locks"), 0);
    let patched = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "title": "Updated title" })),
        )
        .await;
    assert_eq!(patched["state"], "unknown");
    assert_eq!(
        t.strings(&format!("SELECT state FROM runs WHERE id = '{id}'")),
        ["draining_future"]
    );
}

#[tokio::test]
async fn conflict_locks_enforce_atomic_contention_and_release_on_terminal_state() {
    let t = T::new();
    let linux_token = create_device(&t, "linux-runner").await;
    t.board("factory", &[REPO]).await;

    let merge_key = format!("repo:{REPO}:merge");
    let write_key = format!("repo:{REPO}:write");

    // Run 1 acquires exclusive merge lock (max=1) and shared write lock (max=2).
    let run1 = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "board": "factory",
                "kind": "merge",
                "device": "mac",
                "runner": "deterministic",
                "conflict_keys": [
                    { "key": merge_key, "max": 1 },
                    { "key": write_key, "max": 2 }
                ]
            })),
        )
        .await;
    let run1_id = run1["id"].as_str().unwrap().to_owned();
    assert_eq!(run1["conflict_locks"].as_array().unwrap().len(), 2);

    // Run 2 on another device (acting as dispatch:linux-runner) requests both `write_key`
    // (available) AND `merge_key` (at capacity 1/1): entire request rolls back atomically
    // with 409 claim_refused, and expected lock contention is never recorded in api_errors!
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "POST",
        "/v1/runs",
        Some(DISPATCH_LINUX),
        Some(json!({
            "board": "factory",
            "kind": "merge",
            "device": "linux-runner",
            "runner": "deterministic",
            "conflict_keys": [
                { "key": write_key, "max": 2 },
                { "key": merge_key, "max": 1 }
            ]
        })),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    assert!(v["message"].as_str().unwrap().contains(&merge_key), "{v}");
    assert!(v["next"].as_str().unwrap().contains(&run1_id), "{v}");
    assert_eq!(t.count("SELECT COUNT(*) FROM runs"), 1, "rolled back");
    assert_eq!(
        t.count("SELECT COUNT(*) FROM conflict_locks"),
        2,
        "no leaked locks from refused run"
    );
    assert_eq!(
        t.count("SELECT COUNT(*) FROM api_errors"),
        0,
        "expected conflict lock 409 must not record as signal friction"
    );

    // Run 2 requesting only `write_key` (max=2) succeeds (now 2/2).
    let (s, run2) = send_as_device(
        &t,
        &linux_token,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "board": "factory",
            "kind": "worker",
            "device": "linux-runner",
            "runner": "acp",
            "conflict_keys": [{ "key": write_key, "max": 2 }]
        })),
    )
    .await;
    assert_eq!(s, 200, "{run2}");
    let run2_id = run2["id"].as_str().unwrap().to_owned();

    // Run 3 requesting `write_key` is refused (2/2 full).
    let (code, err, _) = t
        .fail(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "board": "factory",
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "conflict_keys": [{ "key": write_key, "max": 2 }]
            })),
        )
        .await;
    assert_eq!((code, err.as_str()), (409, "claim_refused"));

    // Cancelling Run 1 atomically releases both its locks and emits `run_cancelled`.
    let cancelled = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{run1_id}"),
            None,
            Some(json!({ "state": "cancelled", "reason": "operator cancelled" })),
        )
        .await;
    assert_eq!(cancelled["state"], "cancelled");
    assert!(cancelled.get("conflict_locks").is_none());

    let events = t
        .ok("GET", "/v1/events?board=factory&since=0", None, None)
        .await["events"]
        .as_array()
        .unwrap()
        .clone();
    let cancel_ev = events
        .iter()
        .find(|e| e["type"] == "run_cancelled")
        .expect("run_cancelled event emitted");
    assert_eq!(cancel_ev["payload"]["run_id"], run1_id);
    assert_eq!(cancel_ev["payload"]["reason"], "operator cancelled");
    assert_eq!(
        cancel_ev["payload"]["released_locks"],
        json!([merge_key, write_key])
    );

    // Now a new merge run on `linux-runner` can acquire `merge_key`!
    let (s, run3) = send_as_device(
        &t,
        &linux_token,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "board": "factory",
            "kind": "merge",
            "device": "linux-runner",
            "runner": "deterministic",
            "conflict_keys": [{ "key": merge_key, "max": 1 }]
        })),
    )
    .await;
    assert_eq!(s, 200, "{run3}");
    assert_ne!(run3["id"], run2_id);
}

#[tokio::test]
async fn sweeper_orphans_expired_managed_runs_releases_locks_and_writes_trail_rule_event() {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    t.register(WORKER, Some(REPO), Some("task/factory-1")).await;
    let task_id = t.task(WORKER, "factory", "Long running build").await;

    let lock_key = format!("repo:{REPO}:write");
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
                "worktree_branch": "task/factory-1",
                "conflict_keys": [{ "key": lock_key, "max": 1 }]
            })),
        )
        .await;
    let run_id = run["id"].as_str().unwrap().to_owned();
    let expected_expiry = run["lease_expires_at"].clone();

    // Also create a queued run and an interactive run: neither should be swept.
    let queued = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "board": "factory",
                "kind": "command",
                "device": "mac",
                "runner": "deterministic",
                "state": "queued"
            })),
        )
        .await;
    let interactive = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "board": "factory",
                "kind": "interactive",
                "origin": "interactive",
                "device": "mac",
                "runner": "t3"
            })),
        )
        .await;

    // At +179s (1s before 3m TTL): nothing swept.
    t.advance(TimeDelta::seconds(179));
    assert_eq!(t.sweep_runs(), 0);
    assert_eq!(
        t.ok("GET", &format!("/v1/runs/{run_id}"), None, None).await["state"],
        "running"
    );

    // At +181s (1s past expiry): managed run transitions atomically to `orphaned`,
    // releases its conflict lock, and writes a Trail Rule `run_orphaned` event.
    t.advance(TimeDelta::seconds(2));
    assert_eq!(t.sweep_runs(), 1);

    let after = t.ok("GET", &format!("/v1/runs/{run_id}"), None, None).await;
    assert_eq!(after["state"], "orphaned");
    assert!(after["ended_at"].is_string());
    assert!(after.get("lease_expires_at").is_none());
    assert!(after.get("conflict_locks").is_none());
    assert_eq!(t.count("SELECT COUNT(*) FROM conflict_locks"), 0);

    // Queued and interactive runs stay untouched.
    assert_eq!(
        t.ok(
            "GET",
            &format!("/v1/runs/{}", queued["id"].as_str().unwrap()),
            None,
            None
        )
        .await["state"],
        "queued"
    );
    assert_eq!(
        t.ok(
            "GET",
            &format!("/v1/runs/{}", interactive["id"].as_str().unwrap()),
            None,
            None
        )
        .await["state"],
        "running"
    );

    // Verify Trail Rule `run_orphaned` event carries device, branch, task, cause, expiry,
    // and released locks.
    let events = t
        .ok("GET", "/v1/events?board=factory&since=0", None, None)
        .await["events"]
        .as_array()
        .unwrap()
        .clone();
    let orphan = events
        .iter()
        .find(|e| e["type"] == "run_orphaned")
        .expect("run_orphaned Trail Rule event");
    assert_eq!(orphan["actor"], "server");
    assert_eq!(orphan["task"], task_id);
    assert_eq!(orphan["payload"]["run_id"], run_id);
    assert_eq!(orphan["payload"]["device"], "mac");
    assert_eq!(orphan["payload"]["worktree_branch"], "task/factory-1");
    assert_eq!(orphan["payload"]["cause"], "lease_expired");
    assert_eq!(orphan["payload"]["lease_expires_at"], expected_expiry);
    assert_eq!(orphan["payload"]["expired_by_ms"], 1000);
    assert_eq!(orphan["payload"]["released_locks"], json!([lock_key]));

    // Sweeping again is idempotent (0 swept).
    assert_eq!(t.sweep_runs(), 0);
}

#[tokio::test]
async fn live_judge_and_merger_runs_grant_coordinator_rights_across_devices_without_board_lease() {
    let t = T::new();
    let linux_token = create_device(&t, "linux-runner").await;
    t.board("factory", &[REPO]).await;
    t.board("other", &[]).await;

    // Register worker on `mac`, reviewer & merger on `linux-runner`. No board holds any
    // coordinator lease!
    t.register(WORKER, Some(REPO), None).await;
    for agent in [REVIEWER, MERGER, OUTSIDER] {
        let (s, v) = send_as_device(
            &t,
            &linux_token,
            "POST",
            "/v1/agents",
            Some(agent),
            Some(json!({ "id": agent, "runtime": "claude", "device": "linux-runner" })),
        )
        .await;
        assert_eq!(s, 200, "{v}");
    }

    let task_id = t.task(WORKER, "factory", "Cross-device work").await;
    let other_task = t.task(WORKER, "other", "Other board work").await;
    assert_eq!(t.claim(WORKER, &task_id).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &task_id, json!({ "status": "review" }))
            .await
            .0,
        200
    );
    assert_eq!(t.claim(WORKER, &other_task).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &other_task, json!({ "status": "review" }))
            .await
            .0,
        200
    );

    // Without a Run or coordinator lease, MERGER and REVIEWER cannot hand over `task_id`.
    let version = t.get_task(&task_id).await["version"].clone();
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "PATCH",
        &format!("/v1/tasks/{task_id}"),
        Some(MERGER),
        Some(json!({ "agent": MERGER, "version": version, "owner": MERGER })),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");

    // A `worker` run does NOT grant coordinator rights.
    let (s, worker_run) = send_as_device(
        &t,
        &linux_token,
        "POST",
        "/v1/runs",
        Some(DISPATCH_LINUX),
        Some(json!({
            "board": "factory",
            "kind": "worker",
            "device": "linux-runner",
            "runner": "acp",
            "agent_id": OUTSIDER,
            "state": "running"
        })),
    )
    .await;
    assert_eq!(s, 200, "{worker_run}");
    let version = t.get_task(&task_id).await["version"].clone();
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "PATCH",
        &format!("/v1/tasks/{task_id}"),
        Some(OUTSIDER),
        Some(json!({ "agent": OUTSIDER, "version": version, "owner": OUTSIDER })),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");

    // Start a live `review` Run on `linux-runner` for `factory` (still NO coordinator lease).
    let (s, review_run) = send_as_device(
        &t,
        &linux_token,
        "POST",
        "/v1/runs",
        Some(DISPATCH_LINUX),
        Some(json!({
            "task_id": task_id,
            "kind": "review",
            "device": "linux-runner",
            "runner": "acp",
            "agent_id": REVIEWER,
            "state": "running"
        })),
    )
    .await;
    assert_eq!(s, 200, "{review_run}");
    let review_run_id = review_run["id"].as_str().unwrap().to_owned();

    // REVIEWER now has coordinator rights on `factory` (can move task to blocked, then back
    // to review, and its `said` is kept), but NOT on `other`.
    let version = t.get_task(&task_id).await["version"].clone();
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "PATCH",
        &format!("/v1/tasks/{task_id}"),
        Some(REVIEWER),
        Some(json!({
            "agent": REVIEWER,
            "version": version,
            "status": "blocked",
            "blocked_reason": "needs rebase"
        })),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let version = v["version"].clone();
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "PATCH",
        &format!("/v1/tasks/{task_id}"),
        Some(REVIEWER),
        Some(json!({ "agent": REVIEWER, "version": version, "status": "review" })),
    )
    .await;
    assert_eq!(s, 200, "{v}");

    // Rights do not leak to `other`.
    let other_ver = t.get_task(&other_task).await["version"].clone();
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "PATCH",
        &format!("/v1/tasks/{other_task}"),
        Some(REVIEWER),
        Some(json!({ "agent": REVIEWER, "version": other_ver, "owner": REVIEWER })),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");

    // Turn ending (`said`) is recorded for a live judge run.
    let (s, _) = send_as_device(
        &t,
        &linux_token,
        "POST",
        &format!("/v1/agents/{REVIEWER}/state"),
        Some(REVIEWER),
        Some(json!({ "state": "idle", "paused_until": null, "said": "review done" })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, said) = t
        .as_user("GET", &format!("/v1/agents/{REVIEWER}/said"), None)
        .await;
    assert_eq!(s.as_u16(), 200);
    assert_eq!(said[0]["board"], "factory");
    assert_eq!(said[0]["text"], "review done");

    // Once the review run ends (`succeeded`), REVIEWER immediately loses coordinator rights.
    let (s, _) = send_as_device(
        &t,
        &linux_token,
        "PATCH",
        &format!("/v1/runs/{review_run_id}"),
        Some(DISPATCH_LINUX),
        Some(json!({ "state": "succeeded" })),
    )
    .await;
    assert_eq!(s, 200);
    let version = t.get_task(&task_id).await["version"].clone();
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "PATCH",
        &format!("/v1/tasks/{task_id}"),
        Some(REVIEWER),
        Some(json!({ "agent": REVIEWER, "version": version, "owner": REVIEWER })),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");

    // Start a live `merge` Run on `linux-runner`: MERGER hands `task_id` to itself and
    // marks it done without any coordinator lease!
    let (s, merge_run) = send_as_device(
        &t,
        &linux_token,
        "POST",
        "/v1/runs",
        Some(DISPATCH_LINUX),
        Some(json!({
            "task_id": task_id,
            "kind": "merge",
            "device": "linux-runner",
            "runner": "deterministic",
            "agent_id": MERGER,
            "state": "running"
        })),
    )
    .await;
    assert_eq!(s, 200, "{merge_run}");

    let version = t.get_task(&task_id).await["version"].clone();
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "PATCH",
        &format!("/v1/tasks/{task_id}"),
        Some(MERGER),
        Some(json!({ "agent": MERGER, "version": version, "owner": MERGER })),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let version = v["version"].clone();
    let (s, done) = send_as_device(
        &t,
        &linux_token,
        "POST",
        &format!("/v1/tasks/{task_id}/done"),
        Some(MERGER),
        Some(json!({ "agent": MERGER, "version": version, "evidence": check(0) })),
    )
    .await;
    assert_eq!(s, 200, "{done}");
    assert_eq!(done["status"], "done");

    // Once the merge run's lease expires (even before the sweeper runs), rights lapse.
    let task2 = t.task(WORKER, "factory", "Second cross-device task").await;
    assert_eq!(t.claim(WORKER, &task2).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &task2, json!({ "status": "review" }))
            .await
            .0,
        200
    );
    t.advance(TimeDelta::seconds(181));
    let version = t.get_task(&task2).await["version"].clone();
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "PATCH",
        &format!("/v1/tasks/{task2}"),
        Some(MERGER),
        Some(json!({ "agent": MERGER, "version": version, "owner": MERGER })),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
}

#[tokio::test]
async fn standing_review_pass_verifies_registered_review_run_across_devices_and_rejects_fail() {
    let t = T::new();
    let linux_token = create_device(&t, "linux-runner").await;
    t.board("factory", &[REPO]).await;
    t.register(WORKER, Some(REPO), None).await;
    for agent in [REVIEWER, OUTSIDER] {
        let (s, v) = send_as_device(
            &t,
            &linux_token,
            "POST",
            "/v1/agents",
            Some(agent),
            Some(json!({ "id": agent, "runtime": "claude", "device": "linux-runner" })),
        )
        .await;
        assert_eq!(s, 200, "{v}");
    }

    let task_id = t.task(WORKER, "factory", "Verify standing review").await;
    assert_eq!(t.claim(WORKER, &task_id).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &task_id, json!({ "status": "review" }))
            .await
            .0,
        200
    );

    // No review note yet -> no standing pass.
    assert_eq!(t.standing_review_pass(&task_id), None);

    // Self-review by task owner does NOT count.
    t.ok(
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(WORKER),
        Some(json!({ "agent": WORKER, "kind": "decision", "text": "review: pass. LGTM" })),
    )
    .await;
    assert_eq!(t.standing_review_pass(&task_id), None);

    // Unregistered outsider writing `review: pass` does NOT count.
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(OUTSIDER),
        Some(
            json!({ "agent": OUTSIDER, "kind": "decision", "text": "review: pass. Unauthorized" }),
        ),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(t.standing_review_pass(&task_id), None);

    // Register a `review` Run on `linux-runner` (different device from `mac` worker).
    let (s, review_run) = send_as_device(
        &t,
        &linux_token,
        "POST",
        "/v1/runs",
        Some(DISPATCH_LINUX),
        Some(json!({
            "task_id": task_id,
            "kind": "review",
            "device": "linux-runner",
            "runner": "acp",
            "agent_id": REVIEWER,
            "state": "running"
        })),
    )
    .await;
    assert_eq!(s, 200, "{review_run}");
    let review_run_id = review_run["id"].as_str().unwrap().to_owned();

    // Clean `review: fail` with Run transitioning to `succeeded` MUST NOT count as pass!
    let (s, v) = send_as_device(
        &t,
        &linux_token,
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(REVIEWER),
        Some(json!({
            "agent": REVIEWER,
            "kind": "decision",
            "text": "review: fail: missing edge-case test"
        })),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, _) = send_as_device(
        &t,
        &linux_token,
        "PATCH",
        &format!("/v1/runs/{review_run_id}"),
        Some(DISPATCH_LINUX),
        Some(json!({ "state": "succeeded" })),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.standing_review_pass(&task_id),
        None,
        "clean review: fail with state=succeeded must never count as standing pass"
    );

    // Worker fixes and re-parks in `review`; second review run posts `review: pass`.
    assert_eq!(
        t.patch(WORKER, &task_id, json!({ "status": "doing" }))
            .await
            .0,
        200
    );
    assert_eq!(
        t.patch(WORKER, &task_id, json!({ "status": "review" }))
            .await
            .0,
        200
    );
    let (s, pass_note) = send_as_device(
        &t,
        &linux_token,
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(REVIEWER),
        Some(json!({
            "agent": REVIEWER,
            "kind": "decision",
            "text": "review: pass. All checks verified.\npr: none"
        })),
    )
    .await;
    assert_eq!(s, 200, "{pass_note}");
    let standing = t
        .standing_review_pass(&task_id)
        .expect("registered review Run pass stands across devices");
    assert_eq!(standing.agent.as_str(), REVIEWER);
    assert_eq!(standing.id, pass_note["id"].as_i64().unwrap());

    // Moving the task out of `review` and back into `review` invalidates the older pass!
    assert_eq!(
        t.patch(WORKER, &task_id, json!({ "status": "doing" }))
            .await
            .0,
        200
    );
    assert_eq!(t.standing_review_pass(&task_id), None);
    assert_eq!(
        t.patch(WORKER, &task_id, json!({ "status": "review" }))
            .await
            .0,
        200
    );
    assert_eq!(
        t.standing_review_pass(&task_id),
        None,
        "older review: pass before latest move to review must not stand"
    );

    // New `review: pass` stands, then `review: void` withdraws it.
    let (s, _) = send_as_device(
        &t,
        &linux_token,
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(REVIEWER),
        Some(json!({ "agent": REVIEWER, "kind": "decision", "text": "decision: review: pass. Ready" })),
    )
    .await;
    assert_eq!(s, 200);
    assert!(t.standing_review_pass(&task_id).is_some());

    let (s, _) = send_as_device(
        &t,
        &linux_token,
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(REVIEWER),
        Some(json!({ "agent": REVIEWER, "kind": "decision", "text": "review: void: PR closed" })),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(t.standing_review_pass(&task_id), None);
}

#[tokio::test]
async fn standing_review_pass_preserves_backward_compatibility_with_legacy_judges() {
    let t = T::new();
    const DISPATCH: &str = "dispatch:mac";
    const LEGACY_JUDGE: &str = "t3:legacy-judge";

    t.board("factory", &[REPO]).await;
    t.ok(
        "PATCH",
        "/v1/boards/factory",
        None,
        Some(json!({ "autopilot": true })),
    )
    .await;
    t.register(DISPATCH, None, None).await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(LEGACY_JUDGE, None, None).await;
    t.ok(
        "POST",
        "/v1/boards/factory/coordinator",
        Some(DISPATCH),
        Some(json!({ "device": "mac", "agent": DISPATCH })),
    )
    .await;

    let task_id = t.task(WORKER, "factory", "Legacy review task").await;
    assert_eq!(t.claim(WORKER, &task_id).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &task_id, json!({ "status": "review" }))
            .await
            .0,
        200
    );

    // Register via legacy POST /v1/boards/{slug}/judges.
    t.ok(
        "POST",
        "/v1/boards/factory/judges",
        Some(DISPATCH),
        Some(json!({ "agent": LEGACY_JUDGE, "kind": "review", "tasks": [task_id] })),
    )
    .await;
    t.ok(
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(LEGACY_JUDGE),
        Some(json!({ "agent": LEGACY_JUDGE, "kind": "decision", "text": "review: pass. Clean" })),
    )
    .await;
    assert_eq!(
        t.standing_review_pass(&task_id)
            .expect("legacy registered judge pass stands")
            .agent
            .as_str(),
        LEGACY_JUDGE
    );

    // Even after the legacy judge ends and its row in `judges` is pruned by a later judge
    // registration, the durable `board_updated` judge event keeps the standing pass valid!
    t.ok(
        "POST",
        &format!("/v1/agents/{LEGACY_JUDGE}/state"),
        Some(LEGACY_JUDGE),
        Some(json!({ "state": "ended", "paused_until": null })),
    )
    .await;
    t.ok(
        "POST",
        "/v1/boards/factory/judges",
        Some(DISPATCH),
        Some(json!({ "agent": "t3:other-judge", "kind": "goal" })),
    )
    .await;
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM judges WHERE agent = '{LEGACY_JUDGE}'"
        )),
        0
    );
    assert!(
        t.standing_review_pass(&task_id).is_some(),
        "standing pass survives legacy judges table cleanup"
    );
}

#[tokio::test]
async fn standing_review_pass_rejects_interactive_queued_cancelled_and_like_wildcards() {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    t.register(DISPATCH_MAC, None, None).await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(OUTSIDER, None, None).await;
    const WILDCARD: &str = "t3:%";
    const WRONG_CASE: &str = "t3:Real-Judge";
    t.register(WILDCARD, None, None).await;
    t.register(WRONG_CASE, None, None).await;

    let task_id = t.task(WORKER, "factory", "Hardened review check").await;
    assert_eq!(t.claim(WORKER, &task_id).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &task_id, json!({ "status": "review" }))
            .await
            .0,
        200
    );

    // 1. Interactive run of kind=review must NOT qualify as a managed review run.
    t.ok(
        "POST",
        "/v1/runs",
        Some(OUTSIDER),
        Some(json!({
            "task_id": task_id,
            "kind": "review",
            "origin": "interactive",
            "device": "mac",
            "runner": "t3",
            "agent_id": OUTSIDER
        })),
    )
    .await;
    t.ok(
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(OUTSIDER),
        Some(json!({ "agent": OUTSIDER, "kind": "decision", "text": "review: pass. Fake" })),
    )
    .await;
    assert_eq!(t.standing_review_pass(&task_id), None);

    // 2. Managed review run cancelled directly from `queued` (never leased/started) must NOT qualify.
    let q = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "task_id": task_id,
                "kind": "review",
                "origin": "managed",
                "device": "mac",
                "runner": "acp",
                "agent_id": OUTSIDER,
                "state": "queued"
            })),
        )
        .await;
    t.ok(
        "PATCH",
        &format!("/v1/runs/{}", q["id"].as_str().unwrap()),
        Some(DISPATCH_MAC),
        Some(json!({ "state": "cancelled" })),
    )
    .await;
    assert_eq!(t.standing_review_pass(&task_id), None);

    // 3. Legacy dispatcher note ending in `(agent t3:real-judge)` must NOT match SQLite LIKE
    //    wildcard agent `t3:%` or case-variant `t3:Real-Judge`.
    t.ok(
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(DISPATCH_MAC),
        Some(json!({
            "agent": DISPATCH_MAC,
            "kind": "note",
            "text": "note: the dispatcher started a review judge for this task: Review (agent t3:real-judge)"
        })),
    )
    .await;
    for impostor in [WILDCARD, WRONG_CASE] {
        t.ok(
            "POST",
            &format!("/v1/tasks/{task_id}/notes"),
            Some(impostor),
            Some(json!({ "agent": impostor, "kind": "decision", "text": "review: pass. Spoofed" })),
        )
        .await;
        assert_eq!(
            t.standing_review_pass(&task_id),
            None,
            "{impostor} must not match legacy judge note"
        );
    }
}

#[tokio::test]
async fn end_unseen_skips_agent_with_live_managed_judge_run_until_expiry() {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, None, None).await;

    // Create a managed review run with a 2-hour lease so it outlives `owner_left_secs` (1h).
    t.ok(
        "POST",
        "/v1/runs",
        Some(DISPATCH_MAC),
        Some(json!({
            "board": "factory",
            "kind": "review",
            "device": "mac",
            "runner": "acp",
            "agent_id": REVIEWER,
            "state": "running",
            "lease_ttl_secs": 7200
        })),
    )
    .await;

    // After 61m unseen: WORKER ends, while REVIEWER is protected by its live managed run.
    t.advance(TimeDelta::minutes(61));
    assert_eq!(t.end_unseen(), 1);
    assert_eq!(
        [WORKER, REVIEWER].map(|a| t.state_of(a)),
        ["ended", "running"]
    );

    // Once the run's 2h lease expires and is swept, `end_unseen` ends REVIEWER too.
    t.advance(TimeDelta::minutes(60));
    assert_eq!(t.sweep_runs(), 1);
    assert_eq!(t.end_unseen(), 1);
    assert_eq!(t.state_of(REVIEWER), "ended");
}

#[tokio::test]
async fn waiting_external_releases_repo_write_locks_reacquires_on_resume_and_reconciles_legacy_rows()
 {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    t.register(WORKER, Some(REPO), Some("task/factory-1")).await;
    let write_key = format!("repo:{REPO}:write");
    let extra_write_key = "repo:github.com/example/other:write";
    let custom_key = "db:migrate";
    let task_id = t
        .task_with(
            WORKER,
            json!({
                "board": "factory",
                "title": "Worker waiting on ask",
                "conflict_keys": [{ "key": extra_write_key, "max": 1 }]
            }),
        )
        .await;
    let task2_id = t.task(WORKER, "factory", "Peer worker task").await;

    // 1. Managed worker run holds derived `repo:<repo>:write`, an explicit cross-repo
    //    `repo:<other>:write` from `task.conflict_keys`, and a non-write custom lock.
    let run1 = t
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
                "conflict_keys": [
                    { "key": write_key, "max": 1 },
                    { "key": extra_write_key, "max": 1 },
                    { "key": custom_key, "max": 1 }
                ]
            })),
        )
        .await;
    let run1_id = run1["id"].as_str().unwrap().to_owned();
    assert_eq!(run1["conflict_locks"].as_array().unwrap().len(), 3);

    // Transitioning `running -> waiting_external` releases all `repo:*:write` locks while
    // preserving non-write locks (`db:migrate`), and records `released_locks` in the Trail Rule event.
    let waiting = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{run1_id}"),
            Some(DISPATCH_MAC),
            Some(json!({ "state": "waiting_external" })),
        )
        .await;
    assert_eq!(waiting["state"], "waiting_external");
    let held_keys: Vec<&str> = waiting["conflict_locks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        held_keys,
        vec![custom_key],
        "waiting_external must release repo:*:write while keeping non-write locks: {waiting}"
    );
    let events = t
        .ok("GET", "/v1/events?board=factory&since=0", None, None)
        .await["events"]
        .as_array()
        .unwrap()
        .clone();
    let wait_ev = events
        .iter()
        .find(|e| {
            e["type"] == "run_updated"
                && e["payload"]["run_id"] == run1_id
                && e["payload"]["to"] == "waiting_external"
        })
        .expect("run_updated Trail Rule event for waiting_external");
    assert_eq!(
        wait_ev["payload"]["released_locks"],
        json!([write_key, extra_write_key])
    );

    // Acquiring `repo:*:write` while in `waiting_external` is refused with 400 invalid.
    let (code, err, v) = t
        .fail(
            "PATCH",
            &format!("/v1/runs/{run1_id}"),
            Some(DISPATCH_MAC),
            Some(json!({ "conflict_keys": [{ "key": write_key, "max": 1 }] })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");

    // Peer worker can now acquire `repo:<repo>:write` (max=1) while run1 is waiting_external!
    let run2 = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "task_id": task2_id,
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "state": "running",
                "conflict_keys": [{ "key": write_key, "max": 1 }]
            })),
        )
        .await;
    let run2_id = run2["id"].as_str().unwrap().to_owned();

    // Resuming run1 (`waiting_external -> running`) re-acquires `repo:<repo>:write` cleanly
    // without failing with 409 even though run2 currently occupies 1/1, so the resumed run's
    // lease renewal never fails and a 3rd worker is blocked until concurrency drops below cap.
    let resumed = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{run1_id}"),
            Some(DISPATCH_MAC),
            Some(json!({ "state": "running", "lease_ttl_secs": 180 })),
        )
        .await;
    assert_eq!(resumed["state"], "running");
    let resumed_keys: Vec<&str> = resumed["conflict_locks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["key"].as_str().unwrap())
        .collect();
    assert!(
        resumed_keys.contains(&write_key.as_str()) && resumed_keys.contains(&extra_write_key),
        "resumed managed worker must re-acquire both derived and explicit repo:*:write: {resumed}"
    );
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM conflict_locks WHERE key = '{write_key}'"
        )),
        2
    );
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM conflict_locks WHERE key = '{extra_write_key}'"
        )),
        1
    );

    // Finish both managed runs to clear the table.
    for rid in [&run1_id, &run2_id] {
        t.ok(
            "PATCH",
            &format!("/v1/runs/{rid}"),
            Some(DISPATCH_MAC),
            Some(json!({ "state": "succeeded" })),
        )
        .await;
    }
    assert_eq!(t.count("SELECT COUNT(*) FROM conflict_locks"), 0);

    // 2. Interactive run holding `repo:<repo>:write` releases it on `idle` -> `waiting_external`.
    let interactive_agent = "claude:idle-lock-test";
    t.register(interactive_agent, Some(REPO), Some("main"))
        .await;
    let interactive_runs = t
        .ok(
            "GET",
            &format!("/v1/runs?agent={interactive_agent}&active=true"),
            None,
            None,
        )
        .await;
    let interactive_id = interactive_runs[0]["id"].as_str().unwrap().to_owned();
    t.ok(
        "PATCH",
        &format!("/v1/runs/{interactive_id}"),
        None,
        Some(json!({ "conflict_keys": [{ "key": write_key, "max": 1 }] })),
    )
    .await;
    assert_eq!(t.count("SELECT COUNT(*) FROM conflict_locks"), 1);
    t.ok(
        "POST",
        &format!("/v1/agents/{interactive_agent}/state"),
        Some(interactive_agent),
        Some(json!({ "state": "idle" })),
    )
    .await;
    assert_eq!(
        t.count("SELECT COUNT(*) FROM conflict_locks"),
        0,
        "interactive idle -> waiting_external must release repo:*:write locks"
    );
    let interactive_events = t
        .ok("GET", "/v1/events?board=factory&since=0", None, None)
        .await["events"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        interactive_events.iter().any(|e| {
            e["type"] == "run_updated"
                && e["actor"] == interactive_agent
                && e["payload"]["run_id"] == interactive_id
                && e["payload"]["from"] == "running"
                && e["payload"]["to"] == "waiting_external"
                && e["payload"]["fields"] == json!(["state", "conflict_keys"])
                && e["payload"]["released_locks"] == json!([write_key])
        }),
        "interactive idle -> waiting_external must emit Trail Rule run_updated event: {interactive_events:?}"
    );

    // 3. Pre-existing legacy `waiting_external` row in `conflict_locks` is reconciled by
    //    `sweep_runs()` with a Trail Rule `run_updated` event, and a second sweep is a no-op.
    let now_ms = t.state.now().timestamp_millis();
    t.exec(&format!(
        "INSERT INTO conflict_locks (key, run_id, max_holders, acquired_at) \
         VALUES ('{write_key}', '{interactive_id}', 1, {now_ms})"
    ));
    assert_eq!(t.count("SELECT COUNT(*) FROM conflict_locks"), 1);
    assert_eq!(t.sweep_runs(), 1);
    assert_eq!(t.count("SELECT COUNT(*) FROM conflict_locks"), 0);
    assert_eq!(t.sweep_runs(), 0, "reconciliation sweep must be idempotent");
    let events_after = t
        .ok("GET", "/v1/events?board=factory&since=0", None, None)
        .await["events"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        events_after.iter().any(|e| {
            e["type"] == "run_updated"
                && e["payload"]["run_id"] == interactive_id
                && e["payload"]["cause"] == "waiting_external_write_lock_released"
                && e["payload"]["released_locks"] == json!([write_key])
        }),
        "expected Trail Rule event for swept waiting_external write lock: {events_after:?}"
    );
}
