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
    let _prop = t
        .task_with(
            WORKER,
            json!({ "board": "factory", "title": "Prop 1", "body": "Proposal: add metric" }),
        )
        .await;

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
    assert!(
        resp["claimed"][0]["score_breakdown"]["age"]
            .as_f64()
            .unwrap()
            > 5.0,
        "expected significant age boost after 12h: {}",
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
    let _prop_t = t
        .task_with(
            WORKER,
            json!({ "board": "wip", "title": "Prop", "body": "Proposal: cleanup" }),
        )
        .await;
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
