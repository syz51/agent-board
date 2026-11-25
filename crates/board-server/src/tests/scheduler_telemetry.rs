//! Tests for Slice 4.4 (`docs/SCHEDULER_DESIGN.md`, Section 6):
//! - Migration 0033 (`scheduler_policies`, `scheduler_holds`, `pathology_state`) + `v1-default`
//! - Structured `run_outcomes` recording on terminal transitions, orphan sweeps, and interactive end
//! - Versioned `GET/PUT /v1/scheduler/policy` stamping `policy_version` and altering claim order
//! - `GET /v1/stats/scheduler` percentiles, per-device, per-board, per-policy-version, and holds
//! - All 5 automated pathology signals with injectable clock and cooldown deduplication
//! - Deterministic offline `replay_schedule` simulator against a populated SQLite fixture DB

use board_proto::{ReplayPolicyConfig, SchedulerWeights};
use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{REPO, T};
use crate::{Db, ReplayOptions, replay_schedule};

const WORKER: &str = "t3:worker";
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

async fn heartbeat_device(
    t: &T,
    token: &str,
    name: &str,
    caps: &[&str],
    repos: &[&str],
    units: u32,
) {
    let (s, v) = send_as(
        t,
        token,
        "PUT",
        &format!("/v1/devices/{name}/heartbeat"),
        None,
        Some(json!({
            "os": "macos",
            "arch": "arm64",
            "capacity": { "weight_units": units, "mem_gib": 32, "cpu_cores": 10 },
            "free_units": units,
            "capabilities": caps,
            "runners": ["acp", "deterministic", "t3"],
            "repos": repos,
            "protocol_features": ["v1-json", "runs-v1", "dispatch-claim-v1"]
        })),
    )
    .await;
    assert_eq!(s, 200, "{v}");
}

#[tokio::test]
async fn migration_0033_seeds_v1_default_policy_and_policy_api_alters_claim_order() {
    let t = T::new();

    // 1. `/healthz` advertises `scheduler-telemetry-v1`.
    let health = t.ok("GET", "/healthz", None, None).await;
    let features: Vec<&str> = health["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(
        features.contains(&"scheduler-telemetry-v1"),
        "missing feature flag: {features:?}"
    );

    // 2. Seeded `v1-default` policy is active immediately after migration 0033.
    let active = t.ok("GET", "/v1/scheduler/policy", None, None).await;
    assert_eq!(active["version"], "v1-default");
    assert_eq!(active["active"], true);
    assert_eq!(active["lease_ttl_secs"], 180);
    assert_eq!(active["sticky_grace_secs"], 600);
    assert_eq!(active["weights"]["w_kind"], 100.0);
    assert_eq!(active["weights"]["w_prio"], 30.0);
    assert!(active.get("history").is_none());

    // 3. Validation rejects out-of-bounds weights, TTLs, duplicate versions, missing evidence
    //    notes from non-user callers, and unregistered `X-Board-Agent` headers.
    for bad in [
        json!({ "version": "bad version with spaces", "notes": "e" }),
        json!({ "version": "v1-default", "notes": "e" }),
        json!({ "version": "v-no-notes" }),
        json!({ "version": "v-neg", "weights": { "w_fair": -1.0 }, "notes": "e" }),
        json!({ "version": "v-huge", "weights": { "w_kind": 1001.0 }, "notes": "e" }),
        json!({ "version": "v-short-ttl", "lease_ttl_secs": 5, "notes": "e" }),
        json!({ "version": "v-short-grace", "sticky_grace_secs": 10, "notes": "e" }),
    ] {
        let (code, err, v) = t.fail("PUT", "/v1/scheduler/policy", None, Some(bad)).await;
        assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");
    }
    let (code, err, v) = t
        .fail(
            "PUT",
            "/v1/scheduler/policy",
            Some("t3:ghost"),
            Some(json!({ "version": "v-unreg", "notes": "replay evidence" })),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // 4. Set up two boards (`heavy-user` with recent consumption, `fresh` with 0 consumption).
    t.register(WORKER, Some(REPO), None).await;
    let repo_a = "github.com/example/alpha";
    let repo_b = "github.com/example/beta";
    autopilot_board(&t, "heavy-user", &[repo_a], 4).await;
    autopilot_board(&t, "fresh", &[repo_b], 4).await;

    // Seed recent consumption on `heavy-user` via one completed run.
    let seed = t.task(WORKER, "heavy-user", "Seed work").await;
    let claimed = t
        .ok(
            "POST",
            "/v1/dispatch/claim",
            Some(DISPATCH_MAC),
            Some(json!({
                "device": "mac",
                "free_units": 8,
                "runners": ["acp", "deterministic"],
                "capabilities": ["git:cloneable"],
                "repos": [repo_a, repo_b],
            })),
        )
        .await;
    assert_eq!(claimed["claimed"][0]["task_id"], seed);
    assert_eq!(claimed["claimed"][0]["run"]["policy_version"], "v1-default");
    let seed_run_id = claimed["claimed"][0]["run"]["id"].as_str().unwrap();
    t.ok(
        "PATCH",
        &format!("/v1/runs/{seed_run_id}"),
        Some(DISPATCH_MAC),
        Some(json!({ "state": "running" })),
    )
    .await;
    t.ok(
        "PATCH",
        &format!("/v1/runs/{seed_run_id}"),
        Some(DISPATCH_MAC),
        Some(json!({ "state": "succeeded" })),
    )
    .await;

    // Now create P2 (size xs -> +30 WSJF) on `heavy-user` and P2 (size m -> +10 WSJF) on `fresh`.
    // Under `v1-default` (w_fair=12): heavy-user (+30 - 12 = +18) beats fresh (+10).
    // Under `v2-fair-first` (w_fair=100): fresh (+10) beats heavy-user (+30 - 100 = -70),
    // while strict priority bands keep P1 always above P2/P3!
    let heavy_p2 = t
        .task_with(
            WORKER,
            json!({ "board": "heavy-user", "title": "Heavy board P2", "priority": "p2", "size": "xs" }),
        )
        .await;
    let fresh_p2 = t
        .task_with(
            WORKER,
            json!({ "board": "fresh", "title": "Fresh board P2", "priority": "p2", "size": "m" }),
        )
        .await;

    // Activate `v2-fair-first` where `w_fair = 100.0` overwhelms `w_prio = 30.0`, and custom
    // lease_ttl_secs = 240.
    let seq_before = t.ok("GET", "/v1/events?limit=1", None, None).await["next_since"]
        .as_i64()
        .unwrap();
    t.advance(TimeDelta::seconds(10));
    let updated = t
        .ok(
            "PUT",
            "/v1/scheduler/policy",
            Some(WORKER),
            Some(json!({
                "version": "v2-fair-first",
                "weights": {
                    "w_kind": 100.0,
                    "w_prio": 30.0,
                    "w_rank": 10.0,
                    "w_age": 16.0,
                    "w_fair": 100.0,
                    "w_loc": 8.0,
                    "w_pref": 1.0,
                    "w_backfill": 6.0
                },
                "lease_ttl_secs": 240,
                "sticky_grace_secs": 900,
                "notes": "Boost fair-share penalty to prevent heavy-board dominance"
            })),
        )
        .await;
    assert_eq!(updated["version"], "v2-fair-first");
    assert_eq!(updated["active"], true);
    assert_eq!(updated["lease_ttl_secs"], 240);
    assert_eq!(updated["sticky_grace_secs"], 900);
    assert_eq!(updated["created_by"], WORKER);

    // Trail Rule event recorded!
    let ev_types = t.event_types(seq_before).await;
    assert!(
        ev_types.contains(&"scheduler_policy_updated".to_owned()),
        "{ev_types:?}"
    );

    // `?history=true` returns both versions with only `v2-fair-first` active.
    let hist = t
        .ok("GET", "/v1/scheduler/policy?history=true", None, None)
        .await;
    assert_eq!(hist["version"], "v2-fair-first");
    let history = hist["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["version"], "v2-fair-first");
    assert_eq!(history[0]["active"], true);
    assert_eq!(history[1]["version"], "v1-default");
    assert_eq!(history[1]["active"], false);

    // Under `v2-fair-first`, `fresh_p2` now outranks `heavy_p2` within the same priority band!
    let claim2 = t
        .ok(
            "POST",
            "/v1/dispatch/claim",
            Some(DISPATCH_MAC),
            Some(json!({
                "device": "mac",
                "free_units": 8,
                "runners": ["acp", "deterministic"],
                "capabilities": ["git:cloneable"],
                "repos": [repo_a, repo_b],
            })),
        )
        .await;
    assert_eq!(
        claim2["claimed"][0]["task_id"], fresh_p2,
        "v2-fair-first should pick fresh_p2 before {heavy_p2}: {claim2}"
    );
    assert_eq!(
        claim2["claimed"][0]["run"]["policy_version"],
        "v2-fair-first"
    );
    // Lease TTL reflects policy's 240s (4m).
    let leased_at: chrono::DateTime<chrono::Utc> = claim2["claimed"][0]["run"]["created_at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let expires_at: chrono::DateTime<chrono::Utc> = claim2["claimed"][0]["run"]["lease_expires_at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!((expires_at - leased_at).num_seconds(), 240);

    // Explicit `POST /v1/runs` and `queued -> leased` transitions also inherit the active
    // policy's 240s lease TTL when `lease_ttl_secs` is omitted.
    let direct_run = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "board": "fresh",
                "kind": "worker",
                "device": "mac",
                "runner": "acp"
            })),
        )
        .await;
    let d_created: chrono::DateTime<chrono::Utc> =
        direct_run["created_at"].as_str().unwrap().parse().unwrap();
    let d_exp: chrono::DateTime<chrono::Utc> = direct_run["lease_expires_at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!((d_exp - d_created).num_seconds(), 240);

    let queued_run = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "board": "fresh",
                "kind": "command",
                "device": "mac",
                "runner": "acp",
                "state": "queued"
            })),
        )
        .await;
    let qid = queued_run["id"].as_str().unwrap();
    t.advance(TimeDelta::seconds(5));
    let leased_q = t
        .ok(
            "PATCH",
            &format!("/v1/runs/{qid}"),
            Some(DISPATCH_MAC),
            Some(json!({ "state": "leased" })),
        )
        .await;
    let q_exp: chrono::DateTime<chrono::Utc> = leased_q["lease_expires_at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!((q_exp - t.state.now()).num_seconds(), 240);

    // Non-user device token (without X-Board-Agent) attributes `created_by` to the device
    // name (never `"user"`), while `--user` attributes `"user"` and needs no `notes`.
    let dev_pol = t
        .ok(
            "PUT",
            "/v1/scheduler/policy",
            None,
            Some(json!({ "version": "v3-dev", "notes": "device-attributed tuning" })),
        )
        .await;
    assert_eq!(dev_pol["created_by"], "mac");

    let (us, user_pol) = t
        .as_user(
            "PUT",
            "/v1/scheduler/policy",
            Some(json!({ "version": "v4-user" })),
        )
        .await;
    assert_eq!(us.as_u16(), 200, "{user_pol}");
    assert_eq!(user_pol["created_by"], "user");
}

