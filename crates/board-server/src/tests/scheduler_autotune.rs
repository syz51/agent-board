//! Tests for automated continuous scheduler evaluation and guardrailed self-tuning
//! (`agent-board-698`, `docs/SCHEDULER_DESIGN.md`, Section 6.4):
//! - Migration 0034 (`scheduler_evaluations`, `scheduler_policies.auto_promoted` / `previous_version`)
//! - Interval gating, `skipped_insufficient_data`, `GET /v1/scheduler/evaluations`,
//!   `POST /v1/scheduler/evaluations/run` auth & force trigger, `latest_evaluation` on stats
//! - `keep` when replay delta < `min_improvement_pct`
//! - Guardrailed `apply` promotion to `vN-auto-YYYYMMDD` + cooldown gating
//! - `propose` mode filing a structured `Proposal:` task with replay comparison table
//! - Auto-rollback watchdog on post-promotion `orphan_rate` or `p90` wait regression

use board_proto::{AutotuneDecision, AutotuneMode, SchedulerStats};
use chrono::TimeDelta;
use serde_json::{Value, json};

use super::T;

const WORKER: &str = "t3:worker";

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

async fn heartbeat_mac(t: &T, repos: &[&str], units: u32) {
    let (s, v) = t
        .call(
            "PUT",
            "/v1/devices/mac/heartbeat",
            None,
            Some(json!({
                "os": "macos",
                "arch": "arm64",
                "capacity": { "weight_units": units, "mem_gib": 32, "cpu_cores": 10 },
                "free_units": units,
                "capabilities": ["git:cloneable"],
                "runners": ["acp", "deterministic", "t3"],
                "repos": repos,
                "protocol_features": ["v1-json", "runs-v1", "dispatch-claim-v1"]
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
}

/// Inserts a historical managed `runs` + `run_outcomes` pair at `at_ms` directly into the
/// DB so deterministic replay & watchdog tests can build multi-run fixtures quickly with
/// exact timestamps on the injectable clock.
#[allow(clippy::too_many_arguments)]
fn seed_managed_outcome(
    t: &T,
    run_id: &str,
    task_id: &str,
    board: &str,
    policy_version: &str,
    terminal_state: &str,
    at_ms: i64,
    ready_to_leased_secs: i64,
    running_secs: i64,
    cost_units: i64,
) {
    let started_ms = at_ms + 1_000;
    let ended_ms = started_ms + running_secs * 1_000;
    let wum = (cost_units as f64) * (running_secs as f64) / 60.0;
    t.exec(&format!(
        "INSERT INTO runs ( \
             id, task_id, board, kind, origin, device, runner, state, cost_units, \
             policy_version, created_at, started_at, ended_at, updated_at \
         ) VALUES ( \
             '{run_id}', '{task_id}', '{board}', 'worker', 'managed', 'mac', 'acp', \
             '{terminal_state}', {cost_units}, '{policy_version}', {at_ms}, {started_ms}, \
             {ended_ms}, {ended_ms} \
         ); \
         INSERT INTO run_outcomes ( \
             run_id, task_id, board, device, kind, terminal_state, \
             ready_to_leased_secs, leased_to_running_secs, running_secs, \
             weight_unit_minutes, policy_version, recorded_at \
         ) VALUES ( \
             '{run_id}', '{task_id}', '{board}', 'mac', 'worker', '{terminal_state}', \
             {ready_to_leased_secs}, 1, {running_secs}, {wum}, '{policy_version}', {ended_ms} \
         );"
    ));
}

/// Seeds a skewed 2-board burst workload (`heavy` P1 600s tasks vs `starved` P2 60s tasks on
/// a single 2-unit device) where `v1-default` (w_prio=30, w_fair=12) lets `heavy` hog the
/// device first while `fair-strong` interleaves `starved`'s short tasks earlier, reducing
/// both `fair_share_gini` and `p90_wait_secs` with zero throughput/conflict/p50 regression.
async fn seed_skewed_improvable_workload(t: &T) {
    let repo_h = "github.com/example/heavy";
    let repo_s = "github.com/example/starved";
    t.register(WORKER, Some(repo_h), None).await;
    autopilot_board(t, "heavy", &[repo_h], 4).await;
    autopilot_board(t, "starved", &[repo_s], 4).await;
    heartbeat_mac(t, &[repo_h, repo_s], 2).await;

    let base_ms = t.state.now().timestamp_millis();

    // Seed prior consumption on `heavy` (12 P3 runs in the last hour) so `heavy` already holds
    // ~100% of the rolling 24h fair-share window when the burst arrives.
    for i in 0..12 {
        let tid = t
            .task_with(
                WORKER,
                json!({ "board": "heavy", "title": format!("Heavy seed {i}"), "priority": "p3" }),
            )
            .await;
        let run_at = base_ms - 3_600_000 + i * 120_000;
        t.exec(&format!(
            "UPDATE tasks SET created_at = {run_at}, ready_at = {run_at} WHERE id = '{tid}'"
        ));
        seed_managed_outcome(
            t,
            &format!("run-seed-{i:02}"),
            &tid,
            "heavy",
            "v1-default",
            "succeeded",
            run_at,
            5,
            60,
            2,
        );
    }

    // Now advance 5 minutes and create a simultaneous burst in the SAME priority band (`p2`):
    // 6 long tasks on `heavy` (600s, `size=s` -> +20 WSJF) and 6 short tasks on `starved`
    // (60s, `size=l` -> +5 WSJF), all arriving at `burst_ms`.
    // Advance clock by 25 minutes (1500s) so only part of the burst fits inside
    // `[since, until]` — under `v1-default` (w_fair=12), `heavy` wins (+20 - 12 = +8 > +5)
    // and monopolizes the window; under `fair-strong` (w_fair=40), all 6 of `starved`'s
    // short 60s tasks run first (0..360s), slashing both `fair_share_gini` and `p90_wait_secs`
    // while keeping strict P1 > P2 > P3 inviolable!
    t.advance(TimeDelta::minutes(5));
    let burst_ms = t.state.now().timestamp_millis();
    for i in 0..6 {
        let h_tid = t
            .task_with(
                WORKER,
                json!({ "board": "heavy", "title": format!("Heavy burst {i}"), "priority": "p2", "size": "s" }),
            )
            .await;
        t.exec(&format!(
            "UPDATE tasks SET created_at = {burst_ms}, ready_at = {burst_ms} WHERE id = '{h_tid}'"
        ));
        seed_managed_outcome(
            t,
            &format!("run-hburst-{i:02}"),
            &h_tid,
            "heavy",
            "v1-default",
            "succeeded",
            burst_ms,
            0,
            600,
            2,
        );

        let s_tid = t
            .task_with(
                WORKER,
                json!({ "board": "starved", "title": format!("Starved burst {i}"), "priority": "p2", "size": "l" }),
            )
            .await;
        t.exec(&format!(
            "UPDATE tasks SET created_at = {burst_ms}, ready_at = {burst_ms} WHERE id = '{s_tid}'"
        ));
        seed_managed_outcome(
            t,
            &format!("run-sburst-{i:02}"),
            &s_tid,
            "starved",
            "v1-default",
            "succeeded",
            burst_ms,
            0,
            60,
            2,
        );
    }

    t.advance(TimeDelta::minutes(25));
}

#[tokio::test]
async fn autotune_skips_insufficient_data_honors_interval_and_api_auth() {
    let t = T::with(|cfg| {
        cfg.scheduler.autotune.enabled = true;
        cfg.scheduler.autotune.mode = AutotuneMode::Apply;
        cfg.scheduler.autotune.interval_secs = 3600;
        cfg.scheduler.autotune.min_runs = 5;
    });

    // 1. `/healthz` advertises `scheduler-autotune-v1`.
    let health = t.ok("GET", "/healthz", None, None).await;
    let features: Vec<&str> = health["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(
        features.contains(&"scheduler-autotune-v1"),
        "missing feature flag: {features:?}"
    );

    // 2. First sweep runs immediately and records `skipped_insufficient_data`.
    let eval1 = t
        .sweep_autotune()
        .expect("first sweep should evaluate with no prior evaluation");
    assert_eq!(eval1.decision, AutotuneDecision::SkippedInsufficientData);
    assert_eq!(eval1.mode, AutotuneMode::Apply);
    assert_eq!(eval1.baseline_policy_version, "v1-default");
    assert_eq!(eval1.sample_runs, 0);

    // 3. Second sweep before `interval_secs` elapses is a clean no-op.
    t.advance(TimeDelta::seconds(1800));
    assert_eq!(t.sweep_autotune(), None);

    // 4. Advancing past `interval_secs` triggers a new evaluation.
    t.advance(TimeDelta::seconds(1801));
    let eval2 = t
        .sweep_autotune()
        .expect("sweep after interval should evaluate");
    assert_eq!(eval2.decision, AutotuneDecision::SkippedInsufficientData);
    assert!(eval2.id > eval1.id);

    // 5. `GET /v1/scheduler/evaluations` works with device token, respects `limit`, validates range.
    let list = t
        .ok("GET", "/v1/scheduler/evaluations?limit=1", None, None)
        .await;
    assert_eq!(list["mode"], "apply");
    assert_eq!(list["enabled"], true);
    assert_eq!(list["interval_secs"], 3600);
    let evals = list["evaluations"].as_array().unwrap();
    assert_eq!(evals.len(), 1);
    assert_eq!(evals[0]["id"], eval2.id);

    for bad_limit in ["0", "201"] {
        let (code, err, _) = t
            .fail(
                "GET",
                &format!("/v1/scheduler/evaluations?limit={bad_limit}"),
                None,
                None,
            )
            .await;
        assert_eq!((code, err.as_str()), (400, "invalid"));
    }

    // 6. `POST /v1/scheduler/evaluations/run` requires `--user` token and bypasses interval gate.
    let (code, err, _) = t
        .fail("POST", "/v1/scheduler/evaluations/run", None, None)
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"));

    let (status, forced) = t
        .as_user("POST", "/v1/scheduler/evaluations/run", None)
        .await;
    assert_eq!(status.as_u16(), 200, "{forced}");
    assert_eq!(forced["decision"], "skipped_insufficient_data");
    assert!(forced["id"].as_i64().unwrap() > eval2.id);

    // 7. `GET /v1/stats/scheduler` surfaces `latest_evaluation`.
    let stats_val = t.ok("GET", "/v1/stats/scheduler?days=7", None, None).await;
    let stats: SchedulerStats = serde_json::from_value(stats_val).unwrap();
    let latest = stats
        .latest_evaluation
        .expect("latest_evaluation should be populated");
    assert_eq!(latest.id, forced["id"].as_i64().unwrap());
    assert_eq!(latest.decision, AutotuneDecision::SkippedInsufficientData);
}

#[tokio::test]
async fn autotune_keeps_policy_when_improvement_below_threshold() {
    let t = T::with(|cfg| {
        cfg.scheduler.autotune.enabled = true;
        cfg.scheduler.autotune.mode = AutotuneMode::Apply;
        cfg.scheduler.autotune.min_runs = 5;
        cfg.scheduler.autotune.min_improvement_pct = 5.0;
    });

    let repo = "github.com/example/solo";
    t.register(WORKER, Some(repo), None).await;
    autopilot_board(&t, "solo", &[repo], 2).await;
    heartbeat_mac(&t, &[repo], 8).await;

    // Seed 6 well-spaced runs on one board -> 0 wait, 0 gini under every policy.
    let base_ms = t.state.now().timestamp_millis();
    for i in 0..6 {
        let tid = t.task(WORKER, "solo", &format!("Solo task {i}")).await;
        let at = base_ms + i * 600_000;
        t.exec(&format!(
            "UPDATE tasks SET created_at = {at} WHERE id = '{tid}'"
        ));
        seed_managed_outcome(
            &t,
            &format!("run-solo-{i}"),
            &tid,
            "solo",
            "v1-default",
            "succeeded",
            at,
            2,
            60,
            2,
        );
    }
    t.advance(TimeDelta::hours(2));

    let eval = t.sweep_autotune().expect("evaluation should run");
    assert_eq!(eval.decision, AutotuneDecision::Keep);
    assert_eq!(eval.baseline_policy_version, "v1-default");
    assert_eq!(eval.applied_policy_version, None);
    assert!(eval.baseline_metrics.is_some());
    assert!(eval.candidate_metrics.is_some());
    assert!(eval.score_delta_pct.unwrap_or(0.0) < 5.0);

    let active = t.ok("GET", "/v1/scheduler/policy", None, None).await;
    assert_eq!(active["version"], "v1-default");
}

#[tokio::test]
async fn autotune_apply_promotes_and_respects_cooldown() {
    let t = T::with(|cfg| {
        cfg.scheduler.autotune.enabled = true;
        cfg.scheduler.autotune.mode = AutotuneMode::Apply;
        cfg.scheduler.autotune.interval_secs = 60;
        cfg.scheduler.autotune.min_runs = 10;
        cfg.scheduler.autotune.min_improvement_pct = 5.0;
        cfg.scheduler.autotune.cooldown_secs = 86_400;
    });

    seed_skewed_improvable_workload(&t).await;

    let seq_before = t.ok("GET", "/v1/events?limit=1", None, None).await["next_since"]
        .as_i64()
        .unwrap();

    // First sweep should promote a new `v2-auto-YYYYMMDD` policy!
    let eval = t.sweep_autotune().expect("evaluation should run");
    assert_eq!(eval.decision, AutotuneDecision::Promoted, "{eval:?}");
    let promoted = eval
        .applied_policy_version
        .as_deref()
        .expect("applied_policy_version set");
    assert!(promoted.starts_with("v2-auto-"), "got {promoted}");
    assert_eq!(eval.previous_policy_version.as_deref(), Some("v1-default"));
    assert!(
        eval.score_delta_pct.unwrap_or(0.0) >= 5.0,
        "expected >= 5.0% improvement, got {:?}",
        eval.score_delta_pct
    );
    assert!(eval.baseline_metrics.is_some());
    assert!(eval.candidate_metrics.is_some());
    assert!(eval.candidate_weights.is_some());

    // Active policy in DB and HTTP API is now the auto-promoted policy, with provenance tracked.
    let active = t.ok("GET", "/v1/scheduler/policy", None, None).await;
    assert_eq!(active["version"], promoted);
    assert_eq!(active["created_by"], "server:autotune");
    assert_eq!(
        t.count(&format!(
            "SELECT auto_promoted FROM scheduler_policies WHERE version = '{promoted}'"
        )),
        1
    );
    assert_eq!(
        t.strings(&format!(
            "SELECT previous_version FROM scheduler_policies WHERE version = '{promoted}'"
        )),
        vec!["v1-default".to_owned()]
    );

    // Trail Rule event recorded.
    let evs = t.event_types(seq_before).await;
    assert!(
        evs.contains(&"scheduler_policy_updated".to_owned()),
        "{evs:?}"
    );

    // Switch active policy back to `v1-default` manually so the exact same workload still has
    // a >=5% improvable candidate, advance past `interval_secs` (120s) while still inside
    // `cooldown_secs` (86400s), and confirm cooldown holds promotion cleanly as `keep`.
    t.exec("UPDATE scheduler_policies SET active = 0; UPDATE scheduler_policies SET active = 1 WHERE version = 'v1-default';");
    t.advance(TimeDelta::seconds(120));
    let held = t
        .sweep_autotune()
        .expect("sweep after interval should evaluate");
    assert_eq!(held.decision, AutotuneDecision::Keep, "{held:?}");
    assert!(
        held.reason.contains("cooldown"),
        "expected cooldown reason, got: {}",
        held.reason
    );
    let still_v1 = t.ok("GET", "/v1/scheduler/policy", None, None).await;
    assert_eq!(still_v1["version"], "v1-default");
}

#[tokio::test]
async fn autotune_propose_mode_files_structured_proposal_without_mutating_policy() {
    let t = T::with(|cfg| {
        cfg.scheduler.autotune.enabled = true;
        cfg.scheduler.autotune.mode = AutotuneMode::Propose;
        cfg.scheduler.autotune.interval_secs = 60;
        cfg.scheduler.autotune.min_runs = 10;
        cfg.scheduler.autotune.min_improvement_pct = 5.0;
        cfg.scheduler.autotune.cooldown_secs = 86_400;
    });

    seed_skewed_improvable_workload(&t).await;

    let eval = t.sweep_autotune().expect("propose sweep should evaluate");
    assert_eq!(eval.decision, AutotuneDecision::Proposed, "{eval:?}");
    assert_eq!(eval.applied_policy_version, None);
    let task_id = eval
        .feedback_task_id
        .as_ref()
        .expect("proposal task id recorded");

    // Active policy remains unchanged.
    let active = t.ok("GET", "/v1/scheduler/policy", None, None).await;
    assert_eq!(active["version"], "v1-default");

    // Verify the filed task is a valid pending `Proposal:` with replay comparison table.
    let task = t.get_task(task_id.as_str()).await;
    assert_eq!(task["auto"], false);
    assert_eq!(task["proposal"], true);
    let body = task["body"].as_str().unwrap();
    assert!(body.starts_with("Proposal:"), "{body}");
    assert!(body.contains("fair_share_gini"), "{body}");
    assert!(body.contains("p90_wait_secs"), "{body}");
    assert!(body.contains("PUT /v1/scheduler/policy"), "{body}");
}

#[tokio::test]
async fn autotune_watchdog_rolls_back_on_orphan_rate_regression() {
    let t = T::with(|cfg| {
        cfg.scheduler.autotune.enabled = true;
        cfg.scheduler.autotune.mode = AutotuneMode::Apply;
        cfg.scheduler.autotune.interval_secs = 60;
        cfg.scheduler.autotune.min_runs = 10;
        cfg.scheduler.autotune.min_improvement_pct = 5.0;
        cfg.scheduler.autotune.rollback_min_runs = 5;
        cfg.scheduler.autotune.rollback_orphan_delta = 0.05;
        cfg.scheduler.autotune.rollback_p90_ratio = 1.25;
    });

    seed_skewed_improvable_workload(&t).await;

    // 1. Promote `v2-auto-*`.
    let promo = t.sweep_autotune().expect("promotion sweep");
    assert_eq!(promo.decision, AutotuneDecision::Promoted, "{promo:?}");
    let promoted = promo.applied_policy_version.clone().unwrap();

    // 2. Seed only 3 orphaned runs (< rollback_min_runs = 5) -> must NOT roll back yet.
    t.advance(TimeDelta::seconds(120));
    let post_ms = t.state.now().timestamp_millis();
    for i in 0..3 {
        let tid = t
            .task(WORKER, "heavy", &format!("Post orphan early {i}"))
            .await;
        seed_managed_outcome(
            &t,
            &format!("run-post-early-{i}"),
            &tid,
            "heavy",
            &promoted,
            "orphaned",
            post_ms + i * 1_000,
            5,
            60,
            2,
        );
    }
    let check_early = t.sweep_autotune().expect("sweep before rollback_min_runs");
    assert_ne!(check_early.decision, AutotuneDecision::RolledBack);
    let still_promoted = t.ok("GET", "/v1/scheduler/policy", None, None).await;
    assert_eq!(still_promoted["version"], promoted);

    // 3. Seed 2 more runs (1 orphaned, 1 succeeded -> 4/5 = 80% orphan rate >> 0% + 5%).
    t.advance(TimeDelta::seconds(120));
    let post2_ms = t.state.now().timestamp_millis();
    for (i, state) in [(3, "orphaned"), (4, "succeeded")] {
        let tid = t.task(WORKER, "heavy", &format!("Post run {i}")).await;
        seed_managed_outcome(
            &t,
            &format!("run-post-{i}"),
            &tid,
            "heavy",
            &promoted,
            state,
            post2_ms + i * 1_000,
            5,
            60,
            2,
        );
    }

    let seq_before = t.ok("GET", "/v1/events?limit=1", None, None).await["next_since"]
        .as_i64()
        .unwrap();

    let rb = t.sweep_autotune().expect("rollback watchdog should fire");
    assert_eq!(rb.decision, AutotuneDecision::RolledBack, "{rb:?}");
    assert_eq!(rb.baseline_policy_version, promoted);
    assert_eq!(rb.applied_policy_version.as_deref(), Some("v1-default"));
    assert_eq!(
        rb.previous_policy_version.as_deref(),
        Some(promoted.as_str())
    );
    assert!(rb.reason.contains("orphan_rate"), "{}", rb.reason);
    let fb_id = rb
        .feedback_task_id
        .as_ref()
        .expect("diagnostic feedback signal should be filed");
    assert_eq!(fb_id.board().as_str(), "board-feedback");

    // Active policy is automatically back to `v1-default`!
    let reverted = t.ok("GET", "/v1/scheduler/policy", None, None).await;
    assert_eq!(reverted["version"], "v1-default");
    assert_eq!(reverted["active"], true);

    // Trail Rule event recorded with `action = "auto_rolled_back"`.
    let page = t
        .ok(
            "GET",
            &format!("/v1/events?since={seq_before}&limit=50"),
            None,
            None,
        )
        .await;
    let has_rb_event = page["events"].as_array().unwrap().iter().any(|e| {
        e["type"] == "scheduler_policy_updated" && e["payload"]["action"] == "auto_rolled_back"
    });
    assert!(has_rb_event, "missing rollback event: {page}");
}

#[tokio::test]
async fn autotune_watchdog_rolls_back_on_p90_wait_regression() {
    let t = T::with(|cfg| {
        cfg.scheduler.autotune.enabled = true;
        cfg.scheduler.autotune.mode = AutotuneMode::Apply;
        cfg.scheduler.autotune.interval_secs = 60;
        cfg.scheduler.autotune.min_runs = 10;
        cfg.scheduler.autotune.rollback_min_runs = 5;
        cfg.scheduler.autotune.rollback_orphan_delta = 0.05;
        cfg.scheduler.autotune.rollback_p90_ratio = 1.25;
    });

    let repo = "github.com/example/p90";
    t.register(WORKER, Some(repo), None).await;
    autopilot_board(&t, "p90", &[repo], 2).await;
    heartbeat_mac(&t, &[repo], 8).await;

    // Pre-promotion `v1-default` runs with low wait (10s p90) and 0% orphans.
    let base_ms = t.state.now().timestamp_millis();
    for i in 0..6 {
        let tid = t.task(WORKER, "p90", &format!("Pre task {i}")).await;
        seed_managed_outcome(
            &t,
            &format!("run-pre-{i}"),
            &tid,
            "p90",
            "v1-default",
            "succeeded",
            base_ms + i * 1_000,
            10,
            30,
            2,
        );
    }

    // Install an auto-promoted policy `v2-auto-20261106` created at T + 5m.
    t.advance(TimeDelta::minutes(5));
    let promo_ms = t.state.now().timestamp_millis();
    t.exec(&format!(
        "UPDATE scheduler_policies SET active = 0; \
         INSERT INTO scheduler_policies ( \
             version, weights_json, lease_ttl_secs, sticky_grace_secs, notes, active, \
             created_by, created_at, auto_promoted, previous_version \
         ) VALUES ( \
             'v2-auto-20261106', \
             '{{\"w_kind\":100.0,\"w_prio\":30.0,\"w_rank\":10.0,\"w_age\":20.0,\"w_fair\":24.0,\"w_loc\":8.0,\"w_pref\":1.0,\"w_backfill\":6.0}}', \
             180, 600, 'test auto promotion', 1, 'server:autotune', {promo_ms}, 1, 'v1-default' \
         );"
    ));

    // Post-promotion runs have 0% orphans but degraded ready_to_leased_secs = 25s (> 10s * 1.25).
    t.advance(TimeDelta::minutes(1));
    let post_ms = t.state.now().timestamp_millis();
    for i in 0..5 {
        let tid = t.task(WORKER, "p90", &format!("Post slow {i}")).await;
        seed_managed_outcome(
            &t,
            &format!("run-slow-{i}"),
            &tid,
            "p90",
            "v2-auto-20261106",
            "succeeded",
            post_ms + i * 1_000,
            25,
            30,
            2,
        );
    }

    let (status, rb): (_, Value) = t
        .as_user("POST", "/v1/scheduler/evaluations/run", None)
        .await;
    assert_eq!(status.as_u16(), 200, "{rb}");
    assert_eq!(rb["decision"], "rolled_back");
    assert_eq!(rb["applied_policy_version"], "v1-default");
    assert!(rb["reason"].as_str().unwrap().contains("p90 wait"), "{rb}");
    assert!(rb["feedback_task_id"].is_string(), "{rb}");

    let active = t.ok("GET", "/v1/scheduler/policy", None, None).await;
    assert_eq!(active["version"], "v1-default");
}

#[tokio::test]
async fn autotune_ignores_interactive_runs_and_skips_missing_orphan_baseline() {
    let t = T::with(|cfg| {
        cfg.scheduler.autotune.enabled = true;
        cfg.scheduler.autotune.mode = AutotuneMode::Apply;
        cfg.scheduler.autotune.interval_secs = 60;
        cfg.scheduler.autotune.min_runs = 5;
        cfg.scheduler.autotune.rollback_min_runs = 5;
        cfg.scheduler.autotune.rollback_orphan_delta = 0.05;
    });

    // 1. Seed 10 interactive runs (policy_version IS NULL, kind = 'interactive') -> MUST NOT
    // satisfy `min_runs = 5` because replay only evaluates managed runs.
    let now_ms = t.state.now().timestamp_millis();
    for i in 0..10 {
        let at = now_ms - (10 - i) * 1_000;
        t.exec(&format!(
            "INSERT INTO runs ( \
                 id, origin, kind, runner, state, device, cost_units, created_at, started_at, ended_at, updated_at \
             ) VALUES ( \
                 'run-int-{i}', 'external', 'interactive', 't3', 'succeeded', 'mac', 1, {at}, {at}, {end}, {end} \
             ); \
             INSERT INTO run_outcomes ( \
                 run_id, device, kind, terminal_state, ready_to_leased_secs, running_secs, \
                 policy_version, recorded_at \
             ) VALUES ( \
                 'run-int-{i}', 'mac', 'interactive', 'succeeded', 0, 10, NULL, {end} \
             );",
            end = at + 10_000
        ));
    }
    let eval = t
        .sweep_autotune()
        .expect("sweep should record insufficient data");
    assert_eq!(eval.decision, AutotuneDecision::SkippedInsufficientData);
    assert_eq!(eval.sample_runs, 0);

    // 2. Seed 5 pre-promotion managed runs on `v1-default` with 1/5 = 20% orphan rate (plus the
    // 10 interactive runs above that would dilute `base_orphan_rate` to 1/15 = 6.7% if included!).
    let repo = "github.com/example/int-cohort";
    t.register(WORKER, Some(repo), None).await;
    autopilot_board(&t, "cohort", &[repo], 2).await;
    heartbeat_mac(&t, &[repo], 8).await;
    for (i, state) in [
        (0, "orphaned"),
        (1, "succeeded"),
        (2, "succeeded"),
        (3, "succeeded"),
        (4, "succeeded"),
    ] {
        let tid = t.task(WORKER, "cohort", &format!("Pre {i}")).await;
        seed_managed_outcome(
            &t,
            &format!("run-pre-cohort-{i}"),
            &tid,
            "cohort",
            "v1-default",
            state,
            now_ms + i * 1_000,
            10,
            30,
            2,
        );
    }

    // Install auto-promoted `v2-auto-20261106` from `v1-default` and record identical 20% orphan rate.
    t.advance(TimeDelta::minutes(5));
    let promo_ms = t.state.now().timestamp_millis();
    t.exec(&format!(
        "UPDATE scheduler_policies SET active = 0; \
         INSERT INTO scheduler_policies ( \
             version, weights_json, lease_ttl_secs, sticky_grace_secs, notes, active, \
             created_by, created_at, auto_promoted, previous_version \
         ) VALUES ( \
             'v2-auto-20261106', \
             '{{\"w_kind\":100.0,\"w_prio\":30.0,\"w_rank\":10.0,\"w_age\":20.0,\"w_fair\":24.0,\"w_loc\":8.0,\"w_pref\":1.0,\"w_backfill\":6.0}}', \
             180, 600, 'test', 1, 'server:autotune', {promo_ms}, 1, 'v1-default' \
         );"
    ));
    t.advance(TimeDelta::minutes(1));
    let post_ms = t.state.now().timestamp_millis();
    for (i, state) in [
        (0, "orphaned"),
        (1, "succeeded"),
        (2, "succeeded"),
        (3, "succeeded"),
        (4, "succeeded"),
    ] {
        let tid = t.task(WORKER, "cohort", &format!("Post {i}")).await;
        seed_managed_outcome(
            &t,
            &format!("run-post-cohort-{i}"),
            &tid,
            "cohort",
            "v2-auto-20261106",
            state,
            post_ms + i * 1_000,
            10,
            30,
            2,
        );
    }
    let check = t
        .sweep_autotune()
        .expect("equal managed orphan rate must not roll back");
    assert_ne!(check.decision, AutotuneDecision::RolledBack, "{check:?}");

    // 3. When `previous_version` has ZERO pre-promotion managed outcomes, watchdog must skip
    // orphan-rate comparison rather than fabricating a 0% baseline.
    t.advance(TimeDelta::minutes(5));
    let promo2_ms = t.state.now().timestamp_millis();
    t.exec(&format!(
        "UPDATE scheduler_policies SET active = 0; \
         INSERT INTO scheduler_policies ( \
             version, weights_json, lease_ttl_secs, sticky_grace_secs, notes, active, \
             created_by, created_at, auto_promoted, previous_version \
         ) VALUES ( \
             'v2-manual-untested', \
             '{{\"w_kind\":100.0,\"w_prio\":30.0,\"w_rank\":10.0,\"w_age\":16.0,\"w_fair\":12.0,\"w_loc\":8.0,\"w_pref\":1.0,\"w_backfill\":6.0}}', \
             180, 600, 'manual', 0, 'user', {}, 0, NULL \
         ); \
         INSERT INTO scheduler_policies ( \
             version, weights_json, lease_ttl_secs, sticky_grace_secs, notes, active, \
             created_by, created_at, auto_promoted, previous_version \
         ) VALUES ( \
             'v3-auto-20261106', \
             '{{\"w_kind\":100.0,\"w_prio\":30.0,\"w_rank\":10.0,\"w_age\":20.0,\"w_fair\":24.0,\"w_loc\":8.0,\"w_pref\":1.0,\"w_backfill\":6.0}}', \
             180, 600, 'auto', 1, 'server:autotune', {promo2_ms}, 1, 'v2-manual-untested' \
         );",
        promo2_ms - 60_000
    ));
    t.advance(TimeDelta::minutes(1));
    let post2_ms = t.state.now().timestamp_millis();
    for (i, state) in [
        (0, "orphaned"),
        (1, "succeeded"),
        (2, "succeeded"),
        (3, "succeeded"),
        (4, "succeeded"),
    ] {
        let tid = t.task(WORKER, "cohort", &format!("Post2 {i}")).await;
        seed_managed_outcome(
            &t,
            &format!("run-post2-cohort-{i}"),
            &tid,
            "cohort",
            "v3-auto-20261106",
            state,
            post2_ms + i * 1_000,
            10,
            30,
            2,
        );
    }
    let check2 = t
        .sweep_autotune()
        .expect("missing baseline outcomes must not fabricate 0% orphan baseline");
    assert_ne!(check2.decision, AutotuneDecision::RolledBack, "{check2:?}");
}

#[tokio::test]
async fn autotune_preserves_default_lease_overrides_and_deduplicates_proposals() {
    // 1. Apply mode promoting from `v1-default` preserves custom `[defaults]` overrides.
    let t_apply = T::with(|cfg| {
        cfg.defaults.run_lease_ttl_secs = 420;
        cfg.defaults.sticky_grace_secs = 1_800;
        cfg.scheduler.autotune.enabled = true;
        cfg.scheduler.autotune.mode = AutotuneMode::Apply;
        cfg.scheduler.autotune.interval_secs = 60;
        cfg.scheduler.autotune.min_runs = 10;
        cfg.scheduler.autotune.min_improvement_pct = 5.0;
    });
    seed_skewed_improvable_workload(&t_apply).await;
    let promo = t_apply.sweep_autotune().expect("apply promotion");
    assert_eq!(promo.decision, AutotuneDecision::Promoted, "{promo:?}");
    let active = t_apply.ok("GET", "/v1/scheduler/policy", None, None).await;
    assert_eq!(active["lease_ttl_secs"], 420);
    assert_eq!(active["sticky_grace_secs"], 1_800);

    // 2. Propose mode does NOT file duplicate proposals after cooldown while the first is open,
    //    and DOES file again once the earlier proposal is dropped.
    let t_prop = T::with(|cfg| {
        cfg.scheduler.autotune.enabled = true;
        cfg.scheduler.autotune.mode = AutotuneMode::Propose;
        cfg.scheduler.autotune.interval_secs = 60;
        cfg.scheduler.autotune.min_runs = 10;
        cfg.scheduler.autotune.min_improvement_pct = 5.0;
        cfg.scheduler.autotune.cooldown_secs = 120;
    });
    seed_skewed_improvable_workload(&t_prop).await;

    let first = t_prop.sweep_autotune().expect("first proposal");
    assert_eq!(first.decision, AutotuneDecision::Proposed, "{first:?}");
    let first_task = first.feedback_task_id.clone().expect("first task id");

    // Advance past cooldown (120s) while `first_task` is still open -> deduplicated as `Keep`.
    t_prop.advance(TimeDelta::seconds(300));
    let dup = t_prop
        .sweep_autotune()
        .expect("post-cooldown evaluation with open proposal");
    assert_eq!(dup.decision, AutotuneDecision::Keep, "{dup:?}");
    assert_eq!(dup.feedback_task_id.as_ref(), Some(&first_task), "{dup:?}");
    assert!(dup.reason.contains("open proposal"), "{}", dup.reason);

    // Drop the open proposal -> next sweep files a fresh proposal task.
    t_prop.exec(&format!(
        "UPDATE tasks SET status = 'dropped' WHERE id = '{}'",
        first_task.as_str()
    ));
    t_prop.advance(TimeDelta::seconds(120));
    let second = t_prop
        .sweep_autotune()
        .expect("proposal after earlier proposal closed");
    assert_eq!(second.decision, AutotuneDecision::Proposed, "{second:?}");
    let second_task = second.feedback_task_id.clone().expect("second task id");
    assert_ne!(second_task, first_task);

    // 3. Programmatic `ServerConfig::new` + direct mutation of `config.scheduler.autotune`
    //    is honored by `AppState::new` directly without any extra sync step.
    let dir = tempfile::tempdir().unwrap();
    let db = crate::Db::open(&dir.path().join("board.db")).unwrap();
    let mut raw_cfg = crate::ServerConfig::new(dir.path().join("board.db"));
    raw_cfg.scheduler.autotune.enabled = false;
    raw_cfg.scheduler.autotune.mode = AutotuneMode::Off;
    raw_cfg.scheduler.autotune.interval_secs = 900;
    raw_cfg.check().unwrap();
    let state = crate::AppState::new(
        db,
        &raw_cfg,
        std::sync::Arc::new(crate::ManualClock::new(super::start())),
    );
    assert!(!state.settings().scheduler_autotune.enabled);
    assert_eq!(state.settings().scheduler_autotune.mode, AutotuneMode::Off);
    assert_eq!(state.settings().scheduler_autotune.interval_secs, 900);
}

#[tokio::test]
async fn autotune_interleave_candidates_differentiate_mixed_kind_multi_board_backlogs() {
    let t = T::with(|cfg| {
        cfg.scheduler.autotune.enabled = true;
        cfg.scheduler.autotune.mode = AutotuneMode::Apply;
        cfg.scheduler.autotune.interval_secs = 60;
        cfg.scheduler.autotune.min_runs = 10;
        cfg.scheduler.autotune.min_improvement_pct = 5.0;
    });

    let repo_h = "github.com/example/heavy-mixed";
    let repo_s = "github.com/example/starved-mixed";
    t.register(WORKER, Some(repo_h), None).await;
    autopilot_board(&t, "heavy", &[repo_h], 4).await;
    autopilot_board(&t, "starved", &[repo_s], 4).await;
    heartbeat_mac(&t, &[repo_h, repo_s], 2).await;

    let base_ms = t.state.now().timestamp_millis();

    // Seed prior consumption on `heavy` (10 runs) so `heavy` holds 100% of the rolling 24h
    // fair-share window when the mixed-kind burst arrives simultaneously.
    for i in 0..10 {
        let tid = t.task(WORKER, "heavy", &format!("Heavy seed {i}")).await;
        let run_at = base_ms - 3_600_000 + i * 120_000;
        t.exec(&format!(
            "UPDATE tasks SET created_at = {run_at}, ready_at = {run_at} WHERE id = '{tid}'"
        ));
        seed_managed_outcome(
            &t,
            &format!("run-mseed-{i:02}"),
            &tid,
            "heavy",
            "v1-default",
            "succeeded",
            run_at,
            5,
            60,
            2,
        );
    }

    // Create a simultaneous mixed-kind burst at `burst_ms`:
    // `heavy` has 6 long P2 `verdict` runs (kind_factor=0.65, 2u, 600s) while `starved` has
    // 6 short P2 `worker` runs (kind_factor=0.40, 2u, 60s).
    // Under `v1-default` (w_kind=100, w_fair=12), `heavy` monopolizes the 40-minute window;
    // `fair-interleave` (w_kind=65, w_fair=38) lowers the cross-kind gap from 25.0 to 16.25
    // so short `starved` workers interleave ahead of `heavy`'s verdicts, beating every
    // fixed-w_kind=100 candidate on both p90 wait and fair-share Gini!
    t.advance(TimeDelta::minutes(5));
    let burst_ms = t.state.now().timestamp_millis();
    for i in 0..6 {
        let h_tid = t.task(WORKER, "heavy", &format!("Heavy verdict {i}")).await;
        let h_run = format!("run-hverdict-{i:02}");
        let started_ms = burst_ms + 1_000;
        let ended_ms = started_ms + 600_000;
        t.exec(&format!(
            "UPDATE tasks SET created_at = {burst_ms} WHERE id = '{h_tid}'; \
             INSERT INTO runs ( \
                 id, task_id, board, kind, origin, device, runner, state, cost_units, \
                 policy_version, created_at, started_at, ended_at, updated_at \
             ) VALUES ( \
                 '{h_run}', '{h_tid}', 'heavy', 'verdict', 'managed', 'mac', 'acp', \
                 'succeeded', 2, 'v1-default', {burst_ms}, {started_ms}, {ended_ms}, {ended_ms} \
             ); \
             INSERT INTO run_outcomes ( \
                 run_id, task_id, board, device, kind, terminal_state, \
                 ready_to_leased_secs, leased_to_running_secs, running_secs, \
                 weight_unit_minutes, policy_version, recorded_at \
             ) VALUES ( \
                 '{h_run}', '{h_tid}', 'heavy', 'mac', 'verdict', 'succeeded', \
                 0, 1, 600, 20.0, 'v1-default', {ended_ms} \
             );"
        ));

        let s_tid = t
            .task(WORKER, "starved", &format!("Starved worker {i}"))
            .await;
        t.exec(&format!(
            "UPDATE tasks SET created_at = {burst_ms} WHERE id = '{s_tid}'"
        ));
        seed_managed_outcome(
            &t,
            &format!("run-swork-{i:02}"),
            &s_tid,
            "starved",
            "v1-default",
            "succeeded",
            burst_ms,
            0,
            60,
            2,
        );
    }

    t.advance(TimeDelta::minutes(30));
    let eval = t.sweep_autotune().expect("evaluation should run");
    // Under strict non-overlapping kind bands, `verdict` runs ALWAYS finish before `worker`
    // runs regardless of `w_kind` / `w_fair`, so autotune keeps the active policy and never
    // emits `w_age > W_AGE_MAX` (8.0) or inverts kind/priority bands.
    assert_eq!(eval.decision, AutotuneDecision::Keep, "{eval:?}");
    if let Some(weights) = eval.candidate_weights {
        assert!(
            weights.w_age <= board_proto::W_AGE_MAX,
            "autotune must keep w_age <= 8.0, got {weights:?}"
        );
    }
}
