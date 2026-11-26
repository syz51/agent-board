//! Comprehensive tests for Slice 2.1 (`POST /v1/dispatch/claim`, Admission, Filter/Taints,
//! Multifactor Score/Fair-Share/Backfill, `conflict_keys`, unified verdict/triage/merge/
//! housekeeping/goal/command claiming, 3m lease expiry, and sticky wake delivery & fallback).

use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{REPO, T, check};

const WORKER: &str = "t3:worker";
const REVIEWER: &str = "t3:reviewer";
const ASKER: &str = "t3:asker";
const DISPATCH_MAC: &str = "dispatch:mac";

async fn create_device(t: &T, name: &str) -> String {
    let (s, v) = t
        .as_user("POST", "/v1/admin/devices", Some(json!({ "name": name })))
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    v["token"].as_str().unwrap().to_owned()
}

async fn send_as(
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

async fn autopilot_board(t: &T, slug: &str, repos: &[&str], parallel_tasks: u32) {
    t.board(slug, repos).await;
    let (s, v) = t
        .as_user(
            "PATCH",
            &format!("/v1/boards/{slug}"),
            Some(json!({
                "autopilot": true,
                "parallel_tasks": parallel_tasks
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
}

async fn claim_as(t: &T, token: &str, body: Value) -> (u16, Value) {
    send_as(t, token, "POST", "/v1/dispatch/claim", None, Some(body)).await
}

fn default_claim_body(device: &str, free_units: u32, repos: &[&str]) -> Value {
    json!({
        "device": device,
        "free_units": free_units,
        "runners": ["acp", "deterministic", "t3"],
        "capabilities": ["git:cloneable"],
        "repos": repos,
    })
}

async fn make_reviewed_task(t: &T, board: &str, title: &str, pass_note: Option<&str>) -> String {
    let task_id = t.task(WORKER, board, title).await;
    assert_eq!(t.claim(WORKER, &task_id).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &task_id, json!({ "status": "review" }))
            .await
            .0,
        200
    );
    // Worker notes the PR and finishes its turn (idle) so the Review readiness gate admits it.
    t.ok(
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(WORKER),
        Some(json!({
            "agent": WORKER,
            "kind": "note",
            "text": "pr: https://github.com/example/repo/pull/1"
        })),
    )
    .await;
    t.ok(
        "POST",
        &format!("/v1/agents/{WORKER}/state"),
        Some(WORKER),
        Some(json!({ "state": "idle" })),
    )
    .await;
    if let Some(text) = pass_note {
        let rev_run = t
            .ok(
                "POST",
                "/v1/runs",
                Some(DISPATCH_MAC),
                Some(json!({
                    "task_id": task_id,
                    "kind": "review",
                    "device": "mac",
                    "runner": "acp",
                    "agent_id": REVIEWER,
                    "state": "running"
                })),
            )
            .await;
        let run_id = rev_run["id"].as_str().unwrap();
        t.ok(
            "POST",
            &format!("/v1/tasks/{task_id}/notes"),
            Some(REVIEWER),
            Some(json!({ "agent": REVIEWER, "kind": "decision", "text": text })),
        )
        .await;
        t.ok(
            "PATCH",
            &format!("/v1/runs/{run_id}"),
            Some(DISPATCH_MAC),
            Some(json!({ "state": "succeeded" })),
        )
        .await;
    }
    task_id
}

#[tokio::test]
async fn dispatch_claim_auth_validation_and_drain_handling() {
    let t = T::new();
    let other_tok = create_device(&t, "runner-b").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "factory", &[REPO], 2).await;
    let _task = t.task(WORKER, "factory", "Ready work").await;

    // Device token (`mac`) cannot claim for another device (`runner-b`).
    let (code, err, v) = t
        .fail(
            "POST",
            "/v1/dispatch/claim",
            None,
            Some(default_claim_body("runner-b", 4, &[REPO])),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // Ordinary worker agent header cannot claim managed runs.
    let (code, err, v) = t
        .fail(
            "POST",
            "/v1/dispatch/claim",
            Some(WORKER),
            Some(default_claim_body("mac", 4, &[REPO])),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // Unknown or interactive RunKind in supported_kinds -> 400 invalid.
    for bad_kind in ["quantum", "interactive"] {
        let (code, err, v) = t
            .fail(
                "POST",
                "/v1/dispatch/claim",
                None,
                Some(json!({
                    "device": "mac",
                    "free_units": 4,
                    "runners": ["acp"],
                    "supported_kinds": [bad_kind]
                })),
            )
            .await;
        assert_eq!((code, err.as_str()), (400, "invalid"), "{bad_kind}: {v}");
    }

    // Revoked device -> 404 not_found (via user token).
    let (s, _) = t
        .as_user("DELETE", "/v1/admin/devices/runner-b", None)
        .await;
    assert_eq!(s.as_u16(), 204);
    let (s, v) = t
        .as_user(
            "POST",
            "/v1/dispatch/claim",
            Some(default_claim_body("runner-b", 4, &[REPO])),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (404, Some("not_found")),
        "{v}"
    );
    // Revoked token itself -> 401 unauthorized.
    let (s, _) = claim_as(&t, &other_tok, default_claim_body("runner-b", 4, &[REPO])).await;
    assert_eq!(s, 401);

    // Draining device returns empty `claimed` with `drain` hold summary.
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/admin/devices/mac",
            Some(json!({ "drain": true })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    let resp = t
        .ok(
            "POST",
            "/v1/dispatch/claim",
            Some(DISPATCH_MAC),
            Some(default_claim_body("mac", 4, &[REPO])),
        )
        .await;
    assert!(resp["claimed"].as_array().unwrap().is_empty());
    assert_eq!(resp["holds_summary"][0]["reason"], "drain");
}

#[tokio::test]
async fn concurrent_claim_race_grants_strictly_one_winner_for_tasks_and_off_autopilot_verdicts() {
    let t = T::new();
    let tok_a = create_device(&t, "mac-pro").await;
    let tok_b = create_device(&t, "mac-air").await;
    t.register(WORKER, Some(REPO), None).await;

    // 1. Concurrent worker race on one autopilot task.
    autopilot_board(&t, "factory", &[REPO], 2).await;
    let task_id = t.task(WORKER, "factory", "Single ready task").await;

    let (r1, r2) = tokio::join!(
        claim_as(&t, &tok_a, default_claim_body("mac-pro", 4, &[REPO])),
        claim_as(&t, &tok_b, default_claim_body("mac-air", 4, &[REPO])),
    );
    assert_eq!(r1.0, 200, "{}", r1.1);
    assert_eq!(r2.0, 200, "{}", r2.1);
    let c1 = r1.1["claimed"].as_array().unwrap();
    let c2 = r2.1["claimed"].as_array().unwrap();
    assert_eq!(c1.len() + c2.len(), 1, "strictly one device wins the task");
    let winner = if c1.len() == 1 { &c1[0] } else { &c2[0] };
    assert_eq!(winner["task_id"], task_id);
    assert_eq!(winner["run"]["kind"], "worker");
    assert_eq!(winner["run"]["state"], "leased");
    assert!(winner["run"]["lease_expires_at"].is_string());

    // 2. Off-autopilot verdict race (agent-board-650): board has autopilot=false,
    //    verdict_judges=true, and 6 pending proposals under no parent. Two home devices
    //    race simultaneously -> strictly ONE claims a batched verdict run (5 oldest)!
    t.board("manual", &[REPO]).await;
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/manual",
            Some(json!({
                "autopilot": false,
                "verdict_judges": true,
                "home_devices": ["mac-pro", "mac-air"]
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    let mut prop_ids = Vec::new();
    for i in 1..=6 {
        t.advance(TimeDelta::seconds(1));
        let pid = t
            .task_with(
                WORKER,
                json!({
                    "board": "manual",
                    "title": format!("Proposal {i}"),
                    "body": format!("Proposal: improvement {i}")
                }),
            )
            .await;
        prop_ids.push(pid);
    }

    let (v1, v2) = tokio::join!(
        claim_as(&t, &tok_a, default_claim_body("mac-pro", 4, &[REPO])),
        claim_as(&t, &tok_b, default_claim_body("mac-air", 4, &[REPO])),
    );
    assert_eq!(v1.0, 200, "{}", v1.1);
    assert_eq!(v2.0, 200, "{}", v2.1);
    let vc1 = v1.1["claimed"].as_array().unwrap();
    let vc2 = v2.1["claimed"].as_array().unwrap();
    assert_eq!(
        vc1.len() + vc2.len(),
        1,
        "agent-board-650: strictly one home device wins the off-autopilot verdict batch"
    );
    let verdict = if vc1.len() == 1 { &vc1[0] } else { &vc2[0] };
    assert_eq!(verdict["run"]["kind"], "verdict");
    assert_eq!(verdict["board"], "manual");
    let batched: Vec<&str> = verdict["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(batched.len(), 5, "batched up to 5 oldest proposals");
    assert_eq!(
        batched,
        prop_ids[..5].iter().map(String::as_str).collect::<Vec<_>>()
    );

    // Turning verdict_judges off prevents claiming proposals on another off-autopilot board.
    t.board("quiet", &[REPO]).await;
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/quiet",
            Some(json!({ "autopilot": false, "verdict_judges": false })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    t.task_with(
        WORKER,
        json!({ "board": "quiet", "title": "Muted proposal", "body": "Proposal: skip me" }),
    )
    .await;
    let (s, resp) = claim_as(&t, &tok_a, default_claim_body("mac-pro", 4, &[REPO])).await;
    assert_eq!(s, 200);
    assert!(resp["claimed"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn taints_isolation_requires_and_prefer_no_schedule_penalty() {
    let t = T::new();
    let control_tok = create_device(&t, "control-host").await;
    let svc_tok = create_device(&t, "service-host").await;
    let air_tok = create_device(&t, "mac-air").await;
    let pro_tok = create_device(&t, "mac-pro").await;

    // Configure 3-plane isolation + PreferNoSchedule soft taint on mac-air.
    for (name, patch) in [
        (
            "control-host",
            json!({
                "role": "control",
                "taints": [{ "key": "role", "value": "control", "effect": "no_schedule" }]
            }),
        ),
        ("service-host", json!({ "role": "service_host" })),
        (
            "mac-air",
            json!({
                "role": "runner",
                "taints": [{ "key": "weight", "value": "heavy", "effect": "prefer_no_schedule" }]
            }),
        ),
        ("mac-pro", json!({ "role": "runner", "preference": 20 })),
    ] {
        let (s, v) = t
            .as_user("PATCH", &format!("/v1/admin/devices/{name}"), Some(patch))
            .await;
        assert_eq!(s.as_u16(), 200, "{name}: {v}");
    }

    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "factory", &[REPO], 4).await;

    let normal_id = t.task(WORKER, "factory", "Normal coding task").await;

    // Neither control-host nor service-host receives untolerated coding tasks!
    for (name, tok) in [("control-host", &control_tok), ("service-host", &svc_tok)] {
        let (s, resp) = claim_as(&t, tok, default_claim_body(name, 8, &[REPO])).await;
        assert_eq!(s, 200, "{resp}");
        assert!(
            resp["claimed"].as_array().unwrap().is_empty(),
            "{name} must never claim untolerated work"
        );
        assert_eq!(resp["holds_summary"][0]["reason"], "taint");
    }

    // A task explicitly tolerating `role=control:NoSchedule` CAN run on control-host.
    let infra_id = t
        .task_with(
            WORKER,
            json!({
                "board": "factory",
                "title": "Control plane health check",
                "tolerations": [{ "key": "role", "value": "control", "effect": "no_schedule" }]
            }),
        )
        .await;
    let (s, resp) = claim_as(
        &t,
        &control_tok,
        default_claim_body("control-host", 4, &[REPO]),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1);
    assert_eq!(resp["claimed"][0]["task_id"], infra_id);

    // Claim the normal task out of the way so we can test `requires` and `PreferNoSchedule`.
    let (s, resp) = claim_as(&t, &pro_tok, default_claim_body("mac-pro", 4, &[REPO])).await;
    assert_eq!(s, 200);
    assert_eq!(resp["claimed"][0]["task_id"], normal_id);

    // Hard capability predicate: `requires: ["os:macos", "tool:xcode"]`.
    let xcode_id = t
        .task_with(
            WORKER,
            json!({
                "board": "factory",
                "title": "Build iOS app",
                "requires": ["os:macos", "tool:xcode"]
            }),
        )
        .await;
    let (s, resp) = claim_as(
        &t,
        &air_tok,
        json!({
            "device": "mac-air",
            "free_units": 8,
            "runners": ["acp"],
            "capabilities": ["os:linux", "git:cloneable"],
            "repos": [REPO]
        }),
    )
    .await;
    assert_eq!(s, 200);
    assert!(resp["claimed"].as_array().unwrap().is_empty());
    assert_eq!(resp["holds_summary"][0]["reason"], "requires");

    let (s, resp) = claim_as(
        &t,
        &pro_tok,
        json!({
            "device": "mac-pro",
            "free_units": 8,
            "runners": ["acp"],
            "capabilities": ["os:macos", "tool:xcode", "git:cloneable"],
            "repos": [REPO]
        }),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(resp["claimed"][0]["task_id"], xcode_id);

    // Soft taint `weight=heavy:PreferNoSchedule` on `mac-air`: on a fresh board/repo with both
    // a heavy (4u) and standard (2u) p2 task, `mac-air` penalizes the heavy task (-10.0 in
    // device_pref) and picks the standard task first!
    let soft_repo = "github.com/example/soft-taint";
    autopilot_board(&t, "soft-taint", &[soft_repo], 4).await;
    let heavy_id = t
        .task_with(
            WORKER,
            json!({ "board": "soft-taint", "title": "Heavy build", "weight": "heavy" }),
        )
        .await;
    t.advance(TimeDelta::seconds(5));
    let std_id = t
        .task_with(
            WORKER,
            json!({ "board": "soft-taint", "title": "Standard fix", "weight": "standard" }),
        )
        .await;
    let (s, resp) = claim_as(
        &t,
        &air_tok,
        default_claim_body("mac-air", 8, &[REPO, soft_repo]),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(
        resp["claimed"][0]["task_id"], std_id,
        "mac-air should prefer standard over heavy due to PreferNoSchedule"
    );
    // Next claim on `mac-air` still falls back to heavy (soft taint), showing the -10 penalty.
    let (s, resp) = claim_as(
        &t,
        &air_tok,
        default_claim_body("mac-air", 8, &[REPO, soft_repo]),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"][0]["task_id"], heavy_id);
    assert!(
        resp["claimed"][0]["score_breakdown"]["device_pref"]
            .as_f64()
            .unwrap()
            <= -10.0,
        "expected PreferNoSchedule penalty in device_pref: {}",
        resp["claimed"][0]["score_breakdown"]
    );
}

#[tokio::test]
async fn conflict_keys_serialize_merges_per_repo_and_release_on_terminal_transition() {
    let t = T::new();
    let tok_a = create_device(&t, "mac-a").await;
    let tok_b = create_device(&t, "mac-b").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    autopilot_board(&t, "factory", &[REPO], 2).await;

    let task1 = make_reviewed_task(
        &t,
        "factory",
        "PR merge 1",
        Some("review: pass. LGTM\npr: https://github.com/example/app/pull/101"),
    )
    .await;
    t.advance(TimeDelta::seconds(5));
    let task2 = make_reviewed_task(
        &t,
        "factory",
        "No-PR task 2",
        Some("review: pass. Verified docs.\npr: none"),
    )
    .await;

    // Device A claims merge for task 1, acquiring `repo:<repo>:merge` (max=1).
    let (s, r1) = claim_as(&t, &tok_a, default_claim_body("mac-a", 4, &[REPO])).await;
    assert_eq!(s, 200, "{r1}");
    assert_eq!(r1["claimed"].as_array().unwrap().len(), 1);
    let c1 = &r1["claimed"][0];
    assert_eq!(c1["run"]["kind"], "merge");
    assert_eq!(c1["run"]["runner"], "deterministic");
    assert_eq!(c1["run"]["cost_units"], 1);
    assert_eq!(c1["task_id"], task1);
    let run1_id = c1["run"]["id"].as_str().unwrap().to_owned();
    let merge_key = format!("repo:{REPO}:merge");
    assert_eq!(c1["conflict_keys"], json!([{ "key": merge_key, "max": 1 }]));

    // Device B calls claim -> task 2 is held by `conflict_key` (`repo:<repo>:merge` 1/1)!
    let (s, r2) = claim_as(&t, &tok_b, default_claim_body("mac-b", 4, &[REPO])).await;
    assert_eq!(s, 200, "{r2}");
    assert!(r2["claimed"].as_array().unwrap().is_empty());
    assert_eq!(r2["holds_summary"][0]["reason"], "conflict_key");

    // Device A runs and finishes Run 1 -> lock releases immediately -> Device B claims task 2!
    let (s, _) = send_as(
        &t,
        &tok_a,
        "PATCH",
        &format!("/v1/runs/{run1_id}"),
        None,
        Some(json!({ "state": "running" })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = send_as(
        &t,
        &tok_a,
        "PATCH",
        &format!("/v1/runs/{run1_id}"),
        None,
        Some(json!({ "state": "succeeded" })),
    )
    .await;
    assert_eq!(s, 200);

    // Close task1 so it's no longer in review.
    let (s, _) = t.done(WORKER, &task1, check(0)).await;
    assert_eq!(s.as_u16(), 200);

    let (s, r3) = claim_as(&t, &tok_b, default_claim_body("mac-b", 4, &[REPO])).await;
    assert_eq!(s, 200, "{r3}");
    assert_eq!(r3["claimed"].as_array().unwrap().len(), 1);
    assert_eq!(r3["claimed"][0]["run"]["kind"], "merge");
    assert_eq!(
        r3["claimed"][0]["task_id"], task2,
        "pr: none reviewed task is claimed as merge once lock releases"
    );
}

#[tokio::test]
async fn admission_parallel_tasks_caps_workers_only_and_zero_llm_bypasses_over_quota() {
    let t = T::new();
    let tok_a = create_device(&t, "mac-a").await;
    let tok_b = create_device(&t, "mac-b").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    t.register(ASKER, Some(REPO), None).await;
    autopilot_board(&t, "factory", &[REPO], 1).await;

    // Create two ready workers, one task needing review, one task needing merge, one proposal.
    let w1 = t.task(WORKER, "factory", "Worker 1").await;
    let w2 = t.task(WORKER, "factory", "Worker 2").await;
    let rev_task = make_reviewed_task(&t, "factory", "Needs review", None).await;
    let merge_task = make_reviewed_task(
        &t,
        "factory",
        "Needs merge",
        Some("review: pass. Ship it\npr: none"),
    )
    .await;
    // Fast-lane P1 proposal exercises immediate Verdict admission alongside review while
    // workers are capped at parallel_tasks=1.
    let _prop = t
        .task_with(
            WORKER,
            json!({
                "board": "factory",
                "title": "Prop 1",
                "body": "Proposal: add metric",
                "priority": "p1"
            }),
        )
        .await;
    // Advance past the 90s single-item verdict debounce window.
    t.advance(TimeDelta::seconds(91));

    // 1. Claim worker 1 explicitly (supported_kinds: ["worker"]) -> fills parallel_tasks=1.
    let (s, r) = claim_as(
        &t,
        &tok_a,
        json!({
            "device": "mac-a",
            "free_units": 8,
            "runners": ["acp", "deterministic"],
            "repos": [REPO],
            "supported_kinds": ["worker"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"][0]["task_id"], w1);

    // Worker 2 is held by `parallel_tasks` because unclaimed leased worker 1 counts!
    let (s, r) = claim_as(
        &t,
        &tok_b,
        json!({
            "device": "mac-b",
            "free_units": 8,
            "runners": ["acp", "deterministic"],
            "repos": [REPO],
            "supported_kinds": ["worker"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    assert!(r["claimed"].as_array().unwrap().is_empty());
    assert_eq!(r["holds_summary"][0]["reason"], "parallel_tasks");

    // Meanwhile review, merge, and verdict are NOT blocked by parallel_tasks!
    let (s, r) = claim_as(
        &t,
        &tok_b,
        json!({
            "device": "mac-b",
            "free_units": 8,
            "runners": ["acp", "deterministic"],
            "repos": [REPO],
            "supported_kinds": ["review", "verdict"],
            "max_claims": 2
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    let kinds: Vec<&str> = r["claimed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["run"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["review", "verdict"]);
    assert_eq!(r["claimed"][0]["task_id"], rev_task);

    // Union check: raise parallel_tasks to 2, have WORKER claim w1 (so w1 is BOTH `doing`
    // AND has an active `leased` worker run). Union count is 1 (not 2!), so w2 IS admitted!
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/factory",
            Some(json!({ "parallel_tasks": 2 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    assert_eq!(t.claim(WORKER, &w1).await.0, 200);

    // 2. Now put `("anthropic", "default")` over usage_stop_at (95% >= 90%).
    let now = t.state.now();
    t.ok(
        "PUT",
        "/v1/usage/anthropic/default",
        None,
        Some(json!({
            "five_hour": { "percent": 95.0, "resets_at": super::iso(now + TimeDelta::hours(2)) },
            "read_at": super::iso(now),
        })),
    )
    .await;
    // Also report `("antigravity", "default")` at 20% (healthy).
    t.ok(
        "PUT",
        "/v1/usage/antigravity/default",
        None,
        Some(json!({
            "five_hour": { "percent": 20.0, "resets_at": super::iso(now + TimeDelta::hours(2)) },
            "read_at": super::iso(now),
        })),
    )
    .await;

    // LLM worker on default `anthropic:default` bucket is HELD by `quota`!
    let (s, r) = claim_as(
        &t,
        &tok_b,
        json!({
            "device": "mac-b",
            "free_units": 8,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["worker"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    assert!(r["claimed"].as_array().unwrap().is_empty());
    assert_eq!(r["holds_summary"][0]["reason"], "quota");

    // Zero-LLM `merge` AND `housekeeping` BYPASS the over-quota `anthropic:default` bucket!
    let ask_task = t.task(ASKER, "factory", "Gone asker task").await;
    assert_eq!(t.claim(ASKER, &ask_task).await.0, 200);
    assert_eq!(
        t.patch(
            ASKER,
            &ask_task,
            json!({ "status": "blocked", "blocked_reason": "waiting on user" })
        )
        .await
        .0,
        200
    );
    let ask_id = t
        .ok(
            "POST",
            "/v1/asks",
            Some(ASKER),
            Some(json!({
                "agent": ASKER,
                "board": "factory",
                "task": ask_task,
                "kind": "decision",
                "question": "Which option?",
                "options": ["A", "B"]
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{ask_id}/answer"),
            Some(json!({ "answer": "A" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    // Mark ASKER ended -> gone!
    t.ok(
        "POST",
        &format!("/v1/agents/{ASKER}/state"),
        Some(ASKER),
        Some(json!({ "state": "ended", "paused_until": null })),
    )
    .await;

    let (s, r) = claim_as(
        &t,
        &tok_b,
        json!({
            "device": "mac-b",
            "free_units": 8,
            "runners": ["acp", "deterministic"],
            "repos": [REPO],
            "max_claims": 2
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    let claimed = r["claimed"].as_array().unwrap();
    assert_eq!(
        claimed.len(),
        2,
        "both merge and housekeeping bypass LLM quota"
    );
    assert_eq!(claimed[0]["run"]["kind"], "merge");
    assert_eq!(claimed[0]["task_id"], merge_task);
    assert_eq!(claimed[1]["run"]["kind"], "housekeeping");
    assert_eq!(
        claimed[1]["housekeeping_action"]["action"],
        "answer_gone_asker"
    );
    assert_eq!(claimed[1]["housekeeping_action"]["ask_id"], ask_id);
    assert_eq!(claimed[1]["housekeeping_action"]["task_id"], ask_task);

    // Device using healthy `antigravity:default` quota bucket CAN claim w2 (also verifying
    // union count for w1 was 1 < parallel_tasks=2)!
    let (s, r) = claim_as(
        &t,
        &tok_b,
        json!({
            "device": "mac-b",
            "free_units": 8,
            "runners": ["acp"],
            "repos": [REPO],
            "quota_bucket": { "provider": "antigravity", "account": "default" },
            "supported_kinds": ["worker"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"][0]["task_id"], w2);
}

#[tokio::test]
async fn multifactor_scoring_kind_priority_rank_age_fair_share_locality_and_backfill() {
    let t = T::new();
    let tok = create_device(&t, "mac-pro").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;

    let repo_a = "github.com/example/repo-a";
    let repo_b = "github.com/example/repo-b";
    autopilot_board(&t, "alpha", &[repo_a], 4).await;
    autopilot_board(&t, "beta", &[repo_b], 4).await;

    // 1. Priority & rank ordering: p1 unranked > p2 rank=1 > p2 rank=2 > p2 unranked > p3.
    let p3 = t
        .task_with(
            WORKER,
            json!({ "board": "alpha", "title": "P3", "priority": "p3" }),
        )
        .await;
    let p2_unranked = t
        .task_with(
            WORKER,
            json!({ "board": "alpha", "title": "P2 unranked", "priority": "p2" }),
        )
        .await;
    let p2_r2 = t
        .task_with(
            WORKER,
            json!({ "board": "alpha", "title": "P2 rank 2", "priority": "p2" }),
        )
        .await;
    let p2_r1 = t
        .task_with(
            WORKER,
            json!({ "board": "alpha", "title": "P2 rank 1", "priority": "p2" }),
        )
        .await;
    let p1 = t
        .task_with(
            WORKER,
            json!({ "board": "alpha", "title": "P1 unranked", "priority": "p1" }),
        )
        .await;
    t.ok(
        "POST",
        "/v1/boards/alpha/rank",
        Some(WORKER),
        Some(json!({ "agent": WORKER, "tasks": [p2_r1, p2_r2] })),
    )
    .await;

    // Claim 4 at once (max_claims=4) with plenty of capacity to observe exact ranking!
    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-pro",
            "free_units": 16,
            "runners": ["acp"],
            "repos": [repo_a],
            "max_claims": 4
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    let order: Vec<&str> = resp["claimed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["task_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            p1.as_str(),
            p2_r1.as_str(),
            p2_r2.as_str(),
            p2_unranked.as_str()
        ]
    );
    // Remaining task is p3.
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/alpha",
            Some(json!({ "parallel_tasks": 4 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    // Cancel the 4 runs so alpha is clean for fair-share test (note: runs remain in 24h
    // history so alpha has consumed 8 units in the rolling 24h window!).
    for c in resp["claimed"].as_array().unwrap() {
        let rid = c["run"]["id"].as_str().unwrap();
        let (s, _) = send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{rid}"),
            None,
            Some(json!({ "state": "cancelled" })),
        )
        .await;
        assert_eq!(s, 200);
    }
    // Drop p1..p3 so we start fresh on alpha & beta.
    for id in [&p1, &p2_r1, &p2_r2, &p2_unranked, &p3] {
        assert_eq!(
            t.patch(WORKER, id, json!({ "status": "dropped" })).await.0,
            200
        );
    }

    // 2. Fair-share + anti-starvation age: alpha consumed 8 units in last 24h, beta consumed 0.
    //    Equal-priority tasks on alpha and beta -> starved beta wins via fair_share!
    let alpha_t = t.task(WORKER, "alpha", "Alpha fresh").await;
    let beta_t = t.task(WORKER, "beta", "Beta starved").await;
    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-pro",
            "free_units": 4,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [repo_a, repo_b],
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"][0]["task_id"], beta_t);
    assert_eq!(resp["claimed"][0]["score_breakdown"]["fair_share"], 0.0);

    // 3. Anti-starvation age: create old task on beta, advance clock 12h, create new task on
    //    beta -> old task has strictly higher age score and wins!
    let old_t = t.task(WORKER, "beta", "Old waiting task").await;
    t.advance(TimeDelta::hours(12));
    let _new_t = t.task(WORKER, "beta", "Fresh task").await;
    // Cancel alpha_t out of the way.
    assert_eq!(
        t.patch(WORKER, &alpha_t, json!({ "status": "dropped" }))
            .await
            .0,
        200
    );
    let (s, resp) = claim_as(&t, &tok, default_claim_body("mac-pro", 4, &[repo_b])).await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"][0]["task_id"], old_t);
    let age = resp["claimed"][0]["score_breakdown"]["age"]
        .as_f64()
        .unwrap();
    assert!(
        age > 3.0 && age <= 8.0,
        "expected capped age boost (<= 8.0) after 12h: {}",
        resp["claimed"][0]["score_breakdown"]
    );

    // 4. Warm-repo locality: create two equal boards/tasks (gamma warm vs delta cold with
    //    git:cloneable) -> device with gamma warm prefers gamma and gets locality > 0!
    let repo_g = "github.com/example/gamma";
    let repo_d = "github.com/example/delta";
    autopilot_board(&t, "gamma", &[repo_g], 2).await;
    autopilot_board(&t, "delta", &[repo_d], 2).await;
    let delta_t = t.task(WORKER, "delta", "Delta cold").await;
    let gamma_t = t.task(WORKER, "gamma", "Gamma warm").await;
    // Drop beta's remaining task so only gamma and delta compete.
    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-pro",
            "free_units": 2,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [repo_g],
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"][0]["task_id"], gamma_t);
    assert!(
        resp["claimed"][0]["score_breakdown"]["locality"]
            .as_f64()
            .unwrap()
            >= 8.0,
        "expected warm-repo locality bonus: {}",
        resp["claimed"][0]["score_breakdown"]
    );
    assert_ne!(gamma_t, delta_t);

    // 5. Light-job Backfill: on a fresh board `bf`, create a p1 heavy (4u) worker, a p2
    //    standard (2u) worker, and a p2 light (1u) worker. Request with free_units = 2 ->
    //    heavy p1 is blocked by capacity, so light (1u) gets the backfill bonus and wins!
    let repo_bf = "github.com/example/bf";
    autopilot_board(&t, "bf", &[repo_bf], 4).await;
    let _heavy_p1 = t
        .task_with(
            WORKER,
            json!({ "board": "bf", "title": "Heavy P1", "priority": "p1", "weight": "heavy" }),
        )
        .await;
    let _std_p2 = t
        .task_with(
            WORKER,
            json!({ "board": "bf", "title": "Std P2", "priority": "p2", "weight": "standard" }),
        )
        .await;
    let light_p2 = t
        .task_with(
            WORKER,
            json!({ "board": "bf", "title": "Light P2", "priority": "p2", "weight": "light" }),
        )
        .await;
    // Claim with repos=[repo_bf] and NO git:cloneable so only `bf` candidates match.
    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-pro",
            "free_units": 2,
            "runners": ["acp"],
            "capabilities": [],
            "repos": [repo_bf],
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"][0]["task_id"], light_p2);
    assert!(
        resp["claimed"][0]["score_breakdown"]["backfill"]
            .as_f64()
            .unwrap()
            > 0.0,
        "expected backfill bonus when heavy p1 couldn't fit: {}",
        resp["claimed"][0]["score_breakdown"]
    );
    assert!(
        resp["holds_summary"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["reason"] == "capacity"),
        "expected capacity hold for heavy p1: {}",
        resp["holds_summary"]
    );
}

#[tokio::test]
async fn unified_kinds_triage_goal_housekeeping_away_owner_and_queued_command() {
    let t = T::new();
    let tok = create_device(&t, "runner-1").await;
    t.register(WORKER, Some(REPO), None).await;

    // 1. Feedback triage on `board-feedback` when autopilot is on.
    t.ok(
        "POST",
        "/v1/feedback",
        Some(WORKER),
        Some(json!({ "about": "board", "text": "Slow command\ndetails" })),
    )
    .await;
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/board-feedback",
            Some(json!({ "autopilot": true })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    // Advance past the 90s single-item triage debounce window.
    t.advance(TimeDelta::seconds(91));
    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-1",
            "free_units": 4,
            "runners": ["acp"],
            "supported_kinds": ["triage"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["kind"], "triage");
    assert_eq!(resp["claimed"][0]["board"], "board-feedback");
    assert_eq!(resp["claimed"][0]["tasks"].as_array().unwrap().len(), 1);

    // 2. Goal review on an active autopilot board with a goal, a done task, and nothing active.
    t.ok(
        "POST",
        "/v1/boards",
        None,
        Some(json!({
            "slug": "goal-board",
            "title": "Goal Board",
            "goal": "Ship v1 cleanly",
            "autopilot": true,
            "repos": [REPO]
        })),
    )
    .await;
    let done_t = t.task(WORKER, "goal-board", "Finish milestone").await;
    assert_eq!(t.claim(WORKER, &done_t).await.0, 200);
    assert_eq!(t.done(WORKER, &done_t, check(0)).await.0.as_u16(), 200);

    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-1",
            "free_units": 4,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["goal"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["kind"], "goal");
    assert_eq!(resp["claimed"][0]["board"], "goal-board");
    let goal_run_id = resp["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    // Complete goal run; no second goal review until another task_done happens.
    let (s, _) = send_as(
        &t,
        &tok,
        "PATCH",
        &format!("/v1/runs/{goal_run_id}"),
        None,
        Some(json!({ "state": "running" })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = send_as(
        &t,
        &tok,
        "PATCH",
        &format!("/v1/runs/{goal_run_id}"),
        None,
        Some(json!({ "state": "succeeded" })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-1",
            "free_units": 4,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["goal"]
        }),
    )
    .await;
    assert_eq!(s, 200);
    assert!(resp["claimed"].as_array().unwrap().is_empty());

    // 3. Housekeeping ReleaseAwayOwner: doing task past repeat `idle_doing` + 5m grace.
    autopilot_board(&t, "hk", &[REPO], 2).await;
    let away_task = t.task(WORKER, "hk", "Abandoned doing task").await;
    assert_eq!(t.claim(WORKER, &away_task).await.0, 200);
    t.ok(
        "POST",
        &format!("/v1/agents/{WORKER}/state"),
        Some(WORKER),
        Some(json!({ "state": "idle", "paused_until": null })),
    )
    .await;
    // First idle notice at +30m, second (repeat) at +60m.
    t.advance(TimeDelta::minutes(31));
    t.state
        .db()
        .write(
            || t.state.now(),
            t.state.settings(),
            crate::ops::tasks::announce_idle_doing,
        )
        .unwrap();
    t.advance(TimeDelta::minutes(30));
    t.state
        .db()
        .write(
            || t.state.now(),
            t.state.settings(),
            crate::ops::tasks::announce_idle_doing,
        )
        .unwrap();
    // Before 5m grace: not claimable yet.
    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-1",
            "free_units": 4,
            "runners": ["deterministic"],
            "supported_kinds": ["housekeeping"]
        }),
    )
    .await;
    assert_eq!(s, 200);
    assert!(resp["claimed"].as_array().unwrap().is_empty());
    // After 5m grace: claimed as `ReleaseAwayOwner`!
    t.advance(TimeDelta::minutes(6));
    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-1",
            "free_units": 4,
            "runners": ["deterministic"],
            "supported_kinds": ["housekeeping"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["kind"], "housekeeping");
    assert_eq!(
        resp["claimed"][0]["housekeeping_action"]["action"],
        "release_away_owner"
    );
    assert_eq!(
        resp["claimed"][0]["housekeeping_action"]["task_id"],
        away_task
    );
    assert_eq!(resp["claimed"][0]["housekeeping_action"]["owner"], WORKER);

    // 4. Explicitly queued `command` run transitions `queued -> leased` on claim.
    let queued = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "board": "hk",
                "kind": "command",
                "device": "mac",
                "runner": "deterministic",
                "state": "queued",
                "title": "Run backup verification",
                "cost_units": 1
            })),
        )
        .await;
    let qid = queued["id"].as_str().unwrap().to_owned();
    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-1",
            "free_units": 4,
            "runners": ["deterministic"],
            "supported_kinds": ["command"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["id"], qid);
    assert_eq!(resp["claimed"][0]["run"]["state"], "leased");
    assert_eq!(resp["claimed"][0]["run"]["device"], "runner-1");
}

#[tokio::test]
async fn sweeper_lease_expiry_and_sticky_wake_delivery_ack_and_fallback() {
    let t = T::new();
    let tok_a = create_device(&t, "mac-a").await;
    let tok_b = create_device(&t, "mac-b").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "factory", &[REPO], 2).await;

    // Heartbeat both devices so `mac-a` has a recorded `heartbeat_at`.
    for (name, tok) in [("mac-a", &tok_a), ("mac-b", &tok_b)] {
        let (s, _) = send_as(
            &t,
            tok,
            "PUT",
            &format!("/v1/devices/{name}/heartbeat"),
            None,
            Some(json!({ "os": "macos", "runners": ["acp", "deterministic"], "repos": [REPO] })),
        )
        .await;
        assert_eq!(s, 200);
    }

    // 1. 3m lease expiry -> orphaned releases conflict_locks & lets awake `mac-b` claim recovery!
    let task1 = t.task(WORKER, "factory", "Recoverable task").await;
    let (s, r1) = claim_as(&t, &tok_a, default_claim_body("mac-a", 4, &[REPO])).await;
    assert_eq!(s, 200, "{r1}");
    let run1_id = r1["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    // Transition run1 to running & attach WORKER.
    let (s, _) = send_as(
        &t,
        &tok_a,
        "PATCH",
        &format!("/v1/runs/{run1_id}"),
        None,
        Some(json!({
            "state": "running",
            "agent_id": WORKER,
            "worktree_branch": "task/factory-1"
        })),
    )
    .await;
    assert_eq!(s, 200);

    // Create and answer an ask by WORKER -> routes a sticky wake to `mac-a`!
    let ask1 = t
        .ok(
            "POST",
            "/v1/asks",
            Some(WORKER),
            Some(json!({
                "agent": WORKER,
                "board": "factory",
                "task": task1,
                "kind": "decision",
                "question": "Proceed?",
                "options": ["yes", "no"]
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{ask1}/answer"),
            Some(json!({ "answer": "yes" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);

    // Device `mac-b` cannot read `mac-a`'s wakes (403 forbidden).
    let (s, v) = send_as(&t, &tok_b, "GET", "/v1/devices/mac-a/wakes", None, None).await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");

    // Device `mac-a` lists its pending wake, then acknowledges it.
    let (s, wakes) = send_as(&t, &tok_a, "GET", "/v1/devices/mac-a/wakes", None, None).await;
    assert_eq!(s, 200, "{wakes}");
    let list = wakes.as_array().unwrap();
    assert_eq!(list.len(), 1);
    let wake1_id = list[0]["id"].as_i64().unwrap();
    assert_eq!(list[0]["kind"], "answered");
    assert_eq!(list[0]["run_id"], run1_id);

    // Device `mac-b` cannot ack `mac-a`'s wake (403 forbidden).
    let (s, v) = send_as(
        &t,
        &tok_b,
        "POST",
        &format!("/v1/wakes/{wake1_id}/ack"),
        None,
        None,
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");

    // Device `mac-a` acks wake 1 (and ack is idempotent).
    let (s, acked) = send_as(
        &t,
        &tok_a,
        "POST",
        &format!("/v1/wakes/{wake1_id}/ack"),
        None,
        None,
    )
    .await;
    assert_eq!(s, 200, "{acked}");
    assert!(acked["delivered_at"].is_string());
    let (s, wakes_after) = send_as(&t, &tok_a, "GET", "/v1/devices/mac-a/wakes", None, None).await;
    assert_eq!(s, 200);
    assert!(wakes_after.as_array().unwrap().is_empty());

    // Now create a second ask + answer so there is a pending wake when `mac-a`'s 3m lease expires.
    let ask2 = t
        .ok(
            "POST",
            "/v1/asks",
            Some(WORKER),
            Some(json!({
                "agent": WORKER,
                "board": "factory",
                "task": task1,
                "kind": "decision",
                "question": "Second question?",
                "options": ["ok"]
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{ask2}/answer"),
            Some(json!({ "answer": "ok" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);

    // Advance 181s (past 3m lease TTL) -> sweeper orphans run1, releases conflict_locks,
    // expires the pending sticky wake, and lets `mac-b` claim recovery immediately!
    t.advance(TimeDelta::seconds(181));
    assert_eq!(t.sweep_runs(), 1);
    assert_eq!(t.count("SELECT COUNT(*) FROM conflict_locks"), 0);
    let (s, wakes_mac_a) = send_as(&t, &tok_a, "GET", "/v1/devices/mac-a/wakes", None, None).await;
    assert_eq!(s, 200);
    assert!(
        wakes_mac_a.as_array().unwrap().is_empty(),
        "expired sticky wake must no longer be returned"
    );

    // `mac-b` immediately claims recovery for task1!
    let (s, r2) = claim_as(&t, &tok_b, default_claim_body("mac-b", 4, &[REPO])).await;
    assert_eq!(s, 200, "{r2}");
    assert_eq!(r2["claimed"].as_array().unwrap().len(), 1);
    assert_eq!(r2["claimed"][0]["task_id"], task1);
    let run2_id = r2["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();

    // 2. Sticky grace fallback (10m) on `waiting_external` run with a long (1h) lease when
    //    `mac-b`'s heartbeat goes stale!
    let (s, _) = send_as(
        &t,
        &tok_b,
        "PUT",
        "/v1/devices/mac-b/heartbeat",
        None,
        Some(json!({ "os": "macos", "runners": ["acp"], "repos": [REPO] })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = send_as(
        &t,
        &tok_b,
        "PATCH",
        &format!("/v1/runs/{run2_id}"),
        None,
        Some(json!({
            "state": "running",
            "agent_id": WORKER,
            "lease_ttl_secs": 3600
        })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = send_as(
        &t,
        &tok_b,
        "PATCH",
        &format!("/v1/runs/{run2_id}"),
        None,
        Some(json!({ "state": "waiting_external" })),
    )
    .await;
    assert_eq!(s, 200);

    // Enqueue a sticky wake on `mac-b`.
    let ask3 = t
        .ok(
            "POST",
            "/v1/asks",
            Some(WORKER),
            Some(json!({
                "agent": WORKER,
                "board": "factory",
                "task": task1,
                "kind": "decision",
                "question": "Third question?",
                "options": ["go"]
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{ask3}/answer"),
            Some(json!({ "answer": "go" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);

    // At +9m59s: still within 10m sticky_grace_secs -> not swept.
    t.advance(TimeDelta::seconds(599));
    assert_eq!(t.sweep_runs(), 0);

    // At +10m01s: `mac-b` heartbeat is stale past sticky_grace_secs -> run2 orphaned with
    // `device_stale_sticky_fallback` Trail Rule event, wake expired, and `mac-a` can claim!
    t.advance(TimeDelta::seconds(2));
    assert_eq!(t.sweep_runs(), 1);
    let run2_after = t
        .ok("GET", &format!("/v1/runs/{run2_id}"), None, None)
        .await;
    assert_eq!(run2_after["state"], "orphaned");
    assert_eq!(t.count("SELECT COUNT(*) FROM conflict_locks"), 0);

    let events = t
        .ok(
            "GET",
            "/v1/events?board=factory&since=0&limit=100",
            None,
            None,
        )
        .await["events"]
        .as_array()
        .unwrap()
        .clone();
    let fallback_ev = events
        .iter()
        .find(|e| {
            e["type"] == "run_orphaned"
                && e["payload"]["run_id"] == run2_id
                && e["payload"]["cause"] == "device_stale_sticky_fallback"
        })
        .expect("expected device_stale_sticky_fallback Trail Rule event");
    assert_eq!(fallback_ev["payload"]["device"], "mac-b");
    assert!(
        !fallback_ev["payload"]["expired_wakes"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let (s, r3) = claim_as(&t, &tok_a, default_claim_body("mac-a", 4, &[REPO])).await;
    assert_eq!(s, 200, "{r3}");
    assert_eq!(r3["claimed"][0]["task_id"], task1);
}

#[tokio::test]
async fn kind_priority_drains_wip_before_workers_and_pace_limit_zero_holds_llm_only() {
    let t = T::new();
    let tok = create_device(&t, "runner-a").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    t.register(ASKER, Some(REPO), None).await;
    autopilot_board(&t, "wip", &[REPO], 2).await;

    // Create one ready p1 worker, one pending proposal (verdict), one task needing review,
    // and one task with standing review: pass (merge) -> merge > review > verdict > worker!
    let _worker_t = t
        .task_with(
            WORKER,
            json!({ "board": "wip", "title": "New P1 work", "priority": "p1" }),
        )
        .await;
    // Fast-lane P1 proposal exercises immediate Verdict ordering (merge > review > verdict > worker).
    let _prop_t = t
        .task_with(
            WORKER,
            json!({
                "board": "wip",
                "title": "Prop",
                "body": "Proposal: cleanup",
                "priority": "p1"
            }),
        )
        .await;
    // Advance past the 90s single-item verdict debounce window.
    t.advance(TimeDelta::seconds(91));
    let _rev_t = make_reviewed_task(&t, "wip", "Awaiting review", None).await;
    let _merge_t = make_reviewed_task(
        &t,
        "wip",
        "Passed review",
        Some("review: pass. Ready.\npr: none"),
    )
    .await;

    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-a",
            "free_units": 12,
            "runners": ["acp", "deterministic"],
            "repos": [REPO],
            "max_claims": 4
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    let kinds: Vec<&str> = resp["claimed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["run"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["merge", "review", "verdict", "worker"]);

    // Weekly pace hard line (pace_limit == 0) holds LLM runs with `quota` while allowing
    // zero-LLM merge runs!
    autopilot_board(&t, "paced", &[REPO], 2).await;
    let _paced_worker = t.task(WORKER, "paced", "Paced worker").await;
    let paced_merge = make_reviewed_task(
        &t,
        "paced",
        "Paced merge",
        Some("review: pass. Clean.\npr: none"),
    )
    .await;
    // Free the repo merge lock from wip's merge run first.
    let wip_merge_run = resp["claimed"][0]["run"]["id"].as_str().unwrap();
    let (s, _) = send_as(
        &t,
        &tok,
        "PATCH",
        &format!("/v1/runs/{wip_merge_run}"),
        None,
        Some(json!({ "state": "cancelled" })),
    )
    .await;
    assert_eq!(s, 200);

    // Set reserve=10% and report seven_day=92% (past hard line 90%, below board stop 90? Set
    // board usage_stop_at=99 so ONLY pace_limit == 0 triggers!).
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/paced",
            Some(json!({ "usage_stop_at": 99 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    let (s, _) = t
        .as_user(
            "PUT",
            "/v1/usage/anthropic/default/reserve",
            Some(json!({ "reserve": 10.0 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    let now = t.state.now();
    t.ok(
        "PUT",
        "/v1/usage/anthropic/default",
        None,
        Some(json!({
            "seven_day": { "percent": 92.0, "resets_at": super::iso(now + TimeDelta::days(3)) },
            "read_at": super::iso(now),
        })),
    )
    .await;

    // LLM worker is held by `quota` (pace_limit == 0), while zero-LLM merge is claimed!
    let (s, r) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-a",
            "free_units": 8,
            "runners": ["acp", "deterministic"],
            "repos": [REPO],
            "max_claims": 2
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"].as_array().unwrap().len(), 1);
    assert_eq!(r["claimed"][0]["run"]["kind"], "merge");
    assert_eq!(r["claimed"][0]["task_id"], paced_merge);
    assert!(
        r["holds_summary"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["reason"] == "quota"),
        "expected quota hold for paced worker: {}",
        r["holds_summary"]
    );
}

#[tokio::test]
async fn diff_review_edge_cases_wakes_auth_goal_retry_spells_locks_and_cross_device_command() {
    let t = T::new();
    let tok_a = create_device(&t, "mac-a").await;
    let tok_b = create_device(&t, "mac-b").await;
    let tok_never_hb = create_device(&t, "mac-silent").await;
    // Give mac-a and mac-b fresh heartbeats; leave mac-silent without any heartbeat.
    for (dev, tok) in [("mac-a", &tok_a), ("mac-b", &tok_b)] {
        let (s, _) = send_as(
            &t,
            tok,
            "PUT",
            &format!("/v1/devices/{dev}/heartbeat"),
            None,
            Some(json!({ "runners": ["acp", "deterministic"], "repos": [REPO] })),
        )
        .await;
        assert_eq!(s, 200);
    }

    t.register(WORKER, Some(REPO), None).await;
    t.register(ASKER, Some(REPO), None).await;
    autopilot_board(&t, "edge", &[REPO], 2).await;

    // 1. Live managed run + pending sticky wake protects against premature AnswerGoneAsker,
    //    worker agents cannot read/ack device wakes (403), and terminal run update expires wakes.
    let ask_task = t.task(ASKER, "edge", "Waiting external ask").await;
    assert_eq!(t.claim(ASKER, &ask_task).await.0, 200);
    assert_eq!(
        t.patch(
            ASKER,
            &ask_task,
            json!({ "status": "blocked", "blocked_reason": "need decision" })
        )
        .await
        .0,
        200
    );
    let (s, run_v) = send_as(
        &t,
        &tok_a,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "board": "edge",
            "task_id": ask_task,
            "kind": "worker",
            "origin": "managed",
            "device": "mac-a",
            "runner": "acp",
            "agent_id": ASKER,
            "state": "running"
        })),
    )
    .await;
    assert_eq!(s, 200, "{run_v}");
    let run_id = run_v["id"].as_str().unwrap().to_owned();
    let (s, _) = send_as(
        &t,
        &tok_a,
        "PATCH",
        &format!("/v1/runs/{run_id}"),
        None,
        Some(json!({ "state": "waiting_external", "lease_ttl_secs": 3600 })),
    )
    .await;
    assert_eq!(s, 200);
    let ask_id = t
        .ok(
            "POST",
            "/v1/asks",
            Some(ASKER),
            Some(json!({
                "agent": ASKER,
                "board": "edge",
                "task": ask_task,
                "kind": "decision",
                "question": "Pick one",
                "options": ["A", "B"]
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    // Mark ASKER turn ended (simulating turn finishing while run stays in waiting_external).
    t.ok(
        "POST",
        &format!("/v1/agents/{ASKER}/state"),
        Some(ASKER),
        Some(json!({ "state": "ended", "paused_until": null })),
    )
    .await;
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{ask_id}/answer"),
            Some(json!({ "answer": "A" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);

    // Ordinary worker agent on mac-a is forbidden from reading or acking mac-a's wakes!
    let (s, wakes) = send_as(
        &t,
        &tok_a,
        "GET",
        "/v1/devices/mac-a/wakes",
        Some(WORKER),
        None,
    )
    .await;
    assert_eq!(s, 403, "worker must not read device wakes: {wakes}");
    let (s, wakes) = send_as(&t, &tok_a, "GET", "/v1/devices/mac-a/wakes", None, None).await;
    assert_eq!(s, 200, "{wakes}");
    let wake_id = wakes[0]["id"].as_i64().unwrap();
    let (s, ack_err) = send_as(
        &t,
        &tok_a,
        "POST",
        &format!("/v1/wakes/{wake_id}/ack"),
        Some(WORKER),
        None,
    )
    .await;
    assert_eq!(s, 403, "worker must not ack device wakes: {ack_err}");

    // Another device cannot hijack AnswerGoneAsker while mac-a's managed run is live!
    let (s, resp) = claim_as(
        &t,
        &tok_b,
        json!({
            "device": "mac-b",
            "free_units": 4,
            "runners": ["deterministic"],
            "repos": [REPO],
            "supported_kinds": ["housekeeping"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert!(
        resp["claimed"].as_array().unwrap().is_empty(),
        "live waiting_external run must suppress AnswerGoneAsker: {resp}"
    );

    // Cancelling the run immediately expires its pending sticky wake (visible immediately
    // without waiting for sweep_expired).
    let (s, _) = send_as(
        &t,
        &tok_a,
        "PATCH",
        &format!("/v1/runs/{run_id}"),
        None,
        Some(json!({ "state": "cancelled" })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, wakes) = send_as(&t, &tok_a, "GET", "/v1/devices/mac-a/wakes", None, None).await;
    assert_eq!(s, 200);
    assert!(
        wakes.as_array().unwrap().is_empty(),
        "terminal run transition must expire pending wakes immediately: {wakes}"
    );

    // 2. Two gone-asker answered asks on the SAME task: max_claims=4 leases at most ONE
    //    housekeeping run for that task in a single dispatch call.
    let ask_id_2 = t
        .ok(
            "POST",
            "/v1/asks",
            Some(ASKER),
            Some(json!({
                "agent": ASKER,
                "board": "edge",
                "task": ask_task,
                "kind": "decision",
                "question": "Pick second",
                "options": ["C", "D"]
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{ask_id_2}/answer"),
            Some(json!({ "answer": "C" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    // Creating ask_id_2 touched ASKER; mark ASKER ended again so both answered asks are orphaned.
    t.ok(
        "POST",
        &format!("/v1/agents/{ASKER}/state"),
        Some(ASKER),
        Some(json!({ "state": "ended", "paused_until": null })),
    )
    .await;
    let (s, resp) = claim_as(
        &t,
        &tok_b,
        json!({
            "device": "mac-b",
            "free_units": 8,
            "runners": ["deterministic"],
            "repos": [REPO],
            "supported_kinds": ["housekeeping"],
            "max_claims": 4
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(
        resp["claimed"].as_array().unwrap().len(),
        1,
        "must not lease two concurrent housekeeping runs for the same task: {resp}"
    );

    // 3. Quota-paused waiting_external run is preserved past sticky_grace_secs while a
    //    never-heartbeated device's waiting_external run IS orphaned.
    let paused_agent = "t3:paused-quota";
    t.register(paused_agent, Some(REPO), None).await;
    let reset_at = t.state.now() + TimeDelta::hours(2);
    t.ok(
        "POST",
        &format!("/v1/agents/{paused_agent}/state"),
        Some(paused_agent),
        Some(json!({ "state": "paused", "paused_until": super::iso(reset_at) })),
    )
    .await;
    let (s, paused_run) = send_as(
        &t,
        &tok_a,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "board": "edge",
            "kind": "worker",
            "origin": "managed",
            "device": "mac-a",
            "runner": "acp",
            "agent_id": paused_agent,
            "state": "running"
        })),
    )
    .await;
    assert_eq!(s, 200, "{paused_run}");
    let paused_run_id = paused_run["id"].as_str().unwrap().to_owned();
    let (s, _) = send_as(
        &t,
        &tok_a,
        "PATCH",
        &format!("/v1/runs/{paused_run_id}"),
        None,
        Some(json!({ "state": "waiting_external", "lease_ttl_secs": 3600 })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, silent_run) = send_as(
        &t,
        &tok_never_hb,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "board": "edge",
            "kind": "worker",
            "origin": "managed",
            "device": "mac-silent",
            "runner": "acp",
            "state": "running"
        })),
    )
    .await;
    assert_eq!(s, 200, "{silent_run}");
    let silent_run_id = silent_run["id"].as_str().unwrap().to_owned();
    let (s, _) = send_as(
        &t,
        &tok_never_hb,
        "PATCH",
        &format!("/v1/runs/{silent_run_id}"),
        None,
        Some(json!({ "state": "waiting_external", "lease_ttl_secs": 3600 })),
    )
    .await;
    assert_eq!(s, 200);

    // Advance 11m (> sticky_grace_secs=10m, < paused_until=2h): sweep_expired must keep
    // paused_run_id alive and orphan silent_run_id!
    t.advance(TimeDelta::minutes(11));
    let swept = t
        .state
        .write(crate::ops::runs::sweep_expired)
        .await
        .expect("sweep");
    assert!(swept >= 1);
    let (_, paused_after) = send_as(
        &t,
        &tok_a,
        "GET",
        &format!("/v1/runs/{paused_run_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(
        paused_after["state"], "waiting_external",
        "quota-paused waiting_external run must survive sticky grace"
    );
    let (_, silent_after) = send_as(
        &t,
        &tok_never_hb,
        "GET",
        &format!("/v1/runs/{silent_run_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(
        silent_after["state"], "orphaned",
        "never-heartbeated stale device run must trigger sticky fallback"
    );

    // 4. Failed/orphaned goal run does NOT suppress retrying goal review on another device.
    t.ok(
        "POST",
        "/v1/boards",
        None,
        Some(json!({
            "slug": "goal-retry",
            "title": "Goal Retry",
            "goal": "Ship v1",
            "autopilot": true,
            "repos": [REPO]
        })),
    )
    .await;
    let gtask = t.task(WORKER, "goal-retry", "Only task").await;
    assert_eq!(t.claim(WORKER, &gtask).await.0, 200);
    assert_eq!(t.done(WORKER, &gtask, check(0)).await.0, 200);
    // Refresh mac-b heartbeat and claim goal run.
    let (s, _) = send_as(
        &t,
        &tok_b,
        "PUT",
        "/v1/devices/mac-b/heartbeat",
        None,
        Some(json!({ "runners": ["acp", "deterministic"], "repos": [REPO] })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, r) = claim_as(
        &t,
        &tok_b,
        json!({
            "device": "mac-b",
            "free_units": 8,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["goal"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    let goal_run_1 = r["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    // Fail the first goal run -> goal review must become claimable again!
    let (s, _) = send_as(
        &t,
        &tok_b,
        "PATCH",
        &format!("/v1/runs/{goal_run_1}"),
        None,
        Some(json!({ "state": "starting" })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = send_as(
        &t,
        &tok_b,
        "PATCH",
        &format!("/v1/runs/{goal_run_1}"),
        None,
        Some(json!({ "state": "failed" })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, r) = claim_as(
        &t,
        &tok_b,
        json!({
            "device": "mac-b",
            "free_units": 8,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["goal"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(
        r["claimed"][0]["run"]["kind"], "goal",
        "failed goal run must not suppress retry: {r}"
    );
    assert_ne!(r["claimed"][0]["run"]["id"], goal_run_1);

    // 5. ReleaseAwayOwner does not sum idle_doing notices across two distinct spells for a
    //    running agent that heartbeats in between.
    autopilot_board(&t, "spells", &[REPO], 2).await;
    let runner_agent = "t3:spell-runner";
    t.register(runner_agent, Some(REPO), None).await;
    let stask = t.task(runner_agent, "spells", "Spell task").await;
    assert_eq!(t.claim(runner_agent, &stask).await.0, 200);
    // Spell 1: silent 31m -> 1st idle_doing notice.
    t.advance(TimeDelta::minutes(31));
    assert_eq!(
        t.state
            .write(crate::ops::tasks::announce_idle_doing)
            .await
            .unwrap(),
        1
    );
    // Heartbeat -> starts a fresh spell without emitting agent_state_changed.
    t.advance(TimeDelta::minutes(4));
    t.register(runner_agent, Some(REPO), None).await;
    // Spell 2: silent 31m -> 1st notice of spell 2, then wait 6m (> 5m release grace).
    t.advance(TimeDelta::minutes(31));
    assert_eq!(
        t.state
            .write(crate::ops::tasks::announce_idle_doing)
            .await
            .unwrap(),
        1
    );
    t.advance(TimeDelta::minutes(6));
    let (s, r) = claim_as(
        &t,
        &tok_b,
        json!({
            "device": "mac-b",
            "free_units": 8,
            "runners": ["deterministic"],
            "repos": [REPO],
            "supported_kinds": ["housekeeping"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    assert!(
        r["claimed"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["task_id"] != stask),
        "must not sum idle_doing across distinct spells: {r}"
    );

    // 6. Claiming a queued command run from another device clears cross-device `dispatch:<other>`.
    let (s, qcmd) = send_as(
        &t,
        &tok_a,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "board": "spells",
            "kind": "command",
            "origin": "managed",
            "device": "mac-a",
            "runner": "deterministic",
            "agent_id": "dispatch:mac-a",
            "state": "queued",
            "cost_units": 1,
            "title": "Cross-device queued cmd"
        })),
    )
    .await;
    assert_eq!(s, 200, "{qcmd}");
    let qcmd_id = qcmd["id"].as_str().unwrap().to_owned();
    let (s, r) = claim_as(
        &t,
        &tok_b,
        json!({
            "device": "mac-b",
            "free_units": 4,
            "runners": ["deterministic"],
            "repos": [REPO],
            "supported_kinds": ["command"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"][0]["run"]["id"], qcmd_id);
    assert_eq!(r["claimed"][0]["run"]["device"], "mac-b");
    assert!(
        r["claimed"][0]["run"]["agent_id"].is_null(),
        "claiming queued command on another device must clear cross-device dispatch agent_id: {r}"
    );

    // 7. Raising board.parallel_tasks preserves explicit cross-repo locks and stricter run locks.
    let other_repo = "github.com/example/other-repo";
    autopilot_board(&t, "lock-sync", &[REPO], 2).await;
    let ltask = t
        .task_with(
            WORKER,
            json!({
                "board": "lock-sync",
                "title": "Explicit lock task",
                "conflict_keys": [{ "key": format!("repo:{other_repo}:write"), "max": 4 }]
            }),
        )
        .await;
    let (s, lrun) = send_as(
        &t,
        &tok_a,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "board": "lock-sync",
            "task_id": ltask,
            "kind": "worker",
            "origin": "managed",
            "device": "mac-a",
            "runner": "acp",
            "state": "running",
            "conflict_keys": [
                { "key": format!("repo:{REPO}:write"), "max": 1 },
                { "key": format!("repo:{other_repo}:write"), "max": 4 }
            ]
        })),
    )
    .await;
    assert_eq!(s, 200, "{lrun}");
    // Raise parallel_tasks 2 -> 4, then lower 4 -> 1:
    // - stricter run-level lock on REPO (max=1) must NEVER widen to 4
    // - explicit cross-repo lock on other_repo (max=4) must NEVER shrink to 1
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/lock-sync",
            Some(json!({ "parallel_tasks": 4 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    let read_locks = || async {
        t.state
            .read(move |cx| {
                let mut stmt = cx.db.prepare(
                    "SELECT key, max_holders FROM conflict_locks \
                     WHERE key IN (?1, ?2) ORDER BY key",
                )?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![
                            format!("repo:{REPO}:write"),
                            format!("repo:{other_repo}:write")
                        ],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap()
    };
    let expected_locks = vec![
        (format!("repo:{REPO}:write"), 1),
        (format!("repo:{other_repo}:write"), 4),
    ];
    assert_eq!(
        read_locks().await,
        expected_locks,
        "raising parallel_tasks must not widen stricter run-level lock"
    );
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/lock-sync",
            Some(json!({ "parallel_tasks": 1 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    assert_eq!(
        read_locks().await,
        expected_locks,
        "lowering parallel_tasks must not shrink explicit cross-repo lock"
    );
}

#[tokio::test]
async fn dispatch_claim_honors_device_runners_preference_order() {
    let t = T::new();
    let t3_first_tok = create_device(&t, "mac-t3-first").await;
    let acp_first_tok = create_device(&t, "mac-acp-first").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "runner-order", &[REPO], 4).await;

    // 1. Device advertising ["t3", "acp", "deterministic"] gets "t3" when task has no preferred_runner.
    let task_a = t
        .task(WORKER, "runner-order", "Task A (default runner)")
        .await;
    let (s, resp) = claim_as(
        &t,
        &t3_first_tok,
        json!({
            "device": "mac-t3-first",
            "free_units": 4,
            "runners": ["t3", "acp", "deterministic"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1, "{resp}");
    assert_eq!(resp["claimed"][0]["task_id"], task_a);
    assert_eq!(
        resp["claimed"][0]["run"]["runner"], "t3",
        "must honor device runners order [\"t3\", \"acp\", \"deterministic\"]"
    );

    // 2. Device advertising ["acp", "t3", "deterministic"] gets "acp".
    let task_b = t
        .task(WORKER, "runner-order", "Task B (default runner)")
        .await;
    let (s, resp) = claim_as(
        &t,
        &acp_first_tok,
        json!({
            "device": "mac-acp-first",
            "free_units": 4,
            "runners": ["acp", "t3", "deterministic"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1, "{resp}");
    assert_eq!(resp["claimed"][0]["task_id"], task_b);
    assert_eq!(
        resp["claimed"][0]["run"]["runner"], "acp",
        "must honor device runners order [\"acp\", \"t3\", \"deterministic\"]"
    );

    // 3. Explicit task preferred_runner still wins when available; falls back to device order
    //    when unavailable (skipping non-LLM runners like "deterministic").
    let task_c = t
        .task_with(
            WORKER,
            json!({
                "board": "runner-order",
                "title": "Task C (explicit acp)",
                "preferred_runner": "acp",
            }),
        )
        .await;
    let (s, resp) = claim_as(
        &t,
        &t3_first_tok,
        json!({
            "device": "mac-t3-first",
            "free_units": 4,
            "runners": ["t3", "acp", "deterministic"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"][0]["task_id"], task_c);
    assert_eq!(resp["claimed"][0]["run"]["runner"], "acp");

    let task_d = t
        .task_with(
            WORKER,
            json!({
                "board": "runner-order",
                "title": "Task D (unavailable preferred_runner)",
                "preferred_runner": "other",
            }),
        )
        .await;
    let (s, resp) = claim_as(
        &t,
        &t3_first_tok,
        json!({
            "device": "mac-t3-first",
            "free_units": 4,
            "runners": ["deterministic", "t3", "acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"][0]["task_id"], task_d);
    assert_eq!(
        resp["claimed"][0]["run"]["runner"], "t3",
        "must skip deterministic and pick first supported LLM runner in slice order"
    );
}

#[tokio::test]
async fn dispatch_claim_enforces_usage_buckets_pace_limit_and_legacy_fallback() {
    let t = T::new();
    let tok = create_device(&t, "mac-ag").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    autopilot_board(&t, "quota-test", &[REPO], 2).await;

    // Set board usage_stop_at = 80 and legacy reserve = 5.0%.
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/quota-test",
            Some(json!({ "usage_stop_at": 80 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let (s, v) = t
        .as_user("PUT", "/v1/usage/reserve", Some(json!({ "reserve": 5.0 })))
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");

    let _worker_task = t.task(WORKER, "quota-test", "LLM task held by quota").await;
    let merge_task = make_reviewed_task(
        &t,
        "quota-test",
        "Reviewed PR ready to merge",
        Some("review: pass\nPR: https://github.com/example/repo/pull/99"),
    )
    .await;

    let now = t.state.now();
    let week_reset = (now + TimeDelta::days(3)).to_rfc3339();
    let five_reset = (now + TimeDelta::hours(4)).to_rfc3339();

    // 1. Legacy fallback: antigravity/default has NO reading yet, while anthropic/default
    //    is at 85% five_hour (>= usage_stop_at=80). Claiming with quota_bucket=antigravity/default
    //    MUST fall back to anthropic/default and hold LLM worker with Quota!
    let (s, v) = send_as(
        &t,
        &tok,
        "PUT",
        "/v1/usage",
        None,
        Some(json!({
            "five_hour": { "percent": 85.0, "resets_at": five_reset },
            "seven_day": { "percent": 20.0, "resets_at": week_reset },
            "read_at": now.to_rfc3339(),
        })),
    )
    .await;
    assert_eq!(s, 200, "{v}");

    let claim_llm_only = json!({
        "device": "mac-ag",
        "free_units": 4,
        "runners": ["acp", "deterministic"],
        "capabilities": ["git:cloneable"],
        "repos": [REPO],
        "supported_kinds": ["worker"],
        "quota_bucket": { "provider": "antigravity", "account": "default" },
    });
    let (s, resp) = claim_as(&t, &tok, claim_llm_only.clone()).await;
    assert_eq!(s, 200, "{resp}");
    assert!(
        resp["claimed"].as_array().unwrap().is_empty(),
        "fallback to legacy bucket must hold LLM work over usage_stop_at: {resp}"
    );
    assert_eq!(resp["holds_summary"][0]["reason"], "quota", "{resp}");

    // 2. Raise usage_stop_at to 98 so weekly pace (not usage_stop_at) is what holds at 96%.
    //    Report antigravity/default at 96% seven_day (>= 100 - 5.0 = 95% hard line -> pace_limit=0).
    //    First reading inherits legacy 5.0% reserve so pace activates immediately!
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/quota-test",
            Some(json!({ "usage_stop_at": 98 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let (s, bucket) = send_as(
        &t,
        &tok,
        "PUT",
        "/v1/usage/antigravity/default",
        None,
        Some(json!({
            "five_hour": { "percent": 10.0, "resets_at": five_reset },
            "seven_day": { "percent": 96.0, "resets_at": week_reset },
            "read_at": now.to_rfc3339(),
        })),
    )
    .await;
    assert_eq!(s, 200, "{bucket}");
    assert_eq!(bucket["reserve"], 5.0, "{bucket}");
    assert!(bucket["pace"]["hard_since"].is_string(), "{bucket}");

    // LLM worker is held by weekly pace_limit = 0 (`DispatchHoldReason::Quota`).
    let (s, resp) = claim_as(&t, &tok, claim_llm_only.clone()).await;
    assert_eq!(s, 200, "{resp}");
    assert!(resp["claimed"].as_array().unwrap().is_empty(), "{resp}");
    assert_eq!(resp["holds_summary"][0]["reason"], "quota", "{resp}");
    assert!(
        resp["holds_summary"][0]["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("pace limit (0)"),
        "expected pace_limit=0 hold detail: {resp}"
    );

    // Zero-LLM deterministic merge run MUST STILL BE CLAIMED even while pace_limit = 0!
    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-ag",
            "free_units": 4,
            "runners": ["acp", "deterministic"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["worker", "merge"],
            "quota_bucket": { "provider": "antigravity", "account": "default" },
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["kind"], "merge", "{resp}");
    assert_eq!(resp["claimed"][0]["task_id"], merge_task, "{resp}");

    // 3. Once antigravity/default drops below both usage_stop_at and pace line, LLM worker proceeds
    //    even though legacy anthropic/default is still at 85% (target bucket overrides legacy).
    t.advance(TimeDelta::minutes(1));
    let later = t.state.now();
    let (s, bucket) = send_as(
        &t,
        &tok,
        "PUT",
        "/v1/usage/antigravity/default",
        None,
        Some(json!({
            "five_hour": { "percent": 15.0, "resets_at": five_reset },
            "seven_day": { "percent": 20.0, "resets_at": week_reset },
            "read_at": later.to_rfc3339(),
        })),
    )
    .await;
    assert_eq!(s, 200, "{bucket}");
    let (s, resp) = claim_as(&t, &tok, claim_llm_only).await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["kind"], "worker", "{resp}");
}

#[tokio::test]
async fn dispatch_claim_holds_llm_on_stale_measured_weekly_pace_breach_until_next_day_or_reset() {
    let t = T::new();
    let tok = create_device(&t, "mac-ag-stale").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    autopilot_board(&t, "pace-stale", &[REPO], 2).await;

    // Set reserve = 4.0% on antigravity/default so 6-day pace line from anchor 0.0% is exactly 16.0%.
    let (s, v) = t
        .as_user(
            "PUT",
            "/v1/usage/antigravity/default/reserve",
            Some(json!({ "reserve": 4.0 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");

    let _worker_a = t.task(WORKER, "pace-stale", "LLM worker A").await;
    let _worker_b = t.task(WORKER, "pace-stale", "LLM worker B").await;
    let merge_task = make_reviewed_task(
        &t,
        "pace-stale",
        "Reviewed PR ready to merge while stale pace holds",
        Some("review: pass\nPR: https://github.com/example/repo/pull/108"),
    )
    .await;

    let t0 = t.state.now();
    // 5 days 22 hours until reset -> pace_day starts 2h before t0, next_day is 22h after t0.
    let week_reset = t0 + TimeDelta::days(6) - TimeDelta::hours(2);
    let five_reset = t0 + TimeDelta::hours(4);

    // Anchor the pace day at 0.0% -> line = (100 - 4) / 6 = 16.0%.
    let (s, bucket) = send_as(
        &t,
        &tok,
        "PUT",
        "/v1/usage/antigravity/default",
        None,
        Some(json!({
            "five_hour": { "percent": 5.0, "resets_at": five_reset.to_rfc3339() },
            "seven_day": { "percent": 0.0, "resets_at": week_reset.to_rfc3339() },
            "read_at": t0.to_rfc3339(),
        })),
    )
    .await;
    assert_eq!(s, 200, "{bucket}");
    assert!((bucket["pace"]["line"].as_f64().unwrap() - 16.0).abs() < 0.001);

    // Second reading breaches today's line by measured weekly usage: 32.6% >= 16.0%.
    t.advance(TimeDelta::minutes(1));
    let read_at = t.state.now();
    let (s, bucket) = send_as(
        &t,
        &tok,
        "PUT",
        "/v1/usage/antigravity/default",
        None,
        Some(json!({
            "five_hour": { "percent": 20.0, "resets_at": five_reset.to_rfc3339() },
            "seven_day": { "percent": 32.6, "resets_at": week_reset.to_rfc3339() },
            "read_at": read_at.to_rfc3339(),
        })),
    )
    .await;
    assert_eq!(s, 200, "{bucket}");

    // Advance 45 minutes (> USAGE_FRESH_MINUTES = 30m) while still inside the same pace day.
    t.advance(TimeDelta::minutes(45));

    let claim_llm = json!({
        "device": "mac-ag-stale",
        "free_units": 4,
        "runners": ["acp", "deterministic"],
        "capabilities": ["git:cloneable"],
        "repos": [REPO],
        "supported_kinds": ["worker"],
        "quota_bucket": { "provider": "antigravity", "account": "default" },
    });

    // Stale reading with measured 32.6% >= 16.0% still holds LLM runs with Quota!
    let (s, resp) = claim_as(&t, &tok, claim_llm.clone()).await;
    assert_eq!(s, 200, "{resp}");
    assert!(
        resp["claimed"].as_array().unwrap().is_empty(),
        "stale measured pace breach in same day must hold LLM worker: {resp}"
    );
    assert_eq!(resp["holds_summary"][0]["reason"], "quota", "{resp}");
    assert!(
        resp["holds_summary"][0]["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("pace limit (0)"),
        "expected pace_limit=0 hold detail: {resp}"
    );

    // Zero-LLM merge run MUST STILL BE CLAIMED while stale pace holds LLM runs.
    let (s, resp) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-ag-stale",
            "free_units": 4,
            "runners": ["acp", "deterministic"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["worker", "merge"],
            "quota_bucket": { "provider": "antigravity", "account": "default" },
        }),
    )
    .await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["kind"], "merge", "{resp}");
    assert_eq!(resp["claimed"][0]["task_id"], merge_task, "{resp}");

    // Once the next pace day begins (t0 + 22h), the stale same-day line hold releases!
    t.advance(TimeDelta::hours(22));
    let (s, resp) = claim_as(&t, &tok, claim_llm.clone()).await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["kind"], "worker", "{resp}");

    // Now report a reading breaching the hard reserve (96.0% >= 100 - 4.0) and raise
    // usage_stop_at so only pace_limit holds. Even across the next pace day (>24h later,
    // stale), hard reserve keeps holding until week_reset.
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/pace-stale",
            Some(json!({ "usage_stop_at": 99 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let hard_read_at = t.state.now();
    let (s, bucket) = send_as(
        &t,
        &tok,
        "PUT",
        "/v1/usage/antigravity/default",
        None,
        Some(json!({
            "five_hour": { "percent": 10.0, "resets_at": (hard_read_at + TimeDelta::hours(4)).to_rfc3339() },
            "seven_day": { "percent": 96.0, "resets_at": week_reset.to_rfc3339() },
            "read_at": hard_read_at.to_rfc3339(),
        })),
    )
    .await;
    assert_eq!(s, 200, "{bucket}");

    // Advance 25 hours (stale + past next pace day, still before week_reset): still held!
    t.advance(TimeDelta::hours(25));
    let (s, resp) = claim_as(&t, &tok, claim_llm.clone()).await;
    assert_eq!(s, 200, "{resp}");
    assert!(
        resp["claimed"].as_array().unwrap().is_empty(),
        "stale hard reserve breach must keep holding across pace days until reset: {resp}"
    );
    assert_eq!(resp["holds_summary"][0]["reason"], "quota", "{resp}");

    // Advance past week_reset: hard reserve hold releases!
    t.advance(TimeDelta::days(5));
    let (s, resp) = claim_as(&t, &tok, claim_llm).await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["kind"], "worker", "{resp}");
}

#[tokio::test]
async fn dispatch_claim_routes_after_merge_checks_from_merge_to_review_and_back_to_merge() {
    let t = T::new();
    let tok = create_device(&t, "mac-after").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    autopilot_board(&t, "after-merge", &[REPO], 2).await;

    // Custom (non-upgrade.sh) after-merge check routes to the LLM `AfterMerge` review judge.
    let task_id = make_reviewed_task(
        &t,
        "after-merge",
        "Deploy-checked change",
        Some(
            "review: pass. Code looks good.\ncheck: cargo test => 0\nafter merge: verify live /healthz and custom migration smoke test\npr: https://github.com/example/repo/pull/300",
        ),
    )
    .await;

    let claim_body = json!({
        "device": "mac-after",
        "free_units": 4,
        "runners": ["acp", "deterministic"],
        "capabilities": ["git:cloneable"],
        "repos": [REPO],
        "supported_kinds": ["review", "merge"],
    });

    // 1. Before the merged handoff note: server dispatches `merge` to merge the PR first.
    let (s, resp) = claim_as(&t, &tok, claim_body.clone()).await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["kind"], "merge", "{resp}");
    let merge_run_id = resp["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();

    // Dispatcher merges PR, writes the Trail Rule handoff note, and finishes the merge run.
    let dispatch_agent = "dispatch:mac-after";
    let (s, _) = send_as(
        &t,
        &tok,
        "POST",
        "/v1/agents",
        Some(dispatch_agent),
        Some(json!({
            "id": dispatch_agent,
            "device": "mac-after",
            "runtime": "claude",
            "role": "coordinator",
        })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = send_as(
        &t,
        &tok,
        "PATCH",
        &format!("/v1/runs/{merge_run_id}"),
        None,
        Some(json!({ "state": "starting", "agent_id": dispatch_agent })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = send_as(
        &t,
        &tok,
        "PATCH",
        &format!("/v1/runs/{merge_run_id}"),
        None,
        Some(json!({ "state": "running", "agent_id": dispatch_agent })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, note_resp) = send_as(
        &t,
        &tok,
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(dispatch_agent),
        Some(json!({
            "agent": dispatch_agent,
            "kind": "note",
            "text": "note: merged; waiting for after-merge checks (pass #1):\npr: https://github.com/example/repo/pull/300\nbase: main\nleft: deploy/upgrade.sh --yes",
        })),
    )
    .await;
    assert_eq!(s, 200, "{note_resp}");
    let (s, _) = send_as(
        &t,
        &tok,
        "PATCH",
        &format!("/v1/runs/{merge_run_id}"),
        None,
        Some(json!({
            "state": "succeeded",
            "outcome": {
                "status": "awaiting_after_merge",
                "pr_url": "https://github.com/example/repo/pull/300",
            },
        })),
    )
    .await;
    assert_eq!(s, 200);

    // 2. After the merged handoff note: server dispatches `review` titled `after-merge ...`.
    let (s, resp) = claim_as(&t, &tok, claim_body.clone()).await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["kind"], "review", "{resp}");
    assert!(
        resp["claimed"][0]["run"]["title"]
            .as_str()
            .unwrap()
            .starts_with("after-merge "),
        "{resp}"
    );
    let after_run_id = resp["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();

    // After-merge judge starts, posts a clean `review: pass` (no `after merge:` lines), ends.
    let after_reviewer = "t3:after-reviewer";
    t.register(after_reviewer, Some(REPO), None).await;
    let (s, _) = send_as(
        &t,
        &tok,
        "PATCH",
        &format!("/v1/runs/{after_run_id}"),
        None,
        Some(json!({ "state": "starting", "agent_id": after_reviewer })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = send_as(
        &t,
        &tok,
        "PATCH",
        &format!("/v1/runs/{after_run_id}"),
        None,
        Some(json!({ "state": "running", "agent_id": after_reviewer })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, pass2) = send_as(
        &t,
        &tok,
        "POST",
        &format!("/v1/tasks/{task_id}/notes"),
        Some(after_reviewer),
        Some(json!({
            "agent": after_reviewer,
            "kind": "decision",
            "text": "review: pass. Deployed cleanly.\ncheck: deploy/upgrade.sh --yes => 0\npr: https://github.com/example/repo/pull/300",
        })),
    )
    .await;
    assert_eq!(s, 200, "{pass2}");
    let (s, _) = send_as(
        &t,
        &tok,
        "PATCH",
        &format!("/v1/runs/{after_run_id}"),
        None,
        Some(json!({ "state": "succeeded" })),
    )
    .await;
    assert_eq!(s, 200);

    // 3. Now that a clean pass stands without `after merge:` lines: server dispatches `merge`
    //    to close the task.
    let (s, resp) = claim_as(&t, &tok, claim_body).await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1, "{resp}");
    assert_eq!(resp["claimed"][0]["run"]["kind"], "merge", "{resp}");
    let close_run_id = resp["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    let (s, _) = send_as(
        &t,
        &tok,
        "PATCH",
        &format!("/v1/runs/{close_run_id}"),
        None,
        Some(json!({ "state": "running", "agent_id": dispatch_agent })),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = send_as(
        &t,
        &tok,
        "PATCH",
        &format!("/v1/runs/{close_run_id}"),
        None,
        Some(json!({
            "state": "succeeded",
            "outcome": {
                "status": "merged_and_done",
                "pr_url": "https://github.com/example/repo/pull/300",
            },
        })),
    )
    .await;
    assert_eq!(s, 200);

    // 4. Verify run_outcomes / scheduler stats: `awaiting_after_merge` does not record
    //    merge-completion metrics, and the clean two-stage pass counts as 1 review round
    //    with first_pass_merge_rate == 1.0.
    let (s, stats) = t
        .as_user("GET", "/v1/stats/scheduler?board=after-merge", None)
        .await;
    assert_eq!(s.as_u16(), 200, "{stats}");
    assert_eq!(stats["first_pass_merge_rate"], json!(1.0), "{stats}");
    assert_eq!(stats["avg_review_rounds"], json!(1.0), "{stats}");
}

#[tokio::test]
async fn housekeeping_releases_late_first_idle_notice_ended_during_grace_and_skips_active_delegate()
{
    let t = T::new();
    let tok = create_device(&t, "runner-hk").await;
    autopilot_board(&t, "hk-late", &[REPO], 2).await;

    let claim_hk = || {
        json!({
            "device": "runner-hk",
            "free_units": 8,
            "runners": ["deterministic"],
            "repos": [REPO],
            "supported_kinds": ["housekeeping"]
        })
    };

    // 1. Late first `idle_doing` notice (arriving at >= 2 * doing_idle_secs): the server emits
    //    only ONE notice and never sends a separate repeat, so housekeeping must still release
    //    once the 5-minute grace window passes — even if T3 closes the idle session (`ended`,
    //    which bumps `last_seen`) during that grace window.
    let late_owner = "t3:late-owner";
    t.register(late_owner, Some(REPO), None).await;
    let late_task = t
        .task(late_owner, "hk-late", "Late first notice task")
        .await;
    assert_eq!(t.claim(late_owner, &late_task).await.0, 200);
    t.ok(
        "POST",
        &format!("/v1/agents/{late_owner}/state"),
        Some(late_owner),
        Some(json!({ "state": "idle", "paused_until": null })),
    )
    .await;
    // Jump directly past twice `doing_idle_secs` (65m > 60m) before the first sweeper tick.
    t.advance(TimeDelta::minutes(65));
    assert_eq!(
        t.state
            .write(crate::ops::tasks::announce_idle_doing)
            .await
            .unwrap(),
        1
    );
    // Confirm the server emits no second notice for this spell.
    t.advance(TimeDelta::minutes(1));
    assert_eq!(
        t.state
            .write(crate::ops::tasks::announce_idle_doing)
            .await
            .unwrap(),
        0
    );
    // T3 closes the idle session during the grace window, updating `last_seen > noticed_at`.
    t.advance(TimeDelta::minutes(1));
    t.ok(
        "POST",
        &format!("/v1/agents/{late_owner}/state"),
        Some(late_owner),
        Some(json!({ "state": "ended", "paused_until": null })),
    )
    .await;
    // Still inside the 5-minute grace window (2m elapsed): not claimable yet.
    let (s, resp) = claim_as(&t, &tok, claim_hk()).await;
    assert_eq!(s, 200, "{resp}");
    assert!(
        resp["claimed"].as_array().unwrap().is_empty(),
        "must wait out release grace after late first notice: {resp}"
    );
    // Past the 5-minute grace window (+4m -> 6m total since notice): claimed as ReleaseAwayOwner!
    t.advance(TimeDelta::minutes(4));
    let (s, resp) = claim_as(&t, &tok, claim_hk()).await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["claimed"].as_array().unwrap().len(), 1, "{resp}");
    assert_eq!(
        resp["claimed"][0]["housekeeping_action"]["action"],
        "release_away_owner"
    );
    assert_eq!(
        resp["claimed"][0]["housekeeping_action"]["task_id"],
        late_task
    );
    assert_eq!(
        resp["claimed"][0]["housekeeping_action"]["owner"],
        late_owner
    );

    // 2. An active delegated child (`t3_parent` pointing to the away owner) must suppress
    //    `ReleaseAwayOwner` even after the repeat notice + grace.
    let parent = "t3:parent-away";
    let child = "t3:child-working";
    t.register(parent, Some(REPO), None).await;
    let delegated_task = t.task(parent, "hk-late", "Task with active delegate").await;
    assert_eq!(t.claim(parent, &delegated_task).await.0, 200);
    t.ok(
        "POST",
        &format!("/v1/agents/{parent}/state"),
        Some(parent),
        Some(json!({ "state": "idle", "paused_until": null })),
    )
    .await;
    // Emit both idle notices while no delegate exists.
    t.advance(TimeDelta::minutes(31));
    assert_eq!(
        t.state
            .write(crate::ops::tasks::announce_idle_doing)
            .await
            .unwrap(),
        1
    );
    t.advance(TimeDelta::minutes(30));
    assert_eq!(
        t.state
            .write(crate::ops::tasks::announce_idle_doing)
            .await
            .unwrap(),
        1
    );
    // A delegated child starts running during the grace window.
    t.advance(TimeDelta::minutes(2));
    t.ok(
        "POST",
        "/v1/agents",
        Some(child),
        Some(json!({
            "id": child,
            "runtime": "claude",
            "device": "mac",
            "role": "worker",
            "repo": REPO,
            "native_ids": { "t3_parent": "parent-away" }
        })),
    )
    .await;
    // Past the 5-minute grace window: active delegate suppresses ReleaseAwayOwner.
    t.advance(TimeDelta::minutes(4));
    let (s, resp) = claim_as(&t, &tok, claim_hk()).await;
    assert_eq!(s, 200, "{resp}");
    assert!(
        resp["claimed"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["task_id"] != delegated_task),
        "active delegated child must suppress ReleaseAwayOwner: {resp}"
    );

    // 3. An `idle` owner that actively calls a board API during the grace window updates
    //    `last_seen > noticed_at` without transitioning to `running` (`touch_agent` only
    //    revives `ended`), so housekeeping must NOT release its task.
    let active_idle = "t3:active-idle-owner";
    t.register(active_idle, Some(REPO), None).await;
    let active_task = t
        .task(active_idle, "hk-late", "Idle owner active in grace")
        .await;
    assert_eq!(t.claim(active_idle, &active_task).await.0, 200);
    t.ok(
        "POST",
        &format!("/v1/agents/{active_idle}/state"),
        Some(active_idle),
        Some(json!({ "state": "idle", "paused_until": null })),
    )
    .await;
    t.advance(TimeDelta::minutes(65));
    assert_eq!(
        t.state
            .write(crate::ops::tasks::announce_idle_doing)
            .await
            .unwrap(),
        1
    );
    // Owner posts a note during the 5-minute grace window while still marked `idle`.
    t.advance(TimeDelta::minutes(2));
    t.ok(
        "POST",
        &format!("/v1/tasks/{active_task}/notes"),
        Some(active_idle),
        Some(json!({
            "agent": active_idle,
            "kind": "note",
            "text": "Still working during grace"
        })),
    )
    .await;
    // Past the 5-minute grace window: active `idle` owner must keep its task.
    t.advance(TimeDelta::minutes(4));
    let (s, resp) = claim_as(&t, &tok, claim_hk()).await;
    assert_eq!(s, 200, "{resp}");
    assert!(
        resp["claimed"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["task_id"] != active_task),
        "idle owner that called board API during grace must not be released: {resp}"
    );
}

#[tokio::test]
async fn merge_per_task_cooldown_prevents_head_of_line_blocking_and_suppresses_unclear() {
    let t = T::new();
    let tok = create_device(&t, "mac-merge").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;

    let repo_a = "github.com/example/alpha";
    let repo_b = "github.com/example/beta";
    autopilot_board(&t, "alpha", &[repo_a], 2).await;
    autopilot_board(&t, "beta", &[repo_b], 2).await;

    // p1 task on alpha (scores #1) + p2 task on beta (scores #2).
    let p1_task = make_reviewed_task(
        &t,
        "alpha",
        "P1 failing checks PR",
        Some("review: pass. LGTM\npr: https://github.com/example/alpha/pull/1"),
    )
    .await;
    assert_eq!(
        t.patch(WORKER, &p1_task, json!({ "priority": "p1" }))
            .await
            .0,
        200
    );
    t.advance(TimeDelta::seconds(5));
    let p2_task = make_reviewed_task(
        &t,
        "beta",
        "P2 clean PR",
        Some("review: pass. LGTM\npr: https://github.com/example/beta/pull/2"),
    )
    .await;

    let merge_claim = || {
        json!({
            "device": "mac-merge",
            "free_units": 4,
            "runners": ["acp", "deterministic"],
            "capabilities": ["git:cloneable"],
            "repos": [repo_a, repo_b],
            "supported_kinds": ["merge"],
        })
    };

    // 1. First claim picks p1_task; it finishes as `deferred` ("checks failed ...").
    let (s, r1) = claim_as(&t, &tok, merge_claim()).await;
    assert_eq!(s, 200, "{r1}");
    assert_eq!(r1["claimed"][0]["task_id"], p1_task);
    let run1 = r1["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run1}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    // Simulate DeterministicRunner posting its own finding note during the run.
    t.ok(
        "POST",
        &format!("/v1/tasks/{p1_task}/notes"),
        Some(REVIEWER),
        Some(json!({ "agent": REVIEWER, "kind": "finding", "text": "finding: checks failed on abc" })),
    )
    .await;
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run1}"),
            None,
            Some(json!({
                "state": "succeeded",
                "outcome": { "status": "deferred", "reason": "checks failed on abc: rust" }
            })),
        )
        .await
        .0,
        200
    );

    // Immediately (t + 0s), p2_task on beta is claimed — zero Head-of-Line blocking!
    let (s, r2) = claim_as(&t, &tok, merge_claim()).await;
    assert_eq!(s, 200, "{r2}");
    assert_eq!(r2["claimed"].as_array().unwrap().len(), 1, "{r2}");
    assert_eq!(r2["claimed"][0]["task_id"], p2_task);
    let run2 = r2["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run2}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run2}"),
            None,
            Some(json!({ "state": "succeeded", "outcome": { "status": "merged_and_done" } })),
        )
        .await
        .0,
        200
    );
    assert_eq!(t.done(WORKER, &p2_task, check(0)).await.0.as_u16(), 200);

    // 2. Verify 300s cooldown for `checks failed`: skipped at 299s, re-offered at 300s.
    t.advance(TimeDelta::seconds(299));
    let (s, r_skip) = claim_as(&t, &tok, merge_claim()).await;
    assert_eq!(s, 200, "{r_skip}");
    assert!(
        r_skip["claimed"].as_array().unwrap().is_empty(),
        "checks-failed merge must stay in 300s cooldown at 299s: {r_skip}"
    );
    t.advance(TimeDelta::seconds(1));
    let (s, r_retry) = claim_as(&t, &tok, merge_claim()).await;
    assert_eq!(s, 200, "{r_retry}");
    assert_eq!(r_retry["claimed"][0]["task_id"], p1_task);
    let run3 = r_retry["claimed"][0]["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 3. Verify 60s cooldown for generic `deferred` ("checks still pending"): skipped at 59s,
    //    re-offered at 60s.
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run3}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run3}"),
            None,
            Some(json!({
                "state": "succeeded",
                "outcome": { "status": "deferred", "reason": "checks still pending" }
            })),
        )
        .await
        .0,
        200
    );
    t.advance(TimeDelta::seconds(59));
    let (s, r_skip2) = claim_as(&t, &tok, merge_claim()).await;
    assert_eq!(s, 200, "{r_skip2}");
    assert!(r_skip2["claimed"].as_array().unwrap().is_empty());
    t.advance(TimeDelta::seconds(1));
    let (s, r_retry2) = claim_as(&t, &tok, merge_claim()).await;
    assert_eq!(s, 200, "{r_retry2}");
    assert_eq!(r_retry2["claimed"][0]["task_id"], p1_task);
    let run4 = r_retry2["claimed"][0]["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 4. Verify 120s cooldown for `failed` merge run, broken early by a newer `note_added`.
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run4}"),
            None,
            Some(json!({ "state": "failed", "reason": "gh timeout" })),
        )
        .await
        .0,
        200
    );
    t.advance(TimeDelta::seconds(119));
    let (s, r_fail_skip) = claim_as(&t, &tok, merge_claim()).await;
    assert_eq!(s, 200, "{r_fail_skip}");
    assert!(r_fail_skip["claimed"].as_array().unwrap().is_empty());
    // Adding a note after `ended_at` breaks the failed cooldown immediately!
    t.ok(
        "POST",
        &format!("/v1/tasks/{p1_task}/notes"),
        Some(WORKER),
        Some(json!({ "agent": WORKER, "kind": "note", "text": "retried CI" })),
    )
    .await;
    let (s, r_fail_retry) = claim_as(&t, &tok, merge_claim()).await;
    assert_eq!(s, 200, "{r_fail_retry}");
    assert_eq!(r_fail_retry["claimed"][0]["task_id"], p1_task);
    let run5 = r_fail_retry["claimed"][0]["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 5. Verify `unclear` suppression (agent-board-699): stays suppressed across time AND
    //    plain notes until a newer `review: pass` note or newer move to `review` occurs!
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run5}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run5}"),
            None,
            Some(json!({
                "state": "succeeded",
                "outcome": {
                    "status": "unclear",
                    "reason": "https://github.com/other/repo/pull/37 is in none of the board's repositories"
                }
            })),
        )
        .await
        .0,
        200
    );
    t.advance(TimeDelta::hours(2));
    t.ok(
        "POST",
        &format!("/v1/tasks/{p1_task}/notes"),
        Some(WORKER),
        Some(json!({ "agent": WORKER, "kind": "finding", "text": "plain note does not clear unclear" })),
    )
    .await;
    let (s, r_unclear_skip) = claim_as(&t, &tok, merge_claim()).await;
    assert_eq!(s, 200, "{r_unclear_skip}");
    assert!(
        r_unclear_skip["claimed"].as_array().unwrap().is_empty(),
        "unclear merge outcome must stay suppressed until newer pass note or review move: {r_unclear_skip}"
    );

    // Posting a newer `review: pass` note immediately re-offers merge for p1_task!
    t.ok(
        "POST",
        &format!("/v1/tasks/{p1_task}/notes"),
        Some(REVIEWER),
        Some(json!({
            "agent": REVIEWER,
            "kind": "decision",
            "text": "review: pass. Fixed PR link.\npr: none"
        })),
    )
    .await;
    let (s, r_reoffer) = claim_as(&t, &tok, merge_claim()).await;
    assert_eq!(s, 200, "{r_reoffer}");
    assert_eq!(
        r_reoffer["claimed"][0]["task_id"], p1_task,
        "newer review: pass note must clear unclear suppression: {r_reoffer}"
    );
}

#[tokio::test]
async fn review_backlog_backpressure_holds_workers_but_allows_review_and_merge() {
    let t = T::new();
    let tok = create_device(&t, "mac-bp").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    // parallel_tasks=1 => review_backlog_limit = max(3, 1 * 2) = 3.
    autopilot_board(&t, "bp", &[REPO], 1).await;

    let worker_task = t.task(WORKER, "bp", "New ready worker task").await;
    let _rev1 = make_reviewed_task(&t, "bp", "Review 1", None).await;
    let _rev2 =
        make_reviewed_task(&t, "bp", "Review 2", Some("review: pass. LGTM\npr: none")).await;

    let claim_kinds = |kinds: &[&str]| {
        json!({
            "device": "mac-bp",
            "free_units": 8,
            "runners": ["acp", "deterministic"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": kinds,
        })
    };

    // With 2 tasks in review (< limit 3), Worker IS admitted.
    let (s, r) = claim_as(&t, &tok, claim_kinds(&["worker"])).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"][0]["task_id"], worker_task);
    let w_run = r["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{w_run}"),
            None,
            Some(json!({ "state": "cancelled" })),
        )
        .await
        .0,
        200
    );

    // Add a 3rd task in review (count 3 >= limit 3): Worker is HELD with Capacity!
    let rev3 = make_reviewed_task(&t, "bp", "Review 3", None).await;
    let (s, r_held) = claim_as(&t, &tok, claim_kinds(&["worker"])).await;
    assert_eq!(s, 200, "{r_held}");
    assert!(
        r_held["claimed"].as_array().unwrap().is_empty(),
        "worker must be held when review backlog >= limit: {r_held}"
    );
    assert_eq!(r_held["holds_summary"][0]["reason"], "capacity");
    assert!(
        r_held["holds_summary"][0]["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("paused new workers while 3 tasks wait in review (limit 3)"),
        "{r_held}"
    );

    // Review and Merge MUST STILL be admitted while workers are paused!
    let (s, r_drain) = claim_as(&t, &tok, claim_kinds(&["review", "merge"])).await;
    assert_eq!(s, 200, "{r_drain}");
    assert_eq!(r_drain["claimed"].as_array().unwrap().len(), 1, "{r_drain}");
    assert_eq!(r_drain["claimed"][0]["run"]["kind"], "merge");

    let (s, r_rev) = claim_as(&t, &tok, claim_kinds(&["review"])).await;
    assert_eq!(s, 200, "{r_rev}");
    assert_eq!(r_rev["claimed"].as_array().unwrap().len(), 1, "{r_rev}");
    assert_eq!(r_rev["claimed"][0]["run"]["kind"], "review");

    // Raising parallel_tasks=2 raises review_backlog_limit to max(3, 2 * 2) = 4, so 3 waiting
    // tasks now allow a Worker, while a 4th pauses Workers again until one drains.
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/bp",
            Some(json!({ "parallel_tasks": 2 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    let rev4 = make_reviewed_task(&t, "bp", "Review 4", None).await;
    let (s, r_held4) = claim_as(&t, &tok, claim_kinds(&["worker"])).await;
    assert_eq!(s, 200, "{r_held4}");
    assert!(r_held4["claimed"].as_array().unwrap().is_empty());
    assert!(
        r_held4["holds_summary"][0]["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("4 tasks wait in review (limit 4)"),
        "{r_held4}"
    );

    // Drain rev3 and rev4 -> review count drops below 4 (to 2) -> Worker immediately resumes!
    assert_eq!(t.done(WORKER, &rev3, check(0)).await.0.as_u16(), 200);
    assert_eq!(t.done(WORKER, &rev4, check(0)).await.0.as_u16(), 200);
    let (s, r_resumed) = claim_as(&t, &tok, claim_kinds(&["worker"])).await;
    assert_eq!(s, 200, "{r_resumed}");
    assert_eq!(r_resumed["claimed"][0]["task_id"], worker_task);
}

#[tokio::test]
async fn verdict_and_triage_batch_coalescing_debounce() {
    let t = T::new();
    let tok = create_device(&t, "runner-deb").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "b-wait", &[REPO], 2).await;
    autopilot_board(&t, "b-p1", &[REPO], 2).await;
    autopilot_board(&t, "b-batch", &[REPO], 2).await;
    // Disable daily `curate` on `b-wait` and `b-batch` to test immediate non-curate Verdict
    // batch coalescing for P2 proposals (`b-p1` keeps curate enabled to pin P1 fast-lane).
    for slug in ["b-wait", "b-batch"] {
        let (s, _) = t
            .as_user(
                "PATCH",
                &format!("/v1/boards/{slug}"),
                Some(json!({ "curate_every_secs": 0 })),
            )
            .await;
        assert_eq!(s.as_u16(), 200);
    }

    let claim_verdict = || {
        json!({
            "device": "runner-deb",
            "free_units": 8,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["verdict"],
            "max_claims": 4
        })
    };

    // 1. Single P2 proposal waits while < 90s.
    let wait_prop = t
        .task_with(
            WORKER,
            json!({ "board": "b-wait", "title": "Single P2 prop", "body": "Proposal: wait 90s" }),
        )
        .await;
    t.advance(TimeDelta::seconds(45));
    let (s, r) = claim_as(&t, &tok, claim_verdict()).await;
    assert_eq!(s, 200, "{r}");
    assert!(
        r["claimed"].as_array().unwrap().is_empty(),
        "single P2 proposal must wait within 90s debounce window: {r}"
    );

    // 2. Single P1 proposal dispatches immediately (< 90s).
    let p1_prop = t
        .task_with(
            WORKER,
            json!({
                "board": "b-p1",
                "title": "Urgent P1 prop",
                "body": "Proposal: urgent fix",
                "priority": "p1"
            }),
        )
        .await;
    let (s, r) = claim_as(&t, &tok, claim_verdict()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["claimed"][0]["task_id"], p1_prop);

    // 3. Three P2 proposals in the same group dispatch immediately as a 3-item batch.
    let mut batch_ids = Vec::new();
    for i in 1..=3 {
        batch_ids.push(
            t.task_with(
                WORKER,
                json!({
                    "board": "b-batch",
                    "title": format!("Batch prop {i}"),
                    "body": format!("Proposal: batch item {i}")
                }),
            )
            .await,
        );
    }
    let (s, r) = claim_as(&t, &tok, claim_verdict()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["claimed"][0]["board"], "b-batch");
    assert_eq!(r["claimed"][0]["tasks"], json!(batch_ids));

    // 4. Single P2 proposal on `b-wait` still waits at 89s, then dispatches at >= 90s.
    t.advance(TimeDelta::seconds(44));
    let (s, r) = claim_as(&t, &tok, claim_verdict()).await;
    assert_eq!(s, 200, "{r}");
    assert!(r["claimed"].as_array().unwrap().is_empty(), "at 89s: {r}");

    t.advance(TimeDelta::seconds(1));
    let (s, r) = claim_as(&t, &tok, claim_verdict()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["claimed"][0]["task_id"], wait_prop);

    // 5. Triage debounce on `board-feedback`: single P2 waits < 90s, 3 P2 dispatch immediately,
    //    and single P2 dispatches once 90s elapses.
    let fb1 = t
        .ok(
            "POST",
            "/v1/feedback",
            Some(WORKER),
            Some(json!({ "about": "board", "text": "Slow command\ndetails" })),
        )
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/board-feedback",
            Some(json!({ "autopilot": true })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);

    let claim_triage = || {
        json!({
            "device": "runner-deb",
            "free_units": 8,
            "runners": ["acp"],
            "supported_kinds": ["triage"],
            "max_claims": 2
        })
    };
    let (s, r) = claim_as(&t, &tok, claim_triage()).await;
    assert_eq!(s, 200, "{r}");
    assert!(
        r["claimed"].as_array().unwrap().is_empty(),
        "single P2 feedback item must wait in debounce window: {r}"
    );

    // Adding 2 more distinct feedback items reaches batch size 3 -> dispatches immediately!
    let fb2 = t
        .ok(
            "POST",
            "/v1/feedback",
            Some(WORKER),
            Some(json!({ "about": "board", "text": "Broken cache\ndetails" })),
        )
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let fb3 = t
        .ok(
            "POST",
            "/v1/feedback",
            Some(WORKER),
            Some(json!({ "about": "board", "text": "Stale lease\ndetails" })),
        )
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(fb1, fb2);
    assert_ne!(fb2, fb3);
    let (s, r) = claim_as(&t, &tok, claim_triage()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["claimed"][0]["run"]["kind"], "triage");
    assert_eq!(r["claimed"][0]["tasks"], json!([fb1, fb2, fb3]));

    // 6. Setting `judge_batch_debounce_secs = 0` disables debounce for single P2 items.
    let t0 = T::with(|cfg| {
        cfg.defaults.judge_batch_debounce_secs = 0;
    });
    let tok0 = create_device(&t0, "runner-zero").await;
    t0.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t0, "no-deb", &[REPO], 2).await;
    let (s, _) = t0
        .as_user(
            "PATCH",
            "/v1/boards/no-deb",
            Some(json!({ "curate_every_secs": 0 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    let p_now = t0
        .task_with(
            WORKER,
            json!({ "board": "no-deb", "title": "Immediate prop", "body": "Proposal: now" }),
        )
        .await;
    let (s, r) = claim_as(
        &t0,
        &tok0,
        json!({
            "device": "runner-zero",
            "free_units": 4,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["verdict"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"][0]["task_id"], p_now);
}

#[tokio::test]
async fn goal_review_honors_start_time_and_per_board_interval() {
    let t = T::new();
    let tok = create_device(&t, "runner-goal").await;
    t.register(WORKER, Some(REPO), None).await;

    let claim_goal = || {
        json!({
            "device": "runner-goal",
            "free_units": 4,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["goal"]
        })
    };

    // 1. agent-board-658: TaskDone older than the last succeeded goal judge's `started_at`
    //    must NOT re-trigger a second goal run, even if newer than its `created_at`.
    t.ok(
        "POST",
        "/v1/boards",
        None,
        Some(json!({
            "slug": "goal-start",
            "title": "Goal Start",
            "goal": "Ship cleanly",
            "autopilot": true,
            "repos": [REPO]
        })),
    )
    .await;
    let t1 = t.task(WORKER, "goal-start", "Task 1").await;
    assert_eq!(t.claim(WORKER, &t1).await.0, 200);
    assert_eq!(t.done(WORKER, &t1, check(0)).await.0.as_u16(), 200);

    // Lease goal run at t+10s (`created_at`), complete another task at t+15s, start goal run
    // at t+20s (`started_at`), and succeed at t+30s (`ended_at`).
    t.advance(TimeDelta::seconds(10));
    let (s, r) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"][0]["board"], "goal-start");
    assert_eq!(r["claimed"][0]["task_id"], t1);
    assert_eq!(r["claimed"][0]["tasks"], json!([t1]));
    let g1 = r["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();

    t.advance(TimeDelta::seconds(5));
    let t2 = t
        .task(WORKER, "goal-start", "Task 2 before goal start")
        .await;
    assert_eq!(t.claim(WORKER, &t2).await.0, 200);
    assert_eq!(t.done(WORKER, &t2, check(0)).await.0.as_u16(), 200);

    t.advance(TimeDelta::seconds(5));
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{g1}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    t.advance(TimeDelta::seconds(10));
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{g1}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await
        .0,
        200
    );

    // No second goal review: t2 finished before g1 started!
    let (s, r) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{r}");
    assert!(
        r["claimed"].as_array().unwrap().is_empty(),
        "TaskDone before goal started_at must not re-trigger: {r}"
    );

    // A TaskDone AFTER g1 started DOES trigger a goal review on a board with no interval.
    t.advance(TimeDelta::seconds(5));
    let t3 = t.task(WORKER, "goal-start", "Task 3 after goal").await;
    assert_eq!(t.claim(WORKER, &t3).await.0, 200);
    assert_eq!(t.done(WORKER, &t3, check(0)).await.0.as_u16(), 200);
    let (s, r) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"][0]["board"], "goal-start");
    let g2 = r["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{g2}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{g2}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await
        .0,
        200
    );

    // 2. agent-board-604: Per-board `goal_review_every_secs` (6h = 21600s) throttles follow-up
    //    goal runs until 6h elapse after the last goal review completed.
    t.ok(
        "PATCH",
        "/v1/boards/goal-start",
        None,
        Some(json!({ "goal_review_every_secs": 21_600 })),
    )
    .await;
    t.advance(TimeDelta::minutes(30));
    let t4 = t
        .task(WORKER, "goal-start", "Task 4 inside 6h window")
        .await;
    assert_eq!(t.claim(WORKER, &t4).await.0, 200);
    assert_eq!(t.done(WORKER, &t4, check(0)).await.0.as_u16(), 200);
    let (s, r) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{r}");
    assert!(
        r["claimed"].as_array().unwrap().is_empty(),
        "goal review must be throttled inside 6h interval: {r}"
    );

    // Advance past 6h from g2 completion -> goal review is now eligible!
    t.advance(TimeDelta::minutes(330));
    let (s, r) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"][0]["board"], "goal-start");
    assert_eq!(r["claimed"][0]["run"]["kind"], "goal");
    let g3 = r["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{g3}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{g3}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await
        .0,
        200
    );

    // 3. `feedback_board` (`kind = inbox`) NEVER runs goal reviews, even if
    //    `goal_review_every_secs` is cleared to NULL, and setting `goal_review_every_secs = 0`
    //    on a project board also disables goal reviews.
    t.ok(
        "POST",
        "/v1/boards",
        None,
        Some(json!({
            "slug": "board-feedback",
            "title": "Feedback",
            "goal": "Keep friction low",
            "autopilot": true,
            "repos": [REPO]
        })),
    )
    .await;
    t.ok(
        "PATCH",
        "/v1/boards/board-feedback",
        None,
        Some(json!({ "goal_review_every_secs": null })),
    )
    .await;
    let f1 = t.task(WORKER, "board-feedback", "FB 1").await;
    assert_eq!(t.claim(WORKER, &f1).await.0, 200);
    assert_eq!(t.done(WORKER, &f1, check(0)).await.0.as_u16(), 200);
    let (s, r) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{r}");
    assert!(
        r["claimed"].as_array().unwrap().is_empty(),
        "inbox board-feedback must never run goal review: {r}"
    );
    // Disabling goal review on a project board via `goal_review_every_secs = 0` also skips Goal.
    t.ok(
        "PATCH",
        "/v1/boards/goal-start",
        None,
        Some(json!({ "goal_review_every_secs": 0 })),
    )
    .await;
    let d3 = t.task(WORKER, "goal-start", "Done 3").await;
    assert_eq!(t.claim(WORKER, &d3).await.0, 200);
    assert_eq!(t.done(WORKER, &d3, check(0)).await.0.as_u16(), 200);
    let (s, r) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{r}");
    assert!(
        r["claimed"].as_array().unwrap().is_empty(),
        "project board with goal_review_every_secs=0 must not run goal review: {r}"
    );
}

#[tokio::test]
async fn goal_review_names_exact_last_finished_task_on_board_with_over_25_done_tasks() {
    let t = T::new();
    let tok = create_device(&t, "runner-goal-many").await;
    t.register(WORKER, Some(REPO), None).await;
    t.ok(
        "POST",
        "/v1/boards",
        None,
        Some(json!({
            "slug": "many",
            "title": "Many Done",
            "goal": "Ship reliably",
            "autopilot": true,
            "repos": [REPO]
        })),
    )
    .await;

    // Create 26 tasks; finish tasks #2..=#26 first, then finish task #1 LAST so the last
    // finished task is NOT the highest id and would not be identifiable from priority/id order.
    let mut ids = Vec::new();
    for i in 1..=26 {
        ids.push(t.task(WORKER, "many", &format!("Task {i}")).await);
    }
    for id in ids.iter().skip(1) {
        t.advance(TimeDelta::seconds(5));
        assert_eq!(t.claim(WORKER, id).await.0, 200);
        assert_eq!(t.done(WORKER, id, check(0)).await.0.as_u16(), 200);
    }
    t.advance(TimeDelta::seconds(5));
    let last_finished = &ids[0];
    assert_eq!(t.claim(WORKER, last_finished).await.0, 200);
    assert_eq!(
        t.done(WORKER, last_finished, check(0)).await.0.as_u16(),
        200
    );

    // Claiming a Goal run names `last_finished` directly in `task_id`, `tasks`, and `run.task_id`.
    let claim_goal = || {
        json!({
            "device": "runner-goal-many",
            "free_units": 4,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["goal"]
        })
    };
    let (s, r) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"][0]["run"]["kind"], "goal");
    assert_eq!(r["claimed"][0]["task_id"], *last_finished);
    assert_eq!(r["claimed"][0]["run"]["task_id"], *last_finished);
    assert_eq!(r["claimed"][0]["tasks"], json!([last_finished]));
    let run1 = r["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run1}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run1}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await
        .0,
        200
    );

    // Finish a 27th task (past the 25-item MCP task_list default page) and verify the next
    // Goal claim names it directly.
    t.advance(TimeDelta::seconds(10));
    let t27 = t.task(WORKER, "many", "Task 27").await;
    assert_eq!(t.claim(WORKER, &t27).await.0, 200);
    assert_eq!(t.done(WORKER, &t27, check(0)).await.0.as_u16(), 200);

    // Confirm a default 25-item done list omits t27 (it starts at many-1..many-25).
    let listed = t
        .ok(
            "GET",
            "/v1/tasks?board=many&status=done&limit=25",
            None,
            None,
        )
        .await;
    let listed_ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed_ids.len(), 25);
    assert!(!listed_ids.contains(&t27.as_str()));

    let (s, r2) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{r2}");
    assert_eq!(r2["claimed"][0]["run"]["kind"], "goal");
    assert_eq!(r2["claimed"][0]["task_id"], t27);
    assert_eq!(r2["claimed"][0]["tasks"], json!([t27]));
}

#[tokio::test]
async fn strict_economic_bands_priority_rank_wsjf_and_age_cap() {
    let t = T::new();
    let tok = create_device(&t, "mac-econ").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "econ", &[REPO], 4).await;

    // 1. Old P3 task (30 days old!) vs fresh P1 and P2 tasks: strict non-overlapping priority
    //    bands MUST pick P1 > P2 > P3, and age MUST stay <= 8.0!
    let old_p3 = t
        .task_with(
            WORKER,
            json!({ "board": "econ", "title": "Ancient P3", "priority": "p3", "size": "xs" }),
        )
        .await;
    t.advance(TimeDelta::days(30));
    let fresh_p2 = t
        .task_with(
            WORKER,
            json!({ "board": "econ", "title": "Fresh P2", "priority": "p2", "size": "l" }),
        )
        .await;
    let p3_blocker = t
        .task_with(
            WORKER,
            json!({ "board": "econ", "title": "P3 blocker of P1", "priority": "p3", "size": "l" }),
        )
        .await;
    let _p1_dep = t
        .task_with(
            WORKER,
            json!({
                "board": "econ",
                "title": "Blocked P1 dependent",
                "priority": "p1",
                "blockers": [p3_blocker]
            }),
        )
        .await;

    let (s, r) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-econ",
            "free_units": 12,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["worker"],
            "max_claims": 3
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    let claimed = r["claimed"].as_array().unwrap();
    let ids: Vec<&str> = claimed
        .iter()
        .map(|c| c["task_id"].as_str().unwrap())
        .collect();
    // P3 blocker inherits effective_priority=P1 -> wins 1st; fresh P2 -> 2nd; 30d old P3 -> 3rd!
    assert_eq!(
        ids,
        [p3_blocker.as_str(), fresh_p2.as_str(), old_p3.as_str()]
    );
    assert!(
        claimed[2]["score_breakdown"]["age"].as_f64().unwrap() <= 8.0,
        "age must be strictly capped at <= 8.0: {}",
        claimed[2]["score_breakdown"]
    );

    // 2. Explicit Frontier rank (`rank=1 > rank=2 > unranked`) beats 30d age and WSJF, and
    //    among unranked same-priority tasks `xs > s > m > l`. Use a second repo so `econ`'s
    //    3 active `repo:<REPO>:write` locks don't cap `wsjf`'s 4 concurrent claims.
    const REPO2: &str = "github.com/example/repo-wsjf";
    autopilot_board(&t, "wsjf", &[REPO2], 4).await;
    let old_unranked_xs = t
        .task_with(
            WORKER,
            json!({ "board": "wsjf", "title": "Old unranked XS", "priority": "p2", "size": "xs" }),
        )
        .await;
    t.advance(TimeDelta::days(30));
    let ranked_2 = t
        .task_with(
            WORKER,
            json!({ "board": "wsjf", "title": "Rank 2", "priority": "p2", "rank": 2, "size": "l" }),
        )
        .await;
    let ranked_1 = t
        .task_with(
            WORKER,
            json!({ "board": "wsjf", "title": "Rank 1", "priority": "p2", "rank": 1, "size": "l" }),
        )
        .await;
    let unranked_l = t
        .task_with(
            WORKER,
            json!({ "board": "wsjf", "title": "Unranked L", "priority": "p2", "size": "l" }),
        )
        .await;

    let (s, r2) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-econ",
            "free_units": 16,
            "runners": ["acp"],
            "repos": [REPO, REPO2],
            "supported_kinds": ["worker"],
            "max_claims": 4
        }),
    )
    .await;
    assert_eq!(s, 200, "{r2}");
    let ids2: Vec<&str> = r2["claimed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["task_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids2,
        [
            ranked_1.as_str(),
            ranked_2.as_str(),
            old_unranked_xs.as_str(),
            unranked_l.as_str()
        ]
    );
}

#[tokio::test]
async fn intake_ordering_sorts_verdict_and_triage_by_priority_and_reports_before_slicing() {
    let t = T::new();
    let tok = create_device(&t, "mac-intake").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    autopilot_board(&t, "intake", &[REPO], 2).await;
    // Disable daily `curate` on `intake` so all proposals enter immediate Verdict batching
    // and we can verify priority/report sorting before the 5-item slice.
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/intake",
            Some(json!({ "curate_every_secs": 0 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);

    // Seed 6 older P3 proposals, then 1 newer P1 proposal -> P1 MUST be inside the first 5!
    for i in 0..6 {
        t.task_with(
            WORKER,
            json!({
                "board": "intake",
                "title": format!("Old P3 proposal {i}"),
                "body": "Proposal: minor tweak",
                "priority": "p3"
            }),
        )
        .await;
    }
    t.advance(TimeDelta::minutes(5));
    let urgent_prop = t
        .task_with(
            WORKER,
            json!({
                "board": "intake",
                "title": "Urgent P1 proposal",
                "body": "Proposal: fix outage",
                "priority": "p1"
            }),
        )
        .await;

    let (s, r) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-intake",
            "free_units": 4,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["verdict"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    let tasks: Vec<&str> = r["claimed"][0]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(tasks.len(), 5);
    assert_eq!(
        tasks[0], urgent_prop,
        "P1 proposal must lead verdict batch: {tasks:?}"
    );

    // Cooling an open proposal excludes it from Verdict dispatch!
    autopilot_board(&t, "coldprop", &[REPO], 2).await;
    let cold_p = t
        .task_with(
            WORKER,
            json!({ "board": "coldprop", "title": "Cold proposal", "body": "Proposal: later" }),
        )
        .await;
    assert_eq!(
        t.patch(WORKER, &cold_p, json!({ "cooled": true })).await.0,
        200
    );
    let (s, rc) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-intake",
            "free_units": 4,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["verdict"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{rc}");
    assert!(
        rc["claimed"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["board"] != "coldprop"),
        "cooled proposal must not be dispatched for Verdict: {rc}"
    );

    // Seed 6 older single-report feedback items, then 1 newer multi-report feedback item ->
    // high-report item MUST lead the first 5-item triage batch!
    t.ok(
        "POST",
        "/v1/boards",
        None,
        Some(json!({
            "slug": "board-feedback",
            "title": "Feedback",
            "goal": "Triage",
            "autopilot": true,
            "repos": [REPO]
        })),
    )
    .await;
    for i in 0..6 {
        t.ok(
            "POST",
            "/v1/feedback",
            Some(WORKER),
            Some(json!({
                "about": format!("tool-{i}"),
                "text": format!("Minor friction {i}")
            })),
        )
        .await;
    }
    t.advance(TimeDelta::minutes(5));
    let hot = t
        .ok(
            "POST",
            "/v1/feedback",
            Some(WORKER),
            Some(json!({ "about": "board", "text": "Frequent crash on startup" })),
        )
        .await;
    let hot_id = hot["task"]["id"].as_str().unwrap().to_owned();
    // Add 2 more reports on the same signature so report_count = 3.
    t.ok(
        "POST",
        "/v1/feedback",
        Some(REVIEWER),
        Some(json!({ "about": "board", "text": "Frequent crash on startup" })),
    )
    .await;
    t.ok(
        "POST",
        "/v1/feedback",
        Some(WORKER),
        Some(json!({ "about": "board", "text": "Frequent crash on startup" })),
    )
    .await;

    let (s, rt) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-intake",
            "free_units": 4,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["triage"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{rt}");
    let t_ids: Vec<&str> = rt["claimed"][0]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(t_ids.len(), 5);
    assert_eq!(
        t_ids[0], hot_id,
        "multi-report item must lead triage batch: {t_ids:?}"
    );
}

#[tokio::test]
async fn review_readiness_gate_waits_for_owner_idle_and_pr_or_fallback_grace() {
    let t = T::new();
    let tok = create_device(&t, "mac-revgate").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "rg", &[REPO], 2).await;

    let task_id = t.task(WORKER, "rg", "Needs PR before review").await;
    assert_eq!(t.claim(WORKER, &task_id).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &task_id, json!({ "status": "review" }))
            .await
            .0,
        200
    );

    let claim_rev = || {
        json!({
            "device": "mac-revgate",
            "free_units": 4,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["review"]
        })
    };

    // 1. Owner is still `running` and no PR note -> held (empty claim).
    let (s, r) = claim_as(&t, &tok, claim_rev()).await;
    assert_eq!(s, 200, "{r}");
    assert!(r["claimed"].as_array().unwrap().is_empty(), "{r}");

    // 2. Owner goes `idle`, still NO PR note and < 15m elapsed -> still held!
    t.ok(
        "POST",
        &format!("/v1/agents/{WORKER}/state"),
        Some(WORKER),
        Some(json!({ "state": "idle" })),
    )
    .await;
    let (s, r) = claim_as(&t, &tok, claim_rev()).await;
    assert_eq!(s, 200, "{r}");
    assert!(r["claimed"].as_array().unwrap().is_empty(), "{r}");

    // 3. After 15m fallback grace with idle owner -> Review IS admitted even without PR note!
    t.advance(TimeDelta::minutes(15));
    let (s, r) = claim_as(&t, &tok, claim_rev()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"][0]["task_id"], task_id);

    // 4. Separate task: owner running + PR URL note -> STILL held while owner runs,
    //    then immediately admitted once owner goes idle!
    t.ok(
        "POST",
        &format!("/v1/agents/{WORKER}/state"),
        Some(WORKER),
        Some(json!({ "state": "running" })),
    )
    .await;
    let t2 = t.task(WORKER, "rg", "Has PR while running").await;
    assert_eq!(t.claim(WORKER, &t2).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &t2, json!({ "status": "review" })).await.0,
        200
    );
    t.ok(
        "POST",
        &format!("/v1/tasks/{t2}/notes"),
        Some(WORKER),
        Some(json!({
            "agent": WORKER,
            "kind": "note",
            "text": "Opened [PR #42](https://github.com/example/repo/pull/42)"
        })),
    )
    .await;
    // Note call marks WORKER running via touch_agent -> held while running.
    let (s, r) = claim_as(&t, &tok, claim_rev()).await;
    assert_eq!(s, 200, "{r}");
    assert!(r["claimed"].as_array().unwrap().is_empty(), "{r}");

    // Worker goes idle -> immediately ready for Review without waiting 15m!
    t.ok(
        "POST",
        &format!("/v1/agents/{WORKER}/state"),
        Some(WORKER),
        Some(json!({ "state": "idle" })),
    )
    .await;
    let (s, r) = claim_as(&t, &tok, claim_rev()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"][0]["task_id"], t2);
}

#[tokio::test]
async fn leased_file_overlap_holds_worker_until_lease_releases() {
    let t = T::new();
    let tok = create_device(&t, "mac-overlap").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "ov", &[REPO], 4).await;

    // Active `doing` task leasing `crates/board-server/src/ops/dispatch.rs`.
    let active = t.task(WORKER, "ov", "Active refactor").await;
    assert_eq!(t.claim(WORKER, &active).await.0, 200);
    t.ok(
        "POST",
        "/v1/leases",
        Some(WORKER),
        Some(json!({
            "agent": WORKER,
            "task": active,
            "repo": REPO,
            "globs": ["crates/board-server/src/ops/dispatch.rs"]
        })),
    )
    .await;

    // Overlapping P1 task mentioning the leased file with `./` prefix vs disjoint P2 task.
    let overlapping = t
        .task_with(
            WORKER,
            json!({
                "board": "ov",
                "title": "Touch dispatch.rs too",
                "body": "Edit `./crates/board-server/src/ops/dispatch.rs` scoring",
                "priority": "p1"
            }),
        )
        .await;
    let disjoint = t
        .task_with(
            WORKER,
            json!({
                "board": "ov",
                "title": "Touch render.rs",
                "body": "Edit `crates/board/src/render.rs` formatting",
                "priority": "p2"
            }),
        )
        .await;

    // Overlapping P1 is held with `lease_overlap`, so disjoint P2 is dispatched instead!
    let (s, r) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-overlap",
            "free_units": 8,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["worker"],
            "max_claims": 2
        }),
    )
    .await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["claimed"][0]["task_id"], disjoint);
    assert_eq!(r["holds_summary"][0]["reason"], "lease_overlap");

    // Once `active` finishes (releasing its leases), `overlapping` is immediately claimed!
    assert_eq!(t.done(WORKER, &active, check(0)).await.0.as_u16(), 200);
    let (s, r2) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-overlap",
            "free_units": 8,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["worker"]
        }),
    )
    .await;
    assert_eq!(s, 200, "{r2}");
    assert_eq!(r2["claimed"][0]["task_id"], overlapping);
}

#[tokio::test]
async fn merge_exponential_backoff_and_skip_run_outcomes_on_deferred_noop() {
    let t = T::new();
    let tok = create_device(&t, "mac-mbo").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    autopilot_board(&t, "mbo", &[REPO], 2).await;

    let task_id = make_reviewed_task(
        &t,
        "mbo",
        "Pending CI merge",
        Some("review: pass. LGTM.\npr: https://github.com/example/repo/pull/9"),
    )
    .await;

    let claim_merge = || {
        json!({
            "device": "mac-mbo",
            "free_units": 4,
            "runners": ["deterministic"],
            "repos": [REPO],
            "supported_kinds": ["merge"]
        })
    };

    // 1st generic deferred -> 60s cooldown, ZERO run_outcomes rows written!
    let (s, r1) = claim_as(&t, &tok, claim_merge()).await;
    assert_eq!(s, 200, "{r1}");
    let m1 = r1["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{m1}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{m1}"),
            None,
            Some(json!({ "state": "succeeded", "outcome": { "status": "deferred", "reason": "checks still pending" } })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM run_outcomes WHERE run_id = '{m1}'"
        )),
        0,
        "deferred merge must not write a run_outcomes row"
    );

    t.advance(TimeDelta::seconds(59));
    assert!(
        claim_as(&t, &tok, claim_merge()).await.1["claimed"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    t.advance(TimeDelta::seconds(1));

    // 2nd consecutive generic deferred -> 180s cooldown!
    let (s, r2) = claim_as(&t, &tok, claim_merge()).await;
    assert_eq!(s, 200, "{r2}");
    let m2 = r2["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{m2}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{m2}"),
            None,
            Some(json!({ "state": "succeeded", "outcome": { "status": "noop", "reason": "waiting on merge queue" } })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM run_outcomes WHERE run_id = '{m2}'"
        )),
        0,
        "noop merge must not write a run_outcomes row"
    );

    t.advance(TimeDelta::seconds(179));
    assert!(
        claim_as(&t, &tok, claim_merge()).await.1["claimed"]
            .as_array()
            .unwrap()
            .is_empty(),
        "2nd generic deferred must hold for 180s"
    );
    t.advance(TimeDelta::seconds(1));

    // 3rd consecutive generic deferred -> 300s cooldown!
    let (s, r3) = claim_as(&t, &tok, claim_merge()).await;
    assert_eq!(s, 200, "{r3}");
    let m3 = r3["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{m3}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{m3}"),
            None,
            Some(json!({ "state": "succeeded", "outcome": { "status": "deferred", "reason": "checks still pending" } })),
        )
        .await
        .0,
        200
    );
    t.advance(TimeDelta::seconds(299));
    assert!(
        claim_as(&t, &tok, claim_merge()).await.1["claimed"]
            .as_array()
            .unwrap()
            .is_empty(),
        "3rd generic deferred must hold for 300s"
    );
    t.advance(TimeDelta::seconds(1));
    let (s, r4) = claim_as(&t, &tok, claim_merge()).await;
    assert_eq!(s, 200, "{r4}");
    assert_eq!(r4["claimed"][0]["task_id"], task_id);
}

#[tokio::test]
async fn lazy_proposal_admission_quarantines_p2_p3_and_fast_lanes_p1_user_and_high_report_proposals()
 {
    let t = T::new();
    let tok = create_device(&t, "mac-lazy").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    autopilot_board(&t, "lazy", &[REPO], 2).await;

    let claim_verdict = || {
        json!({
            "device": "mac-lazy",
            "free_units": 8,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["verdict"],
            "max_claims": 4
        })
    };

    // 1. Normal P2 and P3 agent proposals wait quietly in Proposals at 0 LLM cost (no Verdict).
    let _p2_agent = t
        .task_with(
            WORKER,
            json!({
                "board": "lazy",
                "title": "Routine agent P2 proposal",
                "body": "Proposal: nice-to-have cleanup",
                "priority": "p2"
            }),
        )
        .await;
    let _p3_agent = t
        .task_with(
            WORKER,
            json!({
                "board": "lazy",
                "title": "Routine agent P3 proposal",
                "body": "Proposal: minor refactor",
                "priority": "p3"
            }),
        )
        .await;
    t.advance(TimeDelta::minutes(5));
    let (s, r) = claim_as(&t, &tok, claim_verdict()).await;
    assert_eq!(s, 200, "{r}");
    assert!(
        r["claimed"].as_array().unwrap().is_empty(),
        "routine P2/P3 agent proposals must wait for Curate, not wake Verdict: {r}"
    );

    // 2. Fast-lane P1 proposal dispatches Verdict immediately!
    let p1_prop = t
        .task_with(
            WORKER,
            json!({
                "board": "lazy",
                "title": "Urgent P1 proposal",
                "body": "Proposal: fix outage",
                "priority": "p1"
            }),
        )
        .await;
    let (s, r) = claim_as(&t, &tok, claim_verdict()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["claimed"][0]["tasks"], json!([p1_prop]));
    // Finish the P1 verdict run so the verdict lock clears.
    let v_id = r["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    for state in ["running", "succeeded"] {
        assert_eq!(
            send_as(
                &t,
                &tok,
                "PATCH",
                &format!("/v1/runs/{v_id}"),
                None,
                Some(json!({ "state": state })),
            )
            .await
            .0,
            200
        );
    }
    assert_eq!(
        t.patch(REVIEWER, &p1_prop, json!({ "auto": true })).await.0,
        200
    );

    // 3. Fast-lane `origin = user` proposal dispatches Verdict (past debounce window)!
    let user_prop = t
        .task_with(
            WORKER,
            json!({
                "board": "lazy",
                "title": "User-origin proposal",
                "body": "Proposal: operator idea",
                "priority": "p2",
                "origin": "user"
            }),
        )
        .await;
    t.advance(TimeDelta::seconds(95));
    let (s, r) = claim_as(&t, &tok, claim_verdict()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["claimed"][0]["tasks"], json!([user_prop]));
    let v2 = r["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    for state in ["running", "succeeded"] {
        assert_eq!(
            send_as(
                &t,
                &tok,
                "PATCH",
                &format!("/v1/runs/{v2}"),
                None,
                Some(json!({ "state": state })),
            )
            .await
            .0,
            200
        );
    }
    assert_eq!(
        t.patch(REVIEWER, &user_prop, json!({ "auto": true }))
            .await
            .0,
        200
    );

    // 4. Fast-lane `report_count >= 3` proposal dispatches Verdict immediately!
    let rep_prop = t
        .task_with(
            WORKER,
            json!({
                "board": "lazy",
                "title": "High-report proposal",
                "body": "Proposal: repeated pain point",
                "priority": "p2"
            }),
        )
        .await;
    let fb_id = t
        .ok(
            "POST",
            "/v1/feedback",
            Some(WORKER),
            Some(json!({ "about": "board", "signature": "sig-high-rep", "text": "Repeated failure\ndetails" })),
        )
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for _ in 0..2 {
        t.ok(
            "POST",
            "/v1/feedback",
            Some(REVIEWER),
            Some(json!({ "about": "board", "signature": "sig-high-rep", "text": "Repeated failure\nagain" })),
        )
        .await;
    }
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/feedback/{fb_id}/resolve"),
            Some(json!({ "kind": "routed", "task": rep_prop })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    t.advance(TimeDelta::seconds(95));
    let (s, r) = claim_as(&t, &tok, claim_verdict()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["claimed"][0]["tasks"], json!([rep_prop]));
}

#[tokio::test]
async fn curate_run_kind_enumerates_on_cadence_and_frontier_refill_and_grants_coordinator_rights() {
    let t = T::new();
    let tok = create_device(&t, "mac-curate").await;
    let curate_agent = "antigravity:curate-1";
    t.register(WORKER, Some(REPO), None).await;
    t.register(curate_agent, Some(REPO), None).await;
    autopilot_board(&t, "proj", &[REPO], 2).await;

    let claim_curate = || {
        json!({
            "device": "mac-curate",
            "free_units": 8,
            "runners": ["acp"],
            "repos": [REPO],
            "supported_kinds": ["curate"],
            "max_claims": 2
        })
    };

    // Empty board -> no Curate.
    let (s, r) = claim_as(&t, &tok, claim_curate()).await;
    assert_eq!(s, 200, "{r}");
    assert!(r["claimed"].as_array().unwrap().is_empty(), "{r}");

    // Add a lazy P2 proposal -> Curate enumerates with cost_units = 2 and lock `curate:proj`.
    let prop = t
        .task_with(
            WORKER,
            json!({
                "board": "proj",
                "title": "Lazy proposal for curate",
                "body": "Proposal: optimize query",
                "priority": "p2"
            }),
        )
        .await;
    let (s, r) = claim_as(&t, &tok, claim_curate()).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["claimed"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["claimed"][0]["run"]["kind"], "curate");
    assert_eq!(r["claimed"][0]["run"]["cost_units"], 2);
    let run_id = r["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();

    // Only 1 Curate at a time per board.
    let (s, r_dup) = claim_as(&t, &tok, claim_curate()).await;
    assert_eq!(s, 200, "{r_dup}");
    assert!(r_dup["claimed"].as_array().unwrap().is_empty(), "{r_dup}");

    // Transition Curate run to running with `curate_agent`: verify LIVE_RUN_RIGHTS grants
    // coordinator rights to accept proposals (`auto = true`), set size/priority/rank, and
    // drop obsolete tasks without holding a board coordinator lease!
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run_id}"),
            None,
            Some(json!({ "state": "running", "agent_id": curate_agent })),
        )
        .await
        .0,
        200
    );
    let (s, accepted) = t
        .patch(
            curate_agent,
            &prop,
            json!({ "auto": true, "size": "s", "priority": "p2" }),
        )
        .await;
    assert_eq!(
        s, 200,
        "curate judge must have rights to accept proposal: {accepted}"
    );
    assert_eq!(accepted["auto"], true);
    assert_eq!(accepted["size"], "s");

    // Finish Curate run -> with no new task changes, even past 1h cooldown Curate must NOT
    // loop hourly (it waits for daily cadence or a real board task change).
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run_id}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await
        .0,
        200
    );
    t.advance(TimeDelta::minutes(65));
    let (s, r_no_loop) = claim_as(&t, &tok, claim_curate()).await;
    assert_eq!(s, 200, "{r_no_loop}");
    assert!(
        r_no_loop["claimed"].as_array().unwrap().is_empty(),
        "unchanged board must NOT trigger hourly Curate loops: {r_no_loop}"
    );

    // Once a new unranked ready task arrives (changing board tasks past the 1h cooldown),
    // frontier refill triggers cleanly!
    let _extra = t.task(WORKER, "proj", "Second unranked ready task").await;
    let (s, r_refill) = claim_as(&t, &tok, claim_curate()).await;
    assert_eq!(s, 200, "{r_refill}");
    assert_eq!(
        r_refill["claimed"].as_array().unwrap().len(),
        1,
        "{r_refill}"
    );
    assert_eq!(r_refill["claimed"][0]["run"]["kind"], "curate");
    let run2 = r_refill["claimed"][0]["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for state in ["running", "succeeded"] {
        assert_eq!(
            send_as(
                &t,
                &tok,
                "PATCH",
                &format!("/v1/runs/{run2}"),
                None,
                Some(json!({ "state": state })),
            )
            .await
            .0,
            200
        );
    }

    // Within 1h cooldown even after a new task arrives, Curate waits out the cooldown.
    let _extra2 = t.task(WORKER, "proj", "Third unranked ready task").await;
    t.advance(TimeDelta::minutes(30));
    let (s, r_cool) = claim_as(&t, &tok, claim_curate()).await;
    assert_eq!(s, 200, "{r_cool}");
    assert!(
        r_cool["claimed"].as_array().unwrap().is_empty(),
        "curate must respect 1h frontier-refill cooldown: {r_cool}"
    );

    // Lazy p2/p3 proposals waiting for Curate must NOT block `RunKind::Goal` on an otherwise
    // idle board.
    assert_eq!(
        t.as_user(
            "PATCH",
            "/v1/boards/proj",
            Some(json!({ "goal": "Ship epic 3 cleanly" }))
        )
        .await
        .0
        .as_u16(),
        200
    );
    let _lazy_prop = t
        .task_with(
            WORKER,
            json!({
                "board": "proj",
                "title": "Waiting p3 proposal",
                "body": "Proposal: low priority cleanup",
                "priority": "p3"
            }),
        )
        .await;
    // Complete all ready auto tasks on `proj` so the board is idle except for `_lazy_prop`.
    for tid in [&prop, &_extra, &_extra2] {
        let (s, body) = t
            .patch(
                WORKER,
                tid,
                json!({ "status": "dropped", "reason": "test cleanup" }),
            )
            .await;
        assert_eq!(s, 200, "{body}");
    }
    let done_task = t.task(WORKER, "proj", "Finished goal trigger").await;
    t.ok(
        "POST",
        &format!("/v1/tasks/{done_task}/claim"),
        Some(WORKER),
        Some(json!({ "agent": WORKER })),
    )
    .await;
    let ver = t.get_task(&done_task).await["version"].as_i64().unwrap();
    t.ok(
        "POST",
        &format!("/v1/tasks/{done_task}/done"),
        Some(WORKER),
        Some(json!({
            "agent": WORKER,
            "version": ver,
            "evidence": [{ "kind": "artifact", "value": "shipped" }]
        })),
    )
    .await;
    let (s, r_goal) = claim_as(
        &t,
        &tok,
        json!({
            "device": "mac-curate",
            "free_units": 8,
            "runners": ["acp", "t3"],
            "repos": [REPO],
            "supported_kinds": ["goal"],
            "max_claims": 1
        }),
    )
    .await;
    assert_eq!(s, 200, "{r_goal}");
    assert_eq!(
        r_goal["claimed"].as_array().unwrap().len(),
        1,
        "lazy Curate-deferred proposals must not block Goal reviews: {r_goal}"
    );
    assert_eq!(r_goal["claimed"][0]["run"]["kind"], "goal");

    // Disabling curate (`curate_every_secs = 0`) stops Curate enumeration completely.
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/proj",
            Some(json!({ "curate_every_secs": 0 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    t.advance(TimeDelta::hours(25));
    let (s, r_off) = claim_as(&t, &tok, claim_curate()).await;
    assert_eq!(s, 200, "{r_off}");
    assert!(r_off["claimed"].as_array().unwrap().is_empty(), "{r_off}");
}

#[tokio::test]
async fn after_merge_standard_upgrade_sh_routes_to_deterministic_merge_and_falls_back_to_llm_review()
 {
    let t = T::new();
    let tok = create_device(&t, "mac-deploy").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    autopilot_board(&t, "deploy-board", &[REPO], 2).await;

    let task_id = make_reviewed_task(
        &t,
        "deploy-board",
        "Auto-deployable change",
        Some(
            "review: pass. Clean diff.\ncheck: cargo test => 0\nafter merge: deploy/upgrade.sh --yes\npr: https://github.com/example/repo/pull/400",
        ),
    )
    .await;

    let dispatch_agent = "dispatch:mac-deploy";
    let (s, _) = send_as(
        &t,
        &tok,
        "POST",
        "/v1/agents",
        Some(dispatch_agent),
        Some(json!({
            "id": dispatch_agent,
            "device": "mac-deploy",
            "runtime": "claude",
            "role": "coordinator",
        })),
    )
    .await;
    assert_eq!(s, 200);

    let claim_body = json!({
        "device": "mac-deploy",
        "free_units": 4,
        "runners": ["acp", "deterministic"],
        "capabilities": ["git:cloneable"],
        "repos": [REPO],
        "supported_kinds": ["review", "merge"],
    });

    // 1. Merge PR and record `AFTER_MERGE_READY_NOTE`.
    let (s, r1) = claim_as(&t, &tok, claim_body.clone()).await;
    assert_eq!(s, 200, "{r1}");
    assert_eq!(r1["claimed"][0]["run"]["kind"], "merge");
    let m1 = r1["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{m1}"),
            None,
            Some(json!({ "state": "running", "agent_id": dispatch_agent })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "POST",
            &format!("/v1/tasks/{task_id}/notes"),
            Some(dispatch_agent),
            Some(json!({
                "agent": dispatch_agent,
                "kind": "note",
                "text": "note: merged; waiting for after-merge checks (pass #1):\npr: https://github.com/example/repo/pull/400\ncommit: 0123456789abcdef0123456789abcdef01234567\nbase: main\nleft: deploy/upgrade.sh --yes",
            })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{m1}"),
            None,
            Some(json!({
                "state": "succeeded",
                "outcome": { "status": "awaiting_after_merge" },
            })),
        )
        .await
        .0,
        200
    );

    // 2. Next claim routes to ZERO-LLM deterministic `RunKind::Merge` (`after-merge-deploy ...`),
    //    NOT an LLM `RunKind::Review`!
    let (s, r2) = claim_as(&t, &tok, claim_body.clone()).await;
    assert_eq!(s, 200, "{r2}");
    assert_eq!(r2["claimed"].as_array().unwrap().len(), 1, "{r2}");
    assert_eq!(r2["claimed"][0]["run"]["kind"], "merge", "{r2}");
    assert_eq!(r2["claimed"][0]["run"]["runner"], "deterministic", "{r2}");
    assert!(
        r2["claimed"][0]["run"]["title"]
            .as_str()
            .unwrap()
            .starts_with("after-merge-deploy "),
        "{r2}"
    );
    let m2 = r2["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();

    // 3. If deterministic deploy fails and writes the Trail Rule fallback finding note, the
    //    very next claim falls back cleanly to the LLM `RunKind::Review` (`after-merge ...`)!
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{m2}"),
            None,
            Some(json!({ "state": "running", "agent_id": dispatch_agent })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "POST",
            &format!("/v1/tasks/{task_id}/notes"),
            Some(dispatch_agent),
            Some(json!({
                "agent": dispatch_agent,
                "kind": "finding",
                "text": "finding: deterministic after-merge deploy failed (exit 1). Falling back to LLM after-merge review judge.",
            })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{m2}"),
            None,
            Some(json!({
                "state": "succeeded",
                "outcome": { "status": "fallback_after_merge", "reason": "exit 1" },
            })),
        )
        .await
        .0,
        200
    );

    let (s, r3) = claim_as(&t, &tok, claim_body).await;
    assert_eq!(s, 200, "{r3}");
    assert_eq!(r3["claimed"].as_array().unwrap().len(), 1, "{r3}");
    assert_eq!(r3["claimed"][0]["run"]["kind"], "review", "{r3}");
    assert!(
        r3["claimed"][0]["run"]["title"]
            .as_str()
            .unwrap()
            .starts_with("after-merge "),
        "{r3}"
    );
}

#[tokio::test]
async fn waiting_external_worker_does_not_hold_repo_write_lock_or_starve_dispatch() {
    let t = T::new();
    let tok = create_device(&t, "runner-a").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "dotfiles", &[REPO], 2).await;

    let t1 = t.task(WORKER, "dotfiles", "Blocked on ask").await;
    let t2 = t.task(WORKER, "dotfiles", "Active worker 2").await;
    let t3 = t.task(WORKER, "dotfiles", "Ready worker 3").await;
    let t4 = t.task(WORKER, "dotfiles", "Overflow worker 4").await;

    let write_key = format!("repo:{REPO}:write");

    // 1. Dispatch claims t1 (worker 1) and t2 (worker 2), filling 2/2 write lock slots.
    let (s, c1) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-a",
            "free_units": 12,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["worker"],
            "max_claims": 2
        }),
    )
    .await;
    assert_eq!(s, 200, "{c1}");
    let claimed = c1["claimed"].as_array().unwrap();
    assert_eq!(claimed.len(), 2, "{c1}");
    let run1_id = claimed.iter().find(|c| c["task_id"] == t1).unwrap()["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let run2_id = claimed.iter().find(|c| c["task_id"] == t2).unwrap()["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM conflict_locks WHERE key = '{write_key}'"
        )),
        2
    );

    // 2. Worker 1 starts running, claims & blocks t1 on an ask, and enters `waiting_external`.
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run1_id}"),
            None,
            Some(json!({ "state": "running", "agent_id": WORKER })),
        )
        .await
        .0,
        200
    );
    assert_eq!(t.claim(WORKER, &t1).await.0, 200);
    t.ok(
        "POST",
        "/v1/asks",
        Some(WORKER),
        Some(json!({
            "agent": WORKER,
            "board": "dotfiles",
            "task": t1,
            "kind": "decision",
            "question": "Proceed?",
            "options": ["Yes", "No"]
        })),
    )
    .await;
    assert_eq!(
        t.patch(
            WORKER,
            &t1,
            json!({ "status": "blocked", "blocked_reason": "waiting on ask" })
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run1_id}"),
            None,
            Some(json!({ "state": "waiting_external", "lease_ttl_secs": 3600 })),
        )
        .await
        .0,
        200
    );

    // Worker 1's write lock must be released immediately so only run2 holds 1/2.
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM conflict_locks WHERE key = '{write_key}'"
        )),
        1
    );

    // 3. Dispatch must claim t3 cleanly — NOT hold it with `ConflictKey`!
    let (s, c2) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-a",
            "free_units": 12,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["worker"],
            "max_claims": 2
        }),
    )
    .await;
    assert_eq!(s, 200, "{c2}");
    let claimed2 = c2["claimed"].as_array().unwrap();
    assert_eq!(
        claimed2.len(),
        1,
        "t3 must claim the freed worker slot: {c2}"
    );
    assert_eq!(claimed2[0]["task_id"], t3);
    assert!(
        !c2.get("holds_summary")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|h| h["reason"] == "conflict_key"),
        "c2 must have no conflict_key holds: {c2}"
    );
    let run3_id = claimed2[0]["run"]["id"].as_str().unwrap().to_owned();

    // A follow-up claim while run2 + run3 occupy 2/2 slots holds t4 on `parallel_tasks`,
    // never `conflict_key`.
    let (s, c_full) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-a",
            "free_units": 12,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["worker"],
            "max_claims": 1
        }),
    )
    .await;
    assert_eq!(s, 200, "{c_full}");
    assert!(c_full["claimed"].as_array().unwrap().is_empty(), "{c_full}");
    let full_holds = c_full["holds_summary"].as_array().unwrap();
    assert!(
        full_holds.iter().any(|h| h["reason"] == "parallel_tasks"),
        "t4 should be held by parallel_tasks: {c_full}"
    );
    assert!(
        !full_holds.iter().any(|h| h["reason"] == "conflict_key"),
        "t4 must never be held by conflict_key: {c_full}"
    );
    assert_eq!(
        t.count("SELECT COUNT(*) FROM scheduler_holds WHERE reason = 'conflict_key'"),
        0
    );

    // 4. Simulate a legacy pre-upgrade `waiting_external` lock row still sitting in SQLite:
    //    complete run3 and park t3 in `review` so a worker slot is free, inject a stale lock
    //    for run1, and verify `POST /v1/dispatch/claim` reconciles it automatically and claims t4!
    assert_eq!(t.claim(WORKER, &t3).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &t3, json!({ "status": "review" })).await.0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run3_id}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run3_id}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await
        .0,
        200
    );
    let now_ms = t.state.now().timestamp_millis();
    t.exec(&format!(
        "INSERT INTO conflict_locks (key, run_id, max_holders, acquired_at) \
         VALUES ('{write_key}', '{run1_id}', 2, {now_ms})"
    ));
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM conflict_locks WHERE key = '{write_key}'"
        )),
        2
    );

    let (s, c3) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-a",
            "free_units": 12,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["worker"],
            "max_claims": 1
        }),
    )
    .await;
    assert_eq!(s, 200, "{c3}");
    assert_eq!(
        c3["claimed"].as_array().unwrap().len(),
        1,
        "dispatch must reconcile legacy waiting_external write locks and claim t4: {c3}"
    );
    assert_eq!(c3["claimed"][0]["task_id"], t4);
    assert_eq!(
        t.count("SELECT COUNT(*) FROM scheduler_holds WHERE reason = 'conflict_key'"),
        0
    );
    let _ = run2_id;
}

#[tokio::test]
async fn managed_worker_terminal_run_ends_agent_and_unblocks_review_including_sibling_tasks() {
    let t = T::new();
    let tok = create_device(&t, "runner-a").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "rev-gate", &[REPO], 2).await;

    let t1 = t.task(WORKER, "rev-gate", "Primary worker task").await;
    let t2 = t
        .task(WORKER, "rev-gate", "Sibling task parked by same worker")
        .await;

    // Claim t1 as a managed worker.
    let (s, c1) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-a",
            "free_units": 8,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["worker"],
            "max_claims": 1
        }),
    )
    .await;
    assert_eq!(s, 200, "{c1}");
    let run_id = c1["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    let worker_agent = "t3:mcp:worker-no-stop-hook";
    t.register(worker_agent, Some(REPO), None).await;

    // Bind the managed run to `worker_agent` and transition it to `running`.
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run_id}"),
            None,
            Some(json!({ "state": "running", "agent_id": worker_agent })),
        )
        .await
        .0,
        200
    );

    // Worker claims both t1 and t2, leases a file, adds PR notes, and parks both in `review`
    // (without ever firing a Stop hook, matching AGENT_BOARD_HOOKS=off).
    assert_eq!(t.claim(worker_agent, &t1).await.0, 200);
    let _ = t
        .ok(
            "POST",
            "/v1/leases",
            Some(worker_agent),
            Some(
                json!({ "agent": worker_agent, "repo": REPO, "task": t1, "globs": ["src/lib.rs"] }),
            ),
        )
        .await;
    let _ = t
        .ok(
            "POST",
            &format!("/v1/tasks/{t1}/notes"),
            Some(worker_agent),
            Some(json!({ "agent": worker_agent, "kind": "note", "text": "PR: https://github.com/acme/repo/pull/42" })),
        )
        .await;
    assert_eq!(
        t.patch(worker_agent, &t1, json!({ "status": "review" }))
            .await
            .0,
        200
    );
    assert_eq!(t.claim(worker_agent, &t2).await.0, 200);
    let _ = t
        .ok(
            "POST",
            &format!("/v1/tasks/{t2}/notes"),
            Some(worker_agent),
            Some(json!({ "agent": worker_agent, "kind": "note", "text": "PR: https://github.com/acme/repo/pull/42" })),
        )
        .await;
    assert_eq!(
        t.patch(worker_agent, &t2, json!({ "status": "review" }))
            .await
            .0,
        200
    );

    // While the managed worker run is still `running`, `Review` must NOT start.
    let (s, rev_while_running) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-a",
            "free_units": 8,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["review"],
            "max_claims": 2
        }),
    )
    .await;
    assert_eq!(s, 200, "{rev_while_running}");
    assert!(
        rev_while_running["claimed"].as_array().unwrap().is_empty(),
        "review must wait while worker run is still active: {rev_while_running}"
    );

    // Managed worker run finishes (`succeeded`): server must transition `worker_agent` from
    // `running` -> `ended`, release its file leases, and immediately allow `Review` on BOTH
    // t1 (primary) and t2 (sibling).
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run_id}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM agents WHERE id = '{worker_agent}' AND state = 'ended'"
        )),
        1,
        "terminal managed run must mark its agent_id ended"
    );
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM leases WHERE agent = '{worker_agent}'"
        )),
        0,
        "terminal managed run must release the ended agent's file leases"
    );
    assert_eq!(
        t.count(
            "SELECT COUNT(*) FROM events \
             WHERE type = 'agent_state_changed' \
               AND json_extract(payload, '$.cause') = 'managed_run_terminal'"
        ),
        1,
        "must emit Trail Rule agent_state_changed event"
    );

    // Simulate a pre-upgrade legacy stuck row (`agents.state = 'running'` despite terminal run)
    // and confirm BOTH `is_review_ready` (primary + sibling) AND `sweep_expired` handle it.
    t.exec(&format!(
        "UPDATE agents SET state = 'running' WHERE id = '{worker_agent}'"
    ));
    let (s, rev_after) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-a",
            "free_units": 8,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["review"],
            "max_claims": 2
        }),
    )
    .await;
    assert_eq!(s, 200, "{rev_after}");
    assert_eq!(
        rev_after["claimed"].as_array().unwrap().len(),
        2,
        "review must immediately dispatch for both primary and sibling tasks once managed run is terminal: {rev_after}"
    );
    for claimed in rev_after["claimed"].as_array().unwrap() {
        let rid = claimed["run"]["id"].as_str().unwrap();
        assert_eq!(
            send_as(
                &t,
                &tok,
                "PATCH",
                &format!("/v1/runs/{rid}"),
                None,
                Some(json!({ "state": "cancelled" })),
            )
            .await
            .0,
            200
        );
    }
    t.exec(&format!(
        "UPDATE tasks SET status = 'done' WHERE id IN ('{t1}', '{t2}')"
    ));

    // `sweep_expired` must reconcile the legacy `running` agent back to `ended`.
    let swept = t.sweep_runs();
    assert!(swept >= 1, "expected sweep_runs to reconcile stuck agent");
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM agents WHERE id = '{worker_agent}' AND state = 'ended'"
        )),
        1,
        "sweep_expired must reconcile pre-existing finished managed agents to ended"
    );

    // A managed `t3` worker finishing (`succeeded`) suppresses Stop hooks too: it keeps
    // `agents.state = 'running'` (for T3 session semantics), still blocks Review while a
    // delegated child is active, and immediately admits Review once no delegate is live.
    let t3_task = t.task(WORKER, "rev-gate", "Managed t3 worker task").await;
    let t3_agent = "t3:managed-worker-no-stop";
    t.register(t3_agent, Some(REPO), None).await;
    let (s, t3_run) = send_as(
        &t,
        &tok,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "task_id": t3_task,
            "kind": "worker",
            "origin": "managed",
            "device": "runner-a",
            "runner": "t3",
            "agent_id": t3_agent,
            "state": "running",
            "lease_ttl_secs": 300
        })),
    )
    .await;
    assert_eq!(s, 200, "{t3_run}");
    let t3_run_id = t3_run["id"].as_str().unwrap();
    assert_eq!(t.claim(t3_agent, &t3_task).await.0, 200);
    let _ = t
        .ok(
            "POST",
            &format!("/v1/tasks/{t3_task}/notes"),
            Some(t3_agent),
            Some(json!({ "agent": t3_agent, "kind": "note", "text": "PR: https://github.com/acme/repo/pull/43" })),
        )
        .await;
    assert_eq!(
        t.patch(t3_agent, &t3_task, json!({ "status": "review" }))
            .await
            .0,
        200
    );
    // Attach a live delegated child thread to `t3_agent`.
    let child_agent = "t3:managed-worker-child";
    t.ok(
        "POST",
        "/v1/agents",
        Some(child_agent),
        Some(json!({
            "id": child_agent,
            "runtime": "antigravity",
            "device": "mac",
            "repo": REPO,
            "native_ids": { "t3_thread": &child_agent[3..], "t3_parent": &t3_agent[3..] }
        })),
    )
    .await;
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{t3_run_id}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await
        .0,
        200
    );
    // While the delegated child is live, Review must wait.
    let (s, rev_blocked_by_child) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-a",
            "free_units": 8,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["review"],
            "max_claims": 2
        }),
    )
    .await;
    assert_eq!(s, 200, "{rev_blocked_by_child}");
    assert!(
        rev_blocked_by_child["claimed"]
            .as_array()
            .unwrap()
            .is_empty(),
        "review must wait while delegated child is active: {rev_blocked_by_child}"
    );
    // Once the delegated child ends, Review immediately dispatches for the finished managed t3 task.
    t.ok(
        "POST",
        &format!("/v1/agents/{child_agent}/state"),
        Some(child_agent),
        Some(json!({ "state": "ended" })),
    )
    .await;
    let (s, rev_t3) = claim_as(
        &t,
        &tok,
        json!({
            "device": "runner-a",
            "free_units": 8,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["review"],
            "max_claims": 2
        }),
    )
    .await;
    assert_eq!(s, 200, "{rev_t3}");
    assert_eq!(
        rev_t3["claimed"].as_array().unwrap().len(),
        1,
        "finished managed t3 worker must admit Review once delegate ends: {rev_t3}"
    );
    assert_eq!(rev_t3["claimed"][0]["task_id"], t3_task);

    // Binding `agent_id` on a terminal transition absorbs any active `run-interactive-*`
    // placeholder so the finished managed ACP agent still transitions to `ended`.
    let late_task = t
        .task(
            WORKER,
            "rev-gate",
            "Late agent_id binding on terminal patch",
        )
        .await;
    let late_agent = "t3:late-bound-acp-worker";
    t.register(late_agent, Some(REPO), None).await;
    let (s, late_run) = send_as(
        &t,
        &tok,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "task_id": late_task,
            "kind": "worker",
            "origin": "managed",
            "device": "runner-a",
            "runner": "acp",
            "state": "running",
            "lease_ttl_secs": 300
        })),
    )
    .await;
    assert_eq!(s, 200, "{late_run}");
    let late_run_id = late_run["id"].as_str().unwrap();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{late_run_id}"),
            None,
            Some(json!({ "state": "succeeded", "agent_id": late_agent })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM agents WHERE id = '{late_agent}' AND state = 'ended'"
        )),
        1,
        "terminal PATCH attaching agent_id must absorb interactive placeholder and end agent"
    );
}

#[tokio::test]
async fn managed_t3_worker_terminal_run_releases_leases_without_ending_agent() {
    let t = T::new();
    let tok = create_device(&t, "runner-a").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "t3-leases", &[REPO], 2).await;

    let task_id = t.task(WORKER, "t3-leases", "Managed t3 worker task").await;
    let t3_agent = "t3:worker-leases";
    t.register(t3_agent, Some(REPO), None).await;
    let (s, run) = send_as(
        &t,
        &tok,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "task_id": task_id,
            "kind": "worker",
            "origin": "managed",
            "device": "runner-a",
            "runner": "t3",
            "agent_id": t3_agent,
            "state": "running",
            "lease_ttl_secs": 300
        })),
    )
    .await;
    assert_eq!(s, 200, "{run}");
    let run_id = run["id"].as_str().unwrap();
    assert_eq!(t.claim(t3_agent, &task_id).await.0, 200);
    t.ok(
        "POST",
        "/v1/leases",
        Some(t3_agent),
        Some(json!({ "agent": t3_agent, "repo": REPO, "task": task_id, "globs": ["SETUP.md"] })),
    )
    .await;
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM leases WHERE agent = '{t3_agent}'"
        )),
        1
    );

    // Managed t3 run succeeds -> file leases released immediately, while `agents.state` stays `running`.
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{run_id}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM leases WHERE agent = '{t3_agent}'"
        )),
        0,
        "finished managed t3 worker must release its file leases"
    );
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM agents WHERE id = '{t3_agent}' AND state = 'running'"
        )),
        1,
        "finished managed t3 worker must preserve agents.state = 'running'"
    );

    // Pre-upgrade lingering lease on a finished managed t3 agent is reconciled by `sweep_runs()`.
    t.ok(
        "POST",
        "/v1/leases",
        Some(t3_agent),
        Some(json!({ "agent": t3_agent, "repo": REPO, "task": task_id, "globs": ["README.md"] })),
    )
    .await;
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM leases WHERE agent = '{t3_agent}'"
        )),
        1
    );
    assert!(t.sweep_runs() >= 1);
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM leases WHERE agent = '{t3_agent}'"
        )),
        0,
        "sweep_runs must release lingering leases of finished managed t3 workers"
    );
}

#[tokio::test]
async fn deploy_train_debounces_while_sibling_in_review_batches_merges_and_flushes_p1_immediately()
{
    let t = T::new();
    let tok = create_device(&t, "runner-a").await;
    t.register(WORKER, Some(REPO), None).await;
    t.register(REVIEWER, Some(REPO), None).await;
    let dispatch_agent = "dispatch:runner-a";
    let (s, _) = send_as(
        &t,
        &tok,
        "POST",
        "/v1/agents",
        Some(dispatch_agent),
        Some(
            json!({ "id": dispatch_agent, "runtime": "claude", "role": "coordinator", "device": "runner-a", "repo": REPO }),
        ),
    )
    .await;
    assert_eq!(s, 200);
    autopilot_board(&t, "train-board", &[REPO], 2).await;

    // Helper: put a task into `awaiting_after_merge` with a standard deploy/upgrade.sh pass.
    let make_deploy_waiting = |title: &str, prio: &str| {
        let t = &t;
        let tok = &tok;
        let title = title.to_owned();
        let prio = prio.to_owned();
        async move {
            let tid = t
                .task_with(
                    WORKER,
                    json!({ "board": "train-board", "title": title, "priority": prio }),
                )
                .await;
            assert_eq!(t.claim(WORKER, &tid).await.0, 200);
            assert_eq!(
                t.patch(WORKER, &tid, json!({ "status": "review" })).await.0,
                200
            );
            let (s, rev) = send_as(
                t,
                tok,
                "POST",
                "/v1/runs",
                None,
                Some(json!({
                    "task_id": tid,
                    "kind": "review",
                    "origin": "managed",
                    "device": "runner-a",
                    "runner": "acp",
                    "agent_id": REVIEWER,
                    "state": "running",
                    "lease_ttl_secs": 300
                })),
            )
            .await;
            assert_eq!(s, 200, "{rev}");
            let rev_id = rev["id"].as_str().unwrap();
            let pass_note = t
                .ok(
                    "POST",
                    &format!("/v1/tasks/{tid}/notes"),
                    Some(REVIEWER),
                    Some(json!({
                        "agent": REVIEWER,
                        "kind": "decision",
                        "text": "review: pass. Ready.\nafter merge: deploy/upgrade.sh --yes (server + devices)\npr: https://github.com/example/repo/pull/10"
                    })),
                )
                .await;
            let pass_id = pass_note["id"].as_i64().unwrap();
            assert_eq!(
                send_as(
                    t,
                    tok,
                    "PATCH",
                    &format!("/v1/runs/{rev_id}"),
                    None,
                    Some(json!({ "state": "succeeded" })),
                )
                .await
                .0,
                200
            );
            let (s, _) = send_as(
                t,
                tok,
                "POST",
                &format!("/v1/tasks/{tid}/notes"),
                Some(dispatch_agent),
                Some(json!({
                    "agent": dispatch_agent,
                    "kind": "note",
                    "text": format!("note: merged; waiting for after-merge checks (pass #{pass_id}):\npr: https://github.com/example/repo/pull/10\ncommit: 1111111111111111111111111111111111111111")
                })),
            )
            .await;
            assert_eq!(s, 200);
            tid
        }
    };

    let d1 = make_deploy_waiting("Merged deploy task 1", "p2").await;
    // A sibling task is STILL in pre-merge review -> Deploy Train debounces d1 for 5m!
    let sibling = t
        .task(WORKER, "train-board", "Sibling still in review")
        .await;
    assert_eq!(t.claim(WORKER, &sibling).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &sibling, json!({ "status": "review" }))
            .await
            .0,
        200
    );

    let claim_merge = || {
        json!({
            "device": "runner-a",
            "free_units": 8,
            "runners": ["deterministic"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["merge"],
            "max_claims": 2
        })
    };
    let (s, debounced) = claim_as(&t, &tok, claim_merge()).await;
    assert_eq!(s, 200, "{debounced}");
    assert!(
        debounced["claimed"].as_array().unwrap().is_empty(),
        "p2 deploy task must debounce while sibling PR is still in pre-merge review: {debounced}"
    );

    // Second PR merges into the train, and sibling finishes (closed) -> wave completes ->
    // Deploy Train immediately flushes BOTH d1 and d2 in ONE batched `after-merge-deploy` run!
    let d2 = make_deploy_waiting("Merged deploy task 2", "p2").await;
    t.exec(&format!(
        "UPDATE tasks SET status = 'done' WHERE id = '{sibling}'"
    ));
    let (s, train) = claim_as(&t, &tok, claim_merge()).await;
    assert_eq!(s, 200, "{train}");
    let claimed = train["claimed"].as_array().unwrap();
    assert_eq!(
        claimed.len(),
        1,
        "both merged tasks must coalesce into ONE deploy run: {train}"
    );
    let tasks: Vec<&str> = claimed[0]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        tasks.len(),
        2,
        "deploy train must carry both tasks: {train}"
    );
    assert!(
        tasks.contains(&d1.as_str()) && tasks.contains(&d2.as_str()),
        "{tasks:?}"
    );
    assert!(
        claimed[0]["run"]["title"]
            .as_str()
            .unwrap()
            .starts_with("after-merge-deploy "),
        "{train}"
    );
    // Cancel that train run so locks release, then verify P1 flushes IMMEDIATELY even with sibling in review!
    let train_run_id = claimed[0]["run"]["id"].as_str().unwrap();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{train_run_id}"),
            None,
            Some(json!({ "state": "cancelled" })),
        )
        .await
        .0,
        200
    );
    t.exec(&format!(
        "UPDATE tasks SET status = 'done' WHERE id IN ('{d1}', '{d2}')"
    ));

    let sibling2 = t
        .task(WORKER, "train-board", "Another sibling in review")
        .await;
    assert_eq!(t.claim(WORKER, &sibling2).await.0, 200);
    assert_eq!(
        t.patch(WORKER, &sibling2, json!({ "status": "review" }))
            .await
            .0,
        200
    );
    let p1_deploy = make_deploy_waiting("Urgent P1 hotfix", "p1").await;
    let (s, urgent) = claim_as(&t, &tok, claim_merge()).await;
    assert_eq!(s, 200, "{urgent}");
    assert_eq!(
        urgent["claimed"].as_array().unwrap().len(),
        1,
        "P1 deploy must bypass debounce and flush immediately: {urgent}"
    );
    assert_eq!(urgent["claimed"][0]["task_id"], p1_deploy);
}

#[tokio::test]
async fn server_default_goal_review_every_secs_throttles_unset_project_boards() {
    let t = T::with(|c| c.defaults.default_goal_review_every_secs = Some(21_600));
    let tok = create_device(&t, "runner-a").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "goal-def", &[REPO], 1).await;
    t.as_user(
        "PATCH",
        "/v1/boards/goal-def",
        Some(json!({ "goal": "Ship reliable code" })),
    )
    .await;

    let t1 = t.task(WORKER, "goal-def", "First completed task").await;
    assert_eq!(t.claim(WORKER, &t1).await.0, 200);
    assert_eq!(
        t.done(
            WORKER,
            &t1,
            json!([{ "kind": "check", "value": "cargo test", "exit_code": 0 }])
        )
        .await
        .0
        .as_u16(),
        200
    );
    let claim_goal = || {
        json!({
            "device": "runner-a",
            "free_units": 8,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["goal"],
            "max_claims": 1
        })
    };
    let (s, g1) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{g1}");
    assert_eq!(g1["claimed"].as_array().unwrap().len(), 1, "{g1}");
    let g1_id = g1["claimed"][0]["run"]["id"].as_str().unwrap();
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{g1_id}"),
            None,
            Some(json!({ "state": "running" })),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send_as(
            &t,
            &tok,
            "PATCH",
            &format!("/v1/runs/{g1_id}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await
        .0,
        200
    );

    // 30 minutes later, a 2nd task finishes: server default 6h cooldown must throttle a 2nd goal run!
    t.advance(TimeDelta::minutes(30));
    let t2 = t.task(WORKER, "goal-def", "Second completed task").await;
    assert_eq!(t.claim(WORKER, &t2).await.0, 200);
    assert_eq!(
        t.done(
            WORKER,
            &t2,
            json!([{ "kind": "check", "value": "cargo test", "exit_code": 0 }])
        )
        .await
        .0
        .as_u16(),
        200
    );
    let (s, g2) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{g2}");
    assert!(
        g2["claimed"].as_array().unwrap().is_empty(),
        "default_goal_review_every_secs must throttle back-to-back goal review within 6h: {g2}"
    );

    // After 6h total, goal review is allowed again!
    t.advance(TimeDelta::hours(6));
    let (s, g3) = claim_as(&t, &tok, claim_goal()).await;
    assert_eq!(s, 200, "{g3}");
    assert_eq!(
        g3["claimed"].as_array().unwrap().len(),
        1,
        "goal review must resume once default_goal_review_every_secs elapses: {g3}"
    );

    // Server default `Some(0)` disables goal reviews on unset project boards.
    let t_off = T::with(|c| c.defaults.default_goal_review_every_secs = Some(0));
    let tok_off = create_device(&t_off, "runner-off").await;
    t_off.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t_off, "goal-off", &[REPO], 1).await;
    t_off
        .as_user(
            "PATCH",
            "/v1/boards/goal-off",
            Some(json!({ "goal": "No auto goal" })),
        )
        .await;
    let toff = t_off.task(WORKER, "goal-off", "Done task").await;
    assert_eq!(t_off.claim(WORKER, &toff).await.0, 200);
    assert_eq!(
        t_off
            .done(
                WORKER,
                &toff,
                json!([{ "kind": "check", "value": "cargo test", "exit_code": 0 }])
            )
            .await
            .0
            .as_u16(),
        200
    );
    let (s, goff) = claim_as(
        &t_off,
        &tok_off,
        json!({
            "device": "runner-off",
            "free_units": 8,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [REPO],
            "supported_kinds": ["goal"],
            "max_claims": 1
        }),
    )
    .await;
    assert_eq!(s, 200, "{goff}");
    assert!(
        goff["claimed"].as_array().unwrap().is_empty(),
        "default_goal_review_every_secs=0 must disable goal reviews on unset project boards: {goff}"
    );
}