#[tokio::test]
async fn run_outcomes_recorded_on_terminal_transitions_sweep_orphans_and_interactive_end() {
    let t = T::new();
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "factory", &[REPO], 2).await;

    // 1. Create a task at T0, wait 60s, claim via dispatch, wait 15s, start running, wait 120s, succeed.
    let task_id = t
        .task_with(
            WORKER,
            json!({ "board": "factory", "title": "Instrumented task", "weight": "heavy" }),
        )
        .await;
    t.advance(TimeDelta::seconds(60));
    let claim = t
        .ok(
            "POST",
            "/v1/dispatch/claim",
            Some(DISPATCH_MAC),
            Some(json!({
                "device": "mac",
                "free_units": 8,
                "runners": ["acp"],
                "capabilities": ["git:cloneable"],
                "repos": [REPO]
            })),
        )
        .await;
    let run1 = claim["claimed"][0]["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(claim["claimed"][0]["run"]["cost_units"], 4);

    // Let run1 orphan via `sweep_runs()` after lease expiry (181s) -> must record run_outcomes!
    t.advance(TimeDelta::seconds(181));
    assert_eq!(t.sweep_runs(), 1);
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM run_outcomes WHERE run_id = '{run1}' AND terminal_state = 'orphaned' AND ready_to_leased_secs = 60"
        )),
        1
    );

    // 2. Retry the same task -> claim run2, start after 15s, succeed after 120s with caller outcome JSON.
    t.advance(TimeDelta::seconds(20));
    let claim2 = t
        .ok(
            "POST",
            "/v1/dispatch/claim",
            Some(DISPATCH_MAC),
            Some(json!({
                "device": "mac",
                "free_units": 8,
                "runners": ["acp"],
                "capabilities": ["git:cloneable"],
                "repos": [REPO]
            })),
        )
        .await;
    let run2 = claim2["claimed"][0]["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(claim2["claimed"][0]["task_id"], task_id);

    t.advance(TimeDelta::seconds(15));
    t.ok(
        "PATCH",
        &format!("/v1/runs/{run2}"),
        Some(DISPATCH_MAC),
        Some(json!({ "state": "running" })),
    )
    .await;

    t.advance(TimeDelta::seconds(120));
    t.ok(
        "PATCH",
        &format!("/v1/runs/{run2}"),
        Some(DISPATCH_MAC),
        Some(json!({
            "state": "succeeded",
            "outcome": {
                "quota_bucket": "antigravity:default",
                "tokens_in": 12000,
                "tokens_out": 3400,
                "warm_cache_hit": true,
                "ci_runs_before_merge": 2,
                "review_rounds": 1,
                "first_pass_merge": true,
                "conflict_wait_secs": 30
            }
        })),
    )
    .await;

    // Verify exact computed run_outcomes fields:
    // - ready_to_leased_secs = 20s (time since run1 orphaned, not original task creation!)
    // - 4 units * 120s / 60s = 8.0 wum; orphan_count=1, retry_count=1.
    let (r2l, l2r, rsecs, cwait, wum, orph, retry, fpm, wch, bucket): (
        i64,
        i64,
        i64,
        i64,
        f64,
        i64,
        i64,
        i64,
        i64,
        String,
    ) = t
        .state
        .db()
        .read(t.state.now(), t.state.settings(), |cx| {
            Ok(cx.db.query_row(
                "SELECT ready_to_leased_secs, leased_to_running_secs, running_secs, conflict_wait_secs, \
                        weight_unit_minutes, orphan_count, retry_count, first_pass_merge, \
                        warm_cache_hit, quota_bucket \
                 FROM run_outcomes WHERE run_id = ?1",
                [&run2],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )?)
        })
        .unwrap();
    assert_eq!(r2l, 20);
    assert_eq!(l2r, 15);
    assert_eq!(rsecs, 120);
    assert_eq!(cwait, 30);
    assert!((wum - 8.0).abs() < 1e-6, "expected 8.0 wum, got {wum}");
    assert_eq!((orph, retry), (1, 1));
    assert_eq!((fpm, wch), (1, 1));
    assert_eq!(bucket, "antigravity:default");

    // 3. Post-terminal outcome patch updates `run_outcomes` idempotently.
    t.ok(
        "PATCH",
        &format!("/v1/runs/{run2}"),
        Some(DISPATCH_MAC),
        Some(json!({
            "outcome": {
                "quota_bucket": "antigravity:default",
                "ci_runs_before_merge": 3,
                "first_pass_merge": false
            }
        })),
    )
    .await;
    assert_eq!(
        t.count(&format!(
            "SELECT ci_runs_before_merge FROM run_outcomes WHERE run_id = '{run2}'"
        )),
        3
    );
    assert_eq!(
        t.count(&format!(
            "SELECT first_pass_merge FROM run_outcomes WHERE run_id = '{run2}'"
        )),
        0
    );

    // 4. Interactive session end also records a `run_outcomes` row!
    t.advance(TimeDelta::seconds(90));
    t.ok(
        "POST",
        &format!("/v1/agents/{WORKER}/state"),
        Some(WORKER),
        Some(json!({ "state": "ended", "paused_until": null })),
    )
    .await;
    assert_eq!(
        t.count("SELECT COUNT(*) FROM run_outcomes WHERE kind = 'interactive' AND terminal_state = 'succeeded'"),
        1
    );
}

#[tokio::test]
async fn scheduler_stats_aggregates_percentiles_devices_boards_holds_and_policy_cohorts() {
    let t = T::new();
    let pro_tok = create_device(&t, "mac-pro").await;
    let air_tok = create_device(&t, "mac-air").await;
    t.register(WORKER, Some(REPO), None).await;

    let repo_a = "github.com/example/a";
    let repo_b = "github.com/example/b";
    autopilot_board(&t, "alpha", &[repo_a], 4).await;
    autopilot_board(&t, "beta", &[repo_b], 4).await;

    heartbeat_device(
        &t,
        &pro_tok,
        "mac-pro",
        &["git:cloneable"],
        &[repo_a, repo_b],
        12,
    )
    .await;
    heartbeat_device(&t, &air_tok, "mac-air", &["git:cloneable"], &[repo_b], 8).await;

    // Run 1 on alpha under `v1-default` via mac-pro (2u, 60s -> 2.0 wum, wait 10s).
    let _t1 = t.task(WORKER, "alpha", "Alpha 1").await;
    t.advance(TimeDelta::seconds(10));
    let (_, c1) = send_as(
        &t,
        &pro_tok,
        "POST",
        "/v1/dispatch/claim",
        None,
        Some(json!({
            "device": "mac-pro",
            "free_units": 8,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [repo_a, repo_b]
        })),
    )
    .await;
    let r1 = c1["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    t.advance(TimeDelta::seconds(5));
    send_as(
        &t,
        &pro_tok,
        "PATCH",
        &format!("/v1/runs/{r1}"),
        None,
        Some(json!({ "state": "running" })),
    )
    .await;
    t.advance(TimeDelta::seconds(60));
    send_as(
        &t,
        &pro_tok,
        "PATCH",
        &format!("/v1/runs/{r1}"),
        None,
        Some(json!({
            "state": "succeeded",
            "outcome": {
                "warm_cache_hit": true,
                "first_pass_merge": true,
                "ci_runs_before_merge": 1,
                "review_rounds": 1
            }
        })),
    )
    .await;

    // Switch to `v2-tuned` policy!
    t.ok(
        "PUT",
        "/v1/scheduler/policy",
        None,
        Some(json!({ "version": "v2-tuned", "notes": "A/B cohort" })),
    )
    .await;

    // Run 2 on beta under `v2-tuned` via mac-air (2u, 180s -> 6.0 wum, wait 30s).
    let _t2 = t.task(WORKER, "beta", "Beta 1").await;
    t.advance(TimeDelta::seconds(30));
    let (_, c2) = send_as(
        &t,
        &air_tok,
        "POST",
        "/v1/dispatch/claim",
        None,
        Some(json!({
            "device": "mac-air",
            "free_units": 8,
            "runners": ["acp"],
            "capabilities": ["git:cloneable"],
            "repos": [repo_b]
        })),
    )
    .await;
    let r2 = c2["claimed"][0]["run"]["id"].as_str().unwrap().to_owned();
    t.advance(TimeDelta::seconds(10));
    send_as(
        &t,
        &air_tok,
        "PATCH",
        &format!("/v1/runs/{r2}"),
        None,
        Some(json!({ "state": "running" })),
    )
    .await;
    t.advance(TimeDelta::seconds(180));
    send_as(
        &t,
        &air_tok,
        "PATCH",
        &format!("/v1/runs/{r2}"),
        None,
        Some(json!({
            "state": "succeeded",
            "outcome": {
                "warm_cache_hit": false,
                "first_pass_merge": false,
                "ci_runs_before_merge": 3,
                "review_rounds": 2
            }
        })),
    )
    .await;

    // Induce a `requires` hold on `beta` and confirm hourly deduplication in `scheduler_holds`.
    let _gpu_task = t
        .task_with(
            WORKER,
            json!({ "board": "beta", "title": "Needs GPU", "requires": ["tool:gpu"] }),
        )
        .await;
    for _ in 0..2 {
        let (s, resp) = send_as(
            &t,
            &pro_tok,
            "POST",
            "/v1/dispatch/claim",
            None,
            Some(json!({
                "device": "mac-pro",
                "free_units": 8,
                "runners": ["acp"],
                "capabilities": ["git:cloneable"],
                "repos": [repo_a, repo_b]
            })),
        )
        .await;
        assert_eq!(s, 200);
        assert_eq!(resp["holds_summary"][0]["reason"], "requires");
    }
    assert_eq!(
        t.count("SELECT COUNT(*) FROM scheduler_holds WHERE reason = 'requires'"),
        1,
        "second hold within 1h must be deduplicated"
    );

    // Refresh only mac-pro so mac-pro is `online` and mac-air is `stale`.
    heartbeat_device(
        &t,
        &pro_tok,
        "mac-pro",
        &["git:cloneable"],
        &[repo_a, repo_b],
        12,
    )
    .await;

    // Query `GET /v1/stats/scheduler?days=7`.
    let stats = t.ok("GET", "/v1/stats/scheduler?days=7", None, None).await;
    assert_eq!(stats["days"], 7);
    assert_eq!(stats["runs_total"], 2, "{stats}");
    assert_eq!(stats["runs_by_kind"]["worker"], 2);
    assert_eq!(stats["runs_by_terminal_state"]["succeeded"], 2);
    assert_eq!(stats["orphan_rate"], 0.0);
    assert_eq!(stats["first_pass_merge_rate"], 0.5);
    assert_eq!(stats["warm_cache_hit_rate"], 0.5);
    assert_eq!(stats["avg_ci_runs_before_merge"], 2.0);
    assert_eq!(stats["avg_review_rounds"], 1.5);
    assert_eq!(stats["ready_to_leased_secs"]["count"], 2);
    assert_eq!(stats["ready_to_leased_secs"]["p50"], 20.0);
    assert_eq!(stats["ready_to_leased_secs"]["p90"], 30.0);
    assert!((stats["total_weight_unit_minutes"].as_f64().unwrap() - 8.0).abs() < 1e-6);
    assert_eq!(stats["holds_by_reason"]["requires"], 1);

    // Per-device includes live statuses.
    let devs = stats["per_device"].as_array().unwrap();
    let pro_row = devs.iter().find(|d| d["device"] == "mac-pro").unwrap();
    let air_row = devs.iter().find(|d| d["device"] == "mac-air").unwrap();
    assert_eq!(pro_row["status"], "online");
    assert_eq!(pro_row["runs"], 1);
    assert_eq!(air_row["status"], "stale");
    assert_eq!(air_row["runs"], 1);

    // Per-board fair share: alpha = 2/8 (25%), beta = 6/8 (75%) + requires hold on beta.
    let boards = stats["per_board"].as_array().unwrap();
    let alpha_row = boards.iter().find(|b| b["board"] == "alpha").unwrap();
    let beta_row = boards.iter().find(|b| b["board"] == "beta").unwrap();
    assert!((alpha_row["fair_share_percent"].as_f64().unwrap() - 25.0).abs() < 1e-6);
    assert!((beta_row["fair_share_percent"].as_f64().unwrap() - 75.0).abs() < 1e-6);
    assert_eq!(beta_row["holds_by_reason"]["requires"], 1);

    // Per-policy-version cohort comparison!
    let policies = stats["per_policy_version"].as_array().unwrap();
    assert_eq!(policies.len(), 2);
    let v1 = policies
        .iter()
        .find(|p| p["policy_version"] == "v1-default")
        .unwrap();
    let v2 = policies
        .iter()
        .find(|p| p["policy_version"] == "v2-tuned")
        .unwrap();
    assert_eq!(v1["runs"], 1);
    assert_eq!(v1["first_pass_merge_rate"], 1.0);
    assert_eq!(v2["runs"], 1);
    assert_eq!(v2["first_pass_merge_rate"], 0.0);

    // Filtered query by `policy_version=v2-tuned` and `board=beta` includes v2 holds,
    // while `policy_version=v1-default` does NOT leak v2's `requires` hold or `beta` board.
    let filtered = t
        .ok(
            "GET",
            "/v1/stats/scheduler?days=7&board=beta&policy_version=v2-tuned",
            None,
            None,
        )
        .await;
    assert_eq!(filtered["runs_total"], 1);
    assert_eq!(filtered["board"], "beta");
    assert_eq!(filtered["policy_version"], "v2-tuned");
    assert_eq!(filtered["holds_by_reason"]["requires"], 1);

    let v1_only = t
        .ok(
            "GET",
            "/v1/stats/scheduler?days=7&policy_version=v1-default",
            None,
            None,
        )
        .await;
    assert_eq!(v1_only["runs_total"], 1);
    assert_eq!(
        v1_only["holds_by_reason"].as_object().unwrap().len(),
        0,
        "v2-tuned holds must not leak into v1-default cohort: {v1_only}"
    );
    assert_eq!(
        v1_only["per_board"].as_array().unwrap().len(),
        1,
        "only alpha ran under v1-default: {v1_only}"
    );

    // Validation rejects out-of-range days.
    let (code, err, _) = t
        .fail("GET", "/v1/stats/scheduler?days=0", None, None)
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid"));

    // Advancing 36 days and pruning clears old `scheduler_holds`.
    t.advance(TimeDelta::days(36));
    assert!(t.prune_signals() >= 1);
    assert_eq!(t.count("SELECT COUNT(*) FROM scheduler_holds"), 0);
}

#[tokio::test]
async fn automated_pathology_signals_detect_and_deduplicate_all_five_conditions() {
    let t = T::new();
    let runner_tok = create_device(&t, "runner-a").await;
    let flaky_tok = create_device(&t, "flaky-box").await;
    let broken_tok = create_device(&t, "broken-box").await;
    t.register(WORKER, Some(REPO), None).await;

    let repo_u = "github.com/example/unsched";
    let repo_s = "github.com/example/starved";
    let repo_b = "github.com/example/busy";
    autopilot_board(&t, "unsched", &[repo_u], 2).await;
    autopilot_board(&t, "starved", &[repo_s], 2).await;
    autopilot_board(&t, "busy", &[repo_b], 2).await;

    heartbeat_device(
        &t,
        &runner_tok,
        "runner-a",
        &["os:linux", "git:cloneable"],
        &[repo_u, repo_s, repo_b],
        8,
    )
    .await;

    // -------------------------------------------------------------------------
    // 1. `signal:scheduler:unschedulable` (requires tool:gpu, no device has it)
    // -------------------------------------------------------------------------
    let gpu_task = t
        .task_with(
            WORKER,
            json!({ "board": "unsched", "title": "GPU training", "requires": ["tool:gpu"] }),
        )
        .await;
    // Initial sweep at T=0 observes `gpu_task` as unschedulable (starts timer, does not fire).
    let s0 = t.sweep_pathologies();
    assert!(s0.filed.is_empty(), "should not fire at T0: {s0:?}");

    // At +30m (< 1h): must NOT fire yet.
    t.advance(TimeDelta::minutes(30));
    heartbeat_device(
        &t,
        &runner_tok,
        "runner-a",
        &["os:linux", "git:cloneable"],
        &[repo_u, repo_s, repo_b],
        8,
    )
    .await;
    let s1 = t.sweep_pathologies();
    assert!(s1.filed.is_empty(), "should not fire at 30m: {s1:?}");

    // At +65m (> 1h): fires `signal:scheduler:unschedulable:<gpu_task>`!
    t.advance(TimeDelta::minutes(35));
    heartbeat_device(
        &t,
        &runner_tok,
        "runner-a",
        &["os:linux", "git:cloneable"],
        &[repo_u, repo_s, repo_b],
        8,
    )
    .await;
    let s2 = t.sweep_pathologies();
    assert_eq!(s2.filed.len(), 1, "{s2:?}");
    let unsched_sig = format!("signal:scheduler:unschedulable:{gpu_task}");
    let fb_items = t
        .ok("GET", "/v1/tasks?board=board-feedback", None, None)
        .await;
    let unsched_item = fb_items
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["feedback"]["signature"] == unsched_sig)
        .expect("unschedulable signal filed");
    assert_eq!(unsched_item["feedback"]["source"], "signal");
    assert_eq!(unsched_item["feedback"]["about"], "scheduler");
    assert_eq!(unsched_item["feedback"]["reports"], 1);

    // Sweeping again 5m later skips via cooldown (no alert storm!).
    t.advance(TimeDelta::minutes(5));
    heartbeat_device(
        &t,
        &runner_tok,
        "runner-a",
        &["os:linux", "git:cloneable"],
        &[repo_u, repo_s, repo_b],
        8,
    )
    .await;
    let s3 = t.sweep_pathologies();
    assert!(s3.filed.is_empty() && s3.updated.is_empty());
    assert!(s3.skipped_cooldown >= 1);

    // Even if a matching GPU device flaps online for a sweep during cooldown, `last_filed_at`
    // is preserved and `first_seen_at` resets so removing GPU does not immediately re-fire!
    heartbeat_device(
        &t,
        &runner_tok,
        "runner-a",
        &["os:linux", "git:cloneable", "tool:gpu"],
        &[repo_u, repo_s, repo_b],
        8,
    )
    .await;
    let s_flap_on = t.sweep_pathologies();
    assert!(s_flap_on.filed.is_empty() && s_flap_on.updated.is_empty());
    heartbeat_device(
        &t,
        &runner_tok,
        "runner-a",
        &["os:linux", "git:cloneable"],
        &[repo_u, repo_s, repo_b],
        8,
    )
    .await;
    // First sweep after losing GPU restarts consecutive timer (`first_seen_at = now`).
    let s_flap_off = t.sweep_pathologies();
    assert!(s_flap_off.filed.is_empty() && s_flap_off.updated.is_empty());

    // Sweeping 65m later (> threshold AND > cooldown) updates the SAME live item (reports -> 2)!
    t.advance(TimeDelta::minutes(65));
    heartbeat_device(
        &t,
        &runner_tok,
        "runner-a",
        &["os:linux", "git:cloneable"],
        &[repo_u, repo_s, repo_b],
        8,
    )
    .await;
    let s4 = t.sweep_pathologies();
    assert!(s4.filed.is_empty());
    assert_eq!(s4.updated.len(), 1);
    let fb_after = t
        .ok("GET", "/v1/tasks?board=board-feedback", None, None)
        .await;
    let matching: Vec<&Value> = fb_after
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["feedback"]["signature"] == unsched_sig)
        .collect();
    assert_eq!(matching.len(), 1, "must deduplicate onto one live item");
    assert_eq!(matching[0]["feedback"]["reports"], 2);

    // -------------------------------------------------------------------------
    // 2. `signal:scheduler:starvation` (board `starved` waits > 6h while `busy` runs;
    //    `unsched` and a board at its `parallel_tasks` cap must NOT trigger starvation!)
    // -------------------------------------------------------------------------
    autopilot_board(&t, "capped", &[repo_b], 1).await;
    let capped_doing = t.task(WORKER, "capped", "Long running work").await;
    assert_eq!(t.claim(WORKER, &capped_doing).await.0, 200);
    let _capped_waiting = t.task(WORKER, "capped", "Queued behind parallel=1").await;

    let _starved_task = t.task(WORKER, "starved", "Waiting silently").await;
    let busy_task = t.task(WORKER, "busy", "Active work").await;
    t.advance(TimeDelta::hours(6) + TimeDelta::minutes(5));
    heartbeat_device(
        &t,
        &runner_tok,
        "runner-a",
        &["os:linux", "git:cloneable"],
        &[repo_u, repo_s, repo_b],
        8,
    )
    .await;
    // Record a recent managed run on `busy` inside the 6h window (while `starved` has 0).
    let (s, _) = send_as(
        &t,
        &runner_tok,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "task_id": busy_task,
            "kind": "worker",
            "device": "runner-a",
            "runner": "acp"
        })),
    )
    .await;
    assert_eq!(s, 200);

    let s_starve = t.sweep_pathologies();
    assert!(
        !s_starve.filed.is_empty(),
        "expected starvation signal: {s_starve:?}"
    );
    assert_eq!(
        t.count(
            "SELECT COUNT(*) FROM feedback WHERE signature = 'signal:scheduler:starvation:starved'"
        ),
        1
    );
    assert_eq!(
        t.count(
            "SELECT COUNT(*) FROM feedback WHERE signature = 'signal:scheduler:starvation:unsched'"
        ),
        0,
        "unschedulable tasks must not trigger starvation"
    );
    assert_eq!(
        t.count(
            "SELECT COUNT(*) FROM feedback WHERE signature = 'signal:scheduler:starvation:capped'"
        ),
        0,
        "boards at their parallel_tasks cap must not trigger starvation"
    );

    // -------------------------------------------------------------------------
    // 3. `signal:scheduler:orphan_storm` (3 orphaned runs on `flaky-box` within 1h)
    // -------------------------------------------------------------------------
    for i in 0..3 {
        let (s, run) = send_as(
            &t,
            &flaky_tok,
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "board": "busy",
                "kind": "worker",
                "device": "flaky-box",
                "runner": "acp",
                "title": format!("flaky {i}"),
                "lease_ttl_secs": 30
            })),
        )
        .await;
        assert_eq!(s, 200, "{run}");
        t.advance(TimeDelta::seconds(35));
        assert!(t.sweep_runs() >= 1);
    }
    t.sweep_pathologies();
    assert_eq!(
        t.count("SELECT COUNT(*) FROM feedback WHERE signature = 'signal:scheduler:orphan_storm:flaky-box'"),
        1
    );

    // -------------------------------------------------------------------------
    // 4. `signal:scheduler:conflict_hot` (conflict key held >= 30% of active hours
    //    across >= 3 distinct hours; a burst of 3 holds in 1 single hour must NOT fire!)
    // -------------------------------------------------------------------------
    autopilot_board(&t, "contended", &[repo_b], 4).await;
    let lock_task = t.task(WORKER, "contended", "Holder").await;
    let burst_key = "repo:github.com/example/burst:merge";
    let (s, _) = send_as(
        &t,
        &runner_tok,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "task_id": lock_task,
            "kind": "merge",
            "device": "runner-a",
            "runner": "deterministic",
            "conflict_keys": [{ "key": burst_key, "max": 1 }],
            "lease_ttl_secs": 3600
        })),
    )
    .await;
    assert_eq!(s, 200);
    for i in 0..3 {
        let bt = t
            .task_with(
                WORKER,
                json!({
                    "board": "contended",
                    "title": format!("Burst waiter {i}"),
                    "conflict_keys": [{ "key": burst_key, "max": 1 }]
                }),
            )
            .await;
        let _ = bt;
    }
    let (s, _) = send_as(
        &t,
        &runner_tok,
        "POST",
        "/v1/dispatch/claim",
        None,
        Some(json!({
            "device": "runner-a",
            "free_units": 8,
            "runners": ["acp", "deterministic"],
            "capabilities": ["git:cloneable"],
            "repos": [repo_b],
            "supported_kinds": ["worker"]
        })),
    )
    .await;
    assert_eq!(s, 200);
    t.sweep_pathologies();
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM feedback WHERE signature = 'signal:scheduler:conflict_hot:{burst_key}'"
        )),
        0,
        "3 holds in a single hour must not trigger conflict_hot"
    );

    let wait_task = t.task(WORKER, "contended", "Blocked by conflict").await;
    let hot_key = "repo:github.com/example/busy:merge";
    let (s, _) = send_as(
        &t,
        &runner_tok,
        "POST",
        "/v1/runs",
        None,
        Some(json!({
            "task_id": lock_task,
            "kind": "merge",
            "device": "runner-a",
            "runner": "deterministic",
            "conflict_keys": [{ "key": hot_key, "max": 1 }],
            "lease_ttl_secs": 14400
        })),
    )
    .await;
    assert_eq!(s, 200);
    // Attach the same conflict key to `wait_task` and trigger 3 holds across 3 hours.
    assert_eq!(
        t.patch(
            WORKER,
            &wait_task,
            json!({ "conflict_keys": [{ "key": hot_key, "max": 1 }] })
        )
        .await
        .0,
        200
    );
    for _ in 0..3 {
        t.advance(TimeDelta::minutes(65));
        let (s, r) = send_as(
            &t,
            &runner_tok,
            "POST",
            "/v1/dispatch/claim",
            None,
            Some(json!({
                "device": "runner-a",
                "free_units": 8,
                "runners": ["acp", "deterministic"],
                "capabilities": ["git:cloneable"],
                "repos": [repo_b],
                "supported_kinds": ["worker"]
            })),
        )
        .await;
        assert_eq!(s, 200, "{r}");
    }
    t.sweep_pathologies();
    let hot_sig = format!("signal:scheduler:conflict_hot:{hot_key}");
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM feedback WHERE signature = '{hot_sig}'"
        )),
        1
    );

    // -------------------------------------------------------------------------
    // 5. `signal:runner:spawn_fail` (3 fast failures <= 30s on `broken-box:acp`)
    // -------------------------------------------------------------------------
    // First add a slow failure (120s) on `broken-box:t3` -> must NOT trigger spawn_fail!
    for _ in 0..3 {
        let (s, run) = send_as(
            &t,
            &broken_tok,
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "board": "busy",
                "kind": "worker",
                "device": "broken-box",
                "runner": "t3",
                "state": "running"
            })),
        )
        .await;
        assert_eq!(s, 200);
        let id = run["id"].as_str().unwrap();
        t.advance(TimeDelta::seconds(90));
        send_as(
            &t,
            &broken_tok,
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "state": "failed", "reason": "test failure" })),
        )
        .await;
    }
    // Now add 3 fast failures (5s) on `broken-box:acp` -> MUST trigger!
    for _ in 0..3 {
        let (s, run) = send_as(
            &t,
            &broken_tok,
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "board": "busy",
                "kind": "worker",
                "device": "broken-box",
                "runner": "acp",
                "state": "running"
            })),
        )
        .await;
        assert_eq!(s, 200);
        let id = run["id"].as_str().unwrap();
        t.advance(TimeDelta::seconds(5));
        send_as(
            &t,
            &broken_tok,
            "PATCH",
            &format!("/v1/runs/{id}"),
            None,
            Some(json!({ "state": "failed", "reason": "binary not found" })),
        )
        .await;
    }
    t.sweep_pathologies();
    assert_eq!(
        t.count("SELECT COUNT(*) FROM feedback WHERE signature = 'signal:runner:spawn_fail:broken-box:acp' AND about = 'runner'"),
        1
    );
    assert_eq!(
        t.count("SELECT COUNT(*) FROM feedback WHERE signature = 'signal:runner:spawn_fail:broken-box:t3'"),
        0,
        "slow failures (>30s) must not trigger spawn_fail"
    );
}

#[tokio::test]
async fn deterministic_offline_replay_compares_baseline_and_candidate_policies() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("board.db");
    let db = Db::open(&db_path).unwrap();
    let t = T::over(db, |_| {});

    let pro_tok = create_device(&t, "mac-pro").await;
    let air_tok = create_device(&t, "mac-air").await;
    t.register(WORKER, Some(REPO), None).await;

    let repo_a = "github.com/example/repo-a";
    let repo_b = "github.com/example/repo-b";
    autopilot_board(&t, "alpha", &[repo_a], 2).await;
    autopilot_board(&t, "beta", &[repo_b], 2).await;

    // mac-pro has 6u and repo_a warm; mac-air has 2u and repo_b warm (enables backfill + locality!).
    heartbeat_device(&t, &pro_tok, "mac-pro", &["git:cloneable"], &[repo_a], 6).await;
    heartbeat_device(&t, &air_tok, "mac-air", &["git:cloneable"], &[repo_b], 2).await;

    // Populate a realistic historical workload: heavy tasks (4u) + light tasks (1u) across both boards.
    for i in 0..4 {
        t.task_with(
            WORKER,
            json!({
                "board": "alpha",
                "title": format!("Heavy alpha {i}"),
                "priority": "p1",
                "weight": "heavy"
            }),
        )
        .await;
        t.advance(TimeDelta::seconds(30));
    }
    for i in 0..4 {
        t.task_with(
            WORKER,
            json!({
                "board": "beta",
                "title": format!("Light beta {i}"),
                "priority": "p3",
                "weight": "light"
            }),
        )
        .await;
        t.advance(TimeDelta::seconds(30));
    }

    // Full-drain horizon (+25m) schedules all 8 tasks; also test a tight horizon where
    // capacity is saturated so policy weights genuinely shift `fair_share_gini`!
    let until = t.state.now() + TimeDelta::minutes(25);
    let candidate = ReplayPolicyConfig {
        version: "v2-locality-fair".to_owned(),
        weights: SchedulerWeights {
            w_fair: 80.0,
            w_loc: 50.0,
            w_backfill: 20.0,
            ..SchedulerWeights::default()
        },
        lease_ttl_secs: None,
        sticky_grace_secs: None,
    };

    let run1 = replay_schedule(
        &db_path,
        ReplayOptions {
            since: None,
            until,
            days: 7,
            candidate: Some(candidate.clone()),
        },
    )
    .unwrap();
    let run2 = replay_schedule(
        &db_path,
        ReplayOptions {
            since: None,
            until,
            days: 7,
            candidate: Some(candidate),
        },
    )
    .unwrap();

    // Bit-for-bit deterministic across repeated replays!
    assert_eq!(run1, run2);
    assert_eq!(run1.historical_tasks, 8);
    assert_eq!(run1.baseline.policy_version, "v1-default");
    assert_eq!(run1.baseline.tasks_scheduled, 8);
    assert!(run1.baseline.p50_wait_secs.is_some());
    assert!(run1.baseline.p90_wait_secs.is_some());
    assert!(run1.baseline.locality_hits > 0);
    assert!(run1.baseline.backfill_count > 0);
    let cand = run1.candidate.as_ref().expect("candidate comparison");
    assert_eq!(cand.policy_version, "v2-locality-fair");
    assert_eq!(cand.tasks_scheduled, 8);
    assert!(cand.locality_hits >= run1.baseline.locality_hits);

    // Tight horizon test: on a separate DB with 1 device (2u) and 2 boards in the SAME priority
    // band (`alpha` P2 size=xs [+30 WSJF] vs `beta` P2 size=m [+10 WSJF]), a 7-minute window
    // fits only 2 sequential 5-minute runs:
    // - baseline (w_fair=12): picks alpha twice (30-12=18 > 10) -> beta gets 0 wum (gini = 0.5).
    // - fair-first (w_fair=120): picks alpha once and beta once -> equal wum (gini = 0.0)!
    let dir2 = tempfile::tempdir().unwrap();
    let db2_path = dir2.path().join("tight.db");
    let db2 = Db::open(&db2_path).unwrap();
    let t2 = T::over(db2, |_| {});
    let dev_tok = create_device(&t2, "single-runner").await;
    t2.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t2, "alpha", &[repo_a], 4).await;
    autopilot_board(&t2, "beta", &[repo_b], 4).await;
    heartbeat_device(
        &t2,
        &dev_tok,
        "single-runner",
        &["git:cloneable"],
        &[repo_a, repo_b],
        2,
    )
    .await;
    for i in 0..3 {
        t2.task_with(
            WORKER,
            json!({ "board": "alpha", "title": format!("Alpha P2 {i}"), "priority": "p2", "size": "xs" }),
        )
        .await;
        t2.task_with(
            WORKER,
            json!({ "board": "beta", "title": format!("Beta P2 {i}"), "priority": "p2", "size": "m" }),
        )
        .await;
    }
    let tight_cmp = replay_schedule(
        &db2_path,
        ReplayOptions {
            since: None,
            until: t2.state.now() + TimeDelta::minutes(7),
            days: 1,
            candidate: Some(ReplayPolicyConfig {
                version: "v2-fair-tight".to_owned(),
                weights: SchedulerWeights {
                    w_fair: 120.0,
                    ..SchedulerWeights::default()
                },
                lease_ttl_secs: None,
                sticky_grace_secs: None,
            }),
        },
    )
    .unwrap();
    assert_eq!(tight_cmp.baseline.tasks_scheduled, 2);
    assert!(
        tight_cmp.baseline.fair_share_gini > 0.4,
        "baseline should starve beta in tight window: {:?}",
        tight_cmp.baseline
    );
    let tight_cand = tight_cmp.candidate.as_ref().unwrap();
    assert_eq!(tight_cand.tasks_scheduled, 2);
    assert!(
        tight_cand.fair_share_gini < tight_cmp.baseline.fair_share_gini,
        "fair-first policy must reduce Gini in tight window: baseline={:?} cand={:?}",
        tight_cmp.baseline,
        tight_cand
    );
}

#[tokio::test]
async fn run_outcomes_token_usage_cache_derivation_and_merge_quality_rollups() {
    let t = T::new();
    t.register(WORKER, Some(REPO), None).await;
    t.register("t3:reviewer", Some(REPO), None).await;
    autopilot_board(&t, "alpha", &[REPO], 2).await;

    // 1. Worker run with tokens_in + cached_tokens >= 50% -> derives warm_cache_hit = 1.
    let task1 = t.task(WORKER, "alpha", "First-pass task").await;
    let w1 = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "task_id": task1,
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "state": "running"
            })),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    t.advance(TimeDelta::seconds(30));
    t.ok(
        "PATCH",
        &format!("/v1/runs/{w1}"),
        None,
        Some(json!({
            "state": "succeeded",
            "outcome": {
                "tokens_in": 10_000,
                "cached_tokens": 8_000,
                "tokens_out": 500,
                "reasoning_tokens": 150
            }
        })),
    )
    .await;

    // 2. Orphaned review retry on task1 must NOT inflate review_rounds or break first_pass_merge!
    let rev_orph = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "task_id": task1,
                "kind": "review",
                "device": "mac",
                "runner": "acp",
                "state": "running"
            })),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    t.ok(
        "PATCH",
        &format!("/v1/runs/{rev_orph}"),
        None,
        Some(json!({ "state": "orphaned" })),
    )
    .await;

    // Review run for task1 with cached_tokens < 50% -> derives warm_cache_hit = 0.
    let rev1 = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "task_id": task1,
                "kind": "review",
                "device": "mac",
                "runner": "acp",
                "agent_id": "t3:reviewer",
                "state": "running"
            })),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    t.ok(
        "POST",
        &format!("/v1/tasks/{task1}/notes"),
        Some("t3:reviewer"),
        Some(
            json!({ "agent": "t3:reviewer", "kind": "decision", "text": "review: pass\npr: none" }),
        ),
    )
    .await;
    t.advance(TimeDelta::seconds(15));
    t.ok(
        "PATCH",
        &format!("/v1/runs/{rev1}"),
        None,
        Some(json!({
            "state": "succeeded",
            "outcome": {
                "tokens_in": 10_000,
                "cached_tokens": 4_000,
                "tokens_out": 400
            }
        })),
    )
    .await;

    // 3. Deferred merge run on task1 does NOT record quality rollups.
    let m_def = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "task_id": task1,
                "kind": "merge",
                "device": "mac",
                "runner": "deterministic",
                "state": "running"
            })),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    t.ok(
        "PATCH",
        &format!("/v1/runs/{m_def}"),
        None,
        Some(json!({
            "state": "succeeded",
            "outcome": { "status": "deferred", "reason": "checks pending" }
        })),
    )
    .await;
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM run_outcomes WHERE run_id = '{m_def}' AND first_pass_merge IS NOT NULL"
        )),
        0
    );

    // 4. Actual succeeded merge run on task1 -> review_rounds=1, first_pass_merge=1.
    let m1 = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "task_id": task1,
                "kind": "merge",
                "device": "mac",
                "runner": "deterministic",
                "state": "running"
            })),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    t.ok(
        "PATCH",
        &format!("/v1/runs/{m1}"),
        None,
        Some(json!({
            "state": "succeeded",
            "outcome": { "status": "merged_and_done" }
        })),
    )
    .await;

    // 5. Second task with 2 terminal review runs and a `review: fail` note -> review_rounds=2, first_pass_merge=0.
    let task2 = t.task(WORKER, "alpha", "Two-round task").await;
    for note in ["review: fail: missing test", "review: pass\npr: none"] {
        let rev = t
            .ok(
                "POST",
                "/v1/runs",
                None,
                Some(json!({
                    "task_id": task2,
                    "kind": "review",
                    "device": "mac",
                    "runner": "acp",
                    "agent_id": "t3:reviewer",
                    "state": "running"
                })),
            )
            .await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        t.ok(
            "POST",
            &format!("/v1/tasks/{task2}/notes"),
            Some("t3:reviewer"),
            Some(json!({ "agent": "t3:reviewer", "kind": "decision", "text": note })),
        )
        .await;
        t.ok(
            "PATCH",
            &format!("/v1/runs/{rev}"),
            None,
            Some(json!({ "state": "succeeded" })),
        )
        .await;
    }
    let m2 = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "task_id": task2,
                "kind": "merge",
                "device": "mac",
                "runner": "deterministic",
                "state": "running"
            })),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    t.ok(
        "PATCH",
        &format!("/v1/runs/{m2}"),
        None,
        Some(json!({ "state": "succeeded" })),
    )
    .await;

    // Verify `GET /v1/stats/scheduler` aggregates both token rollups and quality rollups accurately.
    let stats: board_proto::SchedulerStats =
        serde_json::from_value(t.ok("GET", "/v1/stats/scheduler?days=7", None, None).await)
            .unwrap();
    assert_eq!(stats.runs_with_tokens, 2);
    assert_eq!(stats.tokens_in_total, Some(20_000));
    assert_eq!(stats.cached_tokens_total, Some(12_000));
    assert_eq!(stats.tokens_out_total, Some(900));
    assert_eq!(stats.reasoning_tokens_total, Some(150));
    assert!((stats.token_cache_hit_rate.unwrap() - 0.6).abs() < 1e-6);
    assert!((stats.warm_cache_hit_rate.unwrap() - 0.5).abs() < 1e-6);
    assert!((stats.first_pass_merge_rate.unwrap() - 0.5).abs() < 1e-6);
    assert!((stats.avg_review_rounds.unwrap() - 1.5).abs() < 1e-6);
    assert_eq!(stats.tokens_by_kind["worker"].runs, 1);
    assert_eq!(stats.tokens_by_kind["worker"].tokens_in, 10_000);
    assert_eq!(stats.tokens_by_kind["worker"].cached_tokens, 8_000);
    assert_eq!(stats.tokens_by_kind["worker"].reasoning_tokens, 150);
    assert!((stats.tokens_by_kind["worker"].cache_hit_rate.unwrap() - 0.8).abs() < 1e-6);
    assert_eq!(stats.tokens_by_kind["review"].runs, 1);
    assert_eq!(stats.tokens_by_kind["review"].cached_tokens, 4_000);
    assert!((stats.tokens_by_kind["review"].cache_hit_rate.unwrap() - 0.4).abs() < 1e-6);

    // A board whose runs report only `tokens_in`/`tokens_out` leaves `cached_tokens_total`
    // and `reasoning_tokens_total` as `None` (not misleading `Some(0)`).
    autopilot_board(&t, "nocache", &[REPO], 1).await;
    let nc_task = t.task(WORKER, "nocache", "No-cache-field task").await;
    let nc_run = t
        .ok(
            "POST",
            "/v1/runs",
            None,
            Some(json!({
                "task_id": nc_task,
                "kind": "worker",
                "device": "mac",
                "runner": "acp",
                "state": "running"
            })),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    t.ok(
        "PATCH",
        &format!("/v1/runs/{nc_run}"),
        None,
        Some(json!({
            "state": "succeeded",
            "outcome": { "tokens_in": 5_000, "tokens_out": 200 }
        })),
    )
    .await;
    let nc_stats: board_proto::SchedulerStats = serde_json::from_value(
        t.ok(
            "GET",
            "/v1/stats/scheduler?days=7&board=nocache",
            None,
            None,
        )
        .await,
    )
    .unwrap();
    assert_eq!(nc_stats.runs_with_tokens, 1);
    assert_eq!(nc_stats.tokens_in_total, Some(5_000));
    assert_eq!(nc_stats.tokens_out_total, Some(200));
    assert_eq!(nc_stats.cached_tokens_total, None);
    assert_eq!(nc_stats.reasoning_tokens_total, None);
    assert_eq!(nc_stats.token_cache_hit_rate, None);
    assert_eq!(nc_stats.tokens_by_kind["worker"].cache_hit_rate, None);
}

#[tokio::test]
async fn replay_excludes_deterministic_merge_from_conflict_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("merge-replay.db");
    let db = Db::open(&db_path).unwrap();
    let t = T::over(db, |_| {});
    let tok = create_device(&t, "mac-pro").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "alpha", &[REPO], 2).await;
    heartbeat_device(&t, &tok, "mac-pro", &["git:cloneable"], &[REPO], 8).await;

    // Seed 5 simultaneous deterministic `merge` runs on the same repo (sharing `repo:<REPO>:merge` max=1).
    let base_ms = t.state.now().timestamp_millis();
    for i in 0..5 {
        let tid = t.task(WORKER, "alpha", &format!("Merge task {i}")).await;
        let run_id = format!("run-merge-storm-{i}");
        t.exec(&format!(
            "INSERT INTO runs ( \
                 id, task_id, board, kind, origin, device, runner, state, cost_units, \
                 policy_version, created_at, started_at, ended_at, updated_at \
             ) VALUES ( \
                 '{run_id}', '{tid}', 'alpha', 'merge', 'managed', 'mac-pro', 'deterministic', \
                 'succeeded', 1, 'v1-default', {base_ms}, {base_ms}, {}, {} \
             );",
            base_ms + 30_000,
            base_ms + 30_000
        ));
    }

    let cmp = replay_schedule(
        &db_path,
        ReplayOptions {
            since: None,
            until: t.state.now() + TimeDelta::minutes(10),
            days: 1,
            candidate: None,
        },
    )
    .unwrap();
    assert_eq!(cmp.baseline.tasks_scheduled, 5);
    assert_eq!(
        cmp.baseline.conflict_blocks, 0,
        "deterministic merge contention must not inflate LLM conflict_blocks: {:?}",
        cmp.baseline
    );

    // Adding simultaneous LLM `worker` runs exceeding `parallel_tasks` / `repo:<REPO>:write`
    // (on a board with parallel_tasks=1 and 2 tasks sharing an explicit conflict key) STILL counts.
    for i in 0..2 {
        let tid = t
            .task_with(
                WORKER,
                json!({
                    "board": "alpha",
                    "title": format!("Shared lock worker {i}"),
                    "conflict_keys": [{ "key": "db:migrate", "max": 1 }]
                }),
            )
            .await;
        let run_id = format!("run-worker-lock-{i}");
        t.exec(&format!(
            "INSERT INTO runs ( \
                 id, task_id, board, kind, origin, device, runner, state, cost_units, \
                 policy_version, created_at, started_at, ended_at, updated_at \
             ) VALUES ( \
                 '{run_id}', '{tid}', 'alpha', 'worker', 'managed', 'mac-pro', 'acp', \
                 'succeeded', 2, 'v1-default', {base_ms}, {base_ms}, {}, {} \
             );",
            base_ms + 60_000,
            base_ms + 60_000
        ));
    }
    let cmp2 = replay_schedule(
        &db_path,
        ReplayOptions {
            since: None,
            until: t.state.now() + TimeDelta::minutes(10),
            days: 1,
            candidate: None,
        },
    )
    .unwrap();
    assert!(
        cmp2.baseline.conflict_blocks > 0,
        "LLM conflict_key blocks must still be counted: {:?}",
        cmp2.baseline
    );
}

#[tokio::test]
async fn starvation_respects_quota_holds_and_ignores_deterministic_merge_housekeeping() {
    let t = T::with(|cfg| {
        cfg.defaults.starvation_after_secs = 3600;
    });
    let dev_tok = create_device(&t, "mac-starv").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "quiet", &[REPO], 2).await;
    autopilot_board(&t, "busy", &[REPO], 2).await;
    heartbeat_device(&t, &dev_tok, "mac-starv", &["git:cloneable"], &[REPO], 4).await;

    let _q_task = t.task(WORKER, "quiet", "Waiting task").await;
    let b_task = t.task(WORKER, "busy", "Busy task").await;
    t.advance(TimeDelta::hours(2));
    heartbeat_device(&t, &dev_tok, "mac-starv", &["git:cloneable"], &[REPO], 4).await;
    let now_ms = t.state.now().timestamp_millis();

    // 1. Only deterministic `merge` and `housekeeping` runs on `busy` -> must NOT trigger
    //    starvation on `quiet`!
    t.exec(&format!(
        "INSERT INTO runs (id, task_id, board, kind, origin, device, runner, state, cost_units, \
         created_at, updated_at) VALUES \
         ('run-m1', '{b_task}', 'busy', 'merge', 'managed', 'mac-starv', 'deterministic', 'succeeded', 1, {now_ms}, {now_ms}), \
         ('run-h1', '{b_task}', 'busy', 'housekeeping', 'managed', 'mac-starv', 'deterministic', 'succeeded', 1, {now_ms}, {now_ms});"
    ));
    let s1 = t.sweep_pathologies();
    assert!(
        s1.filed.is_empty(),
        "merge/housekeeping runs must not trigger starvation: {s1:?}"
    );

    // 2. Add a real LLM worker run on `busy`, BUT report fresh usage over `usage_stop_at` (95% >= 90%) ->
    //    quota holds `quiet`, so starvation MUST NOT fire!
    t.exec(&format!(
        "INSERT INTO runs (id, task_id, board, kind, origin, device, runner, state, cost_units, \
         created_at, updated_at) VALUES \
         ('run-w1', '{b_task}', 'busy', 'worker', 'managed', 'mac-starv', 'acp', 'succeeded', 2, {now_ms}, {now_ms});"
    ));
    t.ok(
        "PUT",
        "/v1/usage/anthropic/default",
        None,
        Some(json!({
            "five_hour": { "percent": 95.0, "resets_at": super::iso(t.state.now() + TimeDelta::hours(2)) },
            "read_at": super::iso(t.state.now()),
        })),
    )
    .await;
    let s2 = t.sweep_pathologies();
    assert!(
        s2.filed.is_empty(),
        "quota-held board must not trigger starvation: {s2:?}"
    );

    // 3. Lower usage below `usage_stop_at` (20%) -> starvation DOES fire now!
    t.advance(TimeDelta::minutes(1));
    t.ok(
        "PUT",
        "/v1/usage/anthropic/default",
        None,
        Some(json!({
            "five_hour": { "percent": 20.0, "resets_at": super::iso(t.state.now() + TimeDelta::hours(2)) },
            "read_at": super::iso(t.state.now()),
        })),
    )
    .await;
    let s3 = t.sweep_pathologies();
    assert_eq!(
        s3.filed.len(),
        1,
        "unheld starved board should fire once: {s3:?}"
    );
}

#[tokio::test]
async fn replay_excludes_deterministic_polls_and_pre_start_cancelled_runs() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("replay-filter.db");
    let db = Db::open(&db_path).unwrap();
    let t = T::over(db, |_| {});
    let dev_tok = create_device(&t, "mac-rf").await;
    t.register(WORKER, Some(REPO), None).await;
    autopilot_board(&t, "rf", &[REPO], 2).await;
    heartbeat_device(&t, &dev_tok, "mac-rf", &["git:cloneable"], &[REPO], 4).await;

    let base_ms = t.state.now().timestamp_millis();
    // Seed 1 real worker run, 1 merge poll, 1 housekeeping poll, and 1 pre-start cancelled review
    // (task_id = NULL so unrun-task fallback doesn't synthesize extra items).
    t.exec(&format!(
        "INSERT INTO runs (id, task_id, board, kind, origin, device, runner, state, cost_units, \
         created_at, started_at, ended_at, updated_at) VALUES \
         ('run-real', NULL, 'rf', 'worker', 'managed', 'mac-rf', 'acp', 'succeeded', 2, {base_ms}, {base_ms}, {}, {}), \
         ('run-merge', NULL, 'rf', 'merge', 'managed', 'mac-rf', 'deterministic', 'succeeded', 1, {base_ms}, {base_ms}, {}, {}), \
         ('run-hk', NULL, 'rf', 'housekeeping', 'managed', 'mac-rf', 'deterministic', 'succeeded', 1, {base_ms}, {base_ms}, {}, {}), \
         ('run-cancel', NULL, 'rf', 'review', 'managed', 'mac-rf', 'acp', 'cancelled', 2, {base_ms}, NULL, {}, {});",
        base_ms + 60_000,
        base_ms + 60_000,
        base_ms + 1_000,
        base_ms + 1_000,
        base_ms + 1_000,
        base_ms + 1_000,
        base_ms + 500,
        base_ms + 500,
    ));

    let cmp = replay_schedule(
        &db_path,
        ReplayOptions {
            since: None,
            until: t.state.now() + TimeDelta::minutes(10),
            days: 1,
            candidate: None,
        },
    )
    .unwrap();
    assert_eq!(
        cmp.historical_runs, 1,
        "merge, housekeeping, and pre-start cancelled runs must be excluded: {cmp:?}"
    );
    assert_eq!(cmp.baseline.tasks_scheduled, 1);
}
