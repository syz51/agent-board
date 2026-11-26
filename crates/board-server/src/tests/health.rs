use chrono::TimeDelta;
use serde_json::json;

use super::{REPO, T, iso};
use crate::clock::Clock;

#[tokio::test]
async fn host_health_round_trip_merges_collectors_prunes_samples_and_separates_watchdog() {
    let t = T::new();

    // Non-user device `mac` cannot push health for another host.
    let (status, code, _) = t
        .fail(
            "POST",
            "/v1/hosts/other-host/health",
            None,
            Some(json!({ "collector": "host", "ttl_secs": 300 })),
        )
        .await;
    assert_eq!((status, code.as_str()), (403, "forbidden"));

    // Device `mac` pushes its own host metrics.
    let ack = t
        .ok(
            "POST",
            "/v1/hosts/mac/health",
            None,
            Some(json!({
                "collector": "host",
                "ttl_secs": 180,
                "os": "macos",
                "arch": "arm64",
                "uptime_secs": 86400,
                "resources": {
                    "cpu_pct": 22.5,
                    "load_1m": 1.4,
                    "mem_used_gib": 10.0,
                    "mem_total_gib": 16.0,
                    "disk_used_gib": 120.0,
                    "disk_total_gib": 250.0,
                    "disk_free_gib": 130.0
                },
                "versions": { "board": "deadbeef1234", "app": "1.2.3" },
                "services": [
                    { "name": "dev.board.watch", "kind": "launchd", "state": "running", "pid": 42 }
                ]
            })),
        )
        .await;
    assert_eq!(ack["host"], "mac");
    assert_eq!(ack["recorded_sample"], true);

    // User token can push an external monitoring bridge collector for the same host without
    // clobbering the `"host"` collector's resources or services.
    let (status, ack2) = t
        .as_user(
            "POST",
            "/v1/hosts/mac/health",
            Some(json!({
                "collector": "prometheus",
                "ttl_secs": 300,
                "storage": [
                    { "name": "shared-bucket", "backend": "s3", "used_gib": 40.5, "quota_gib": 50.0, "warn_pct": 80.0 }
                ],
                "backups": [
                    { "target": "pg-daily", "state": "ok", "last_success_at": iso(t.clock.now()), "sla_secs": 86400 }
                ],
                "certs": [
                    { "name": "api-tls", "kind": "tls", "expires_at": iso(t.clock.now() + TimeDelta::days(30)), "warn_days": 14, "ok": true }
                ],
                "alerts": [
                    { "name": "Watchdog", "severity": "info", "summary": "Alert pipeline alive", "is_watchdog": true }
                ],
                "vault": { "reachable": true, "sealed": false, "audit_ok": true, "agents_running": 2 },
                "ci": { "poller_fresh": true, "webhook_errors": 0, "runners_online": 2, "egress_ok": true }
            })),
        )
        .await;
    assert!(status.is_success(), "{status} {ack2}");

    // Read overview: Watchdog is separated (0 firing alerts, watchdog_ok = true), both
    // collectors merged cleanly, and server commit skew is detected when versions differ.
    let overview = t.ok("GET", "/v1/health/overview?days=7", None, None).await;
    assert_eq!(overview["incidents"]["firing_alerts"], 0);
    assert_eq!(overview["incidents"]["watchdog_ok"], true);
    assert_eq!(overview["hosts"].as_array().unwrap().len(), 1);
    let h = &overview["hosts"][0];
    assert_eq!(h["host"], "mac");
    assert_eq!(h["status"], "online");
    assert_eq!(h["collectors"], json!(["host", "prometheus"]));
    assert_eq!(h["resources"]["cpu_pct"], 22.5);
    assert_eq!(h["services"][0]["name"], "dev.board.watch");
    assert_eq!(overview["storage"][0]["name"], "shared-bucket");
    assert_eq!(overview["backups"][0]["target"], "pg-daily");
    assert_eq!(overview["certs"][0]["name"], "api-tls");
    assert_eq!(overview["vault"][0]["sealed"], false);
    assert_eq!(overview["ci"][0]["runners_online"], 2);

    // Steady-state tick (only cpu_pct changes) emits no extra `host_health_updated` event.
    let before_events = t.event_types(0).await;
    let health_events_before = before_events
        .iter()
        .filter(|e| e.as_str() == "host_health_updated")
        .count();
    t.ok(
        "POST",
        "/v1/hosts/mac/health",
        None,
        Some(json!({
            "collector": "host",
            "ttl_secs": 180,
            "resources": { "cpu_pct": 35.0 },
            "versions": { "board": "deadbeef1234", "app": "1.2.3" },
            "services": [
                { "name": "dev.board.watch", "kind": "launchd", "state": "running", "pid": 42 }
            ]
        })),
    )
    .await;
    let after_events = t.event_types(0).await;
    let health_events_after = after_events
        .iter()
        .filter(|e| e.as_str() == "host_health_updated")
        .count();
    assert_eq!(health_events_before, health_events_after);

    // Advancing past 30 days prunes old hourly samples when a new sample arrives.
    assert_eq!(t.count("SELECT COUNT(*) FROM host_health_samples"), 1);
    t.advance(TimeDelta::days(31));
    t.ok(
        "POST",
        "/v1/hosts/mac/health",
        None,
        Some(json!({
            "collector": "host",
            "ttl_secs": 180,
            "resources": { "cpu_pct": 12.0 }
        })),
    )
    .await;
    assert_eq!(t.count("SELECT COUNT(*) FROM host_health_samples"), 1);

    // Advancing past ttl_secs marks the host stale in overview.
    t.advance(TimeDelta::seconds(200));
    let stale_overview = t.ok("GET", "/v1/health/overview", None, None).await;
    assert_eq!(stale_overview["hosts"][0]["status"], "stale");
    assert_eq!(stale_overview["incidents"]["stale_hosts"], 1);
    assert_eq!(stale_overview["posture"], "degraded");
}

#[tokio::test]
async fn schedules_ring_buffer_pruning_filtering_and_overdue_clock() {
    let t = T::new();
    let base = t.clock.now();

    // Push 2 custom jobs + 1 OS job for `mac` / `launchd`.
    let ack = t
        .ok(
            "POST",
            "/v1/hosts/mac/schedules",
            None,
            Some(json!({
                "source": "launchd",
                "jobs": [
                    {
                        "id": "com.example.backup",
                        "name": "Hourly backup",
                        "category": "backup",
                        "enabled": true,
                        "schedule_human": "Hourly",
                        "schedule_raw": "StartInterval=3600",
                        "expected_interval_secs": 3600,
                        "state": "ok",
                        "last_run_at": iso(base),
                        "next_run_at": iso(base + TimeDelta::hours(1)),
                        "last_exit_code": 0,
                        "run_count": 10
                    },
                    {
                        "id": "com.example.weekly",
                        "name": "Weekly audit",
                        "category": "agent_ops",
                        "enabled": true,
                        "schedule_human": "Weekly",
                        "schedule_raw": "StartCalendarInterval=Sun 09:00",
                        "state": "failing",
                        "last_run_at": iso(base),
                        "last_exit_code": 1,
                        "run_count": 3,
                        "last_error": "script exited 1: missing token"
                    },
                    {
                        "id": "apt-daily",
                        "name": "APT daily",
                        "category": "os",
                        "enabled": true,
                        "schedule_human": "Daily",
                        "schedule_raw": "OnCalendar=daily",
                        "state": "ok",
                        "last_run_at": iso(base),
                        "last_exit_code": 0
                    }
                ]
            })),
        )
        .await;
    assert_eq!(ack["upserted"], 3);
    assert_eq!(ack["pruned"], 0);
    assert_eq!(ack["history_recorded"], 3);

    // Default GET /v1/schedules hides OS timers and sorts problems first.
    let list = t.ok("GET", "/v1/schedules", None, None).await;
    assert_eq!(list["summary"]["total"], 2);
    assert_eq!(list["summary"]["failing"], 1);
    assert_eq!(list["summary"]["ok"], 1);
    assert_eq!(list["summary"]["os_hidden"], 1);
    let jobs = list["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0]["id"], "com.example.weekly");
    assert_eq!(jobs[1]["id"], "com.example.backup");

    // include_os=1 reveals the OS job.
    let with_os = t.ok("GET", "/v1/schedules?include_os=1", None, None).await;
    assert_eq!(with_os["jobs"].as_array().unwrap().len(), 3);
    assert_eq!(with_os["summary"]["os_hidden"], 0);

    // Ring buffer caps history at 30 entries per job_key.
    for i in 1..=35_u64 {
        t.advance(TimeDelta::minutes(1));
        let now = t.clock.now();
        t.ok(
            "POST",
            "/v1/hosts/mac/schedules",
            None,
            Some(json!({
                "source": "launchd",
                "jobs": [
                    {
                        "id": "com.example.backup",
                        "name": "Hourly backup",
                        "category": "backup",
                        "enabled": true,
                        "schedule_human": "Hourly",
                        "schedule_raw": "StartInterval=3600",
                        "expected_interval_secs": 3600,
                        "state": "ok",
                        "last_run_at": iso(now),
                        "next_run_at": iso(now + TimeDelta::hours(1)),
                        "last_exit_code": 0,
                        "run_count": 10 + i
                    }
                ]
            })),
        )
        .await;
    }

    // The 2 omitted jobs (`com.example.weekly`, `apt-daily`) were pruned on the first batch.
    let after_prune = t.ok("GET", "/v1/schedules?include_os=1", None, None).await;
    assert_eq!(after_prune["jobs"].as_array().unwrap().len(), 1);

    let history = t
        .ok(
            "GET",
            "/v1/schedules/mac:launchd:com.example.backup/history",
            None,
            None,
        )
        .await;
    let entries = history["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 30);
    assert_eq!(entries[0]["run_count"], 45);
    assert_eq!(entries[29]["run_count"], 16);

    // Advancing clock past next_run_at + grace (900s for 3600s interval) dynamically marks job overdue.
    t.advance(TimeDelta::minutes(80));
    let overdue_list = t
        .ok("GET", "/v1/schedules?status=overdue", None, None)
        .await;
    assert_eq!(overdue_list["summary"]["overdue"], 1);
    assert_eq!(overdue_list["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(overdue_list["jobs"][0]["state"], "overdue");
}

#[tokio::test]
async fn health_overview_aggregates_tokens_leases_goal_cooldowns_and_upcoming_not_before() {
    let t = T::new();
    t.board("app", &[REPO]).await;
    t.register("t3:worker", Some(REPO), Some("main")).await;

    // Set goal_review_every_secs on `app` and create a task with future not_before.
    t.ok(
        "PATCH",
        "/v1/boards/app",
        None,
        Some(json!({ "autopilot": true, "goal_review_every_secs": 21600 })),
    )
    .await;
    let future_not_before = t.clock.now() + TimeDelta::days(5);
    t.task_with(
        "t3:worker",
        json!({
            "board": "app",
            "title": "Rotate service token",
            "not_before": iso(future_not_before),
        }),
    )
    .await;

    // Seed a managed run + run_outcomes with token counts.
    let now_ms = t.clock.now().timestamp_millis();
    t.exec(&format!(
        "INSERT INTO runs (id, board, kind, origin, device, runner, agent_id, state, cost_units, \
             outcome_json, created_at, started_at, ended_at, updated_at) \
         VALUES ('run-tok-1', 'app', 'worker', 'managed', 'mac', 'acp', 't3:worker', 'succeeded', 2, \
                 '{{\"model\":\"gemini-4-neon\"}}', {now_ms}, {now_ms}, {now_ms}, {now_ms}); \
         INSERT INTO run_outcomes (run_id, board, device, kind, terminal_state, tokens_in, \
             cached_tokens, tokens_out, reasoning_tokens, recorded_at) \
         VALUES ('run-tok-1', 'app', 'mac', 'worker', 'succeeded', 100000, 90000, 5000, 1200, {now_ms});"
    ));

    let overview = t.ok("GET", "/v1/health/overview?days=7", None, None).await;
    assert_eq!(overview["tokens"]["runs_with_tokens"], 1);
    assert_eq!(overview["tokens"]["tokens_in"], 100_000);
    assert_eq!(overview["tokens"]["cached_tokens"], 90_000);
    assert_eq!(overview["tokens"]["billable_in"], 10_000);
    assert_eq!(overview["tokens"]["tokens_out"], 5_000);
    assert_eq!(overview["tokens"]["reasoning_tokens"], 1_200);
    assert!((overview["tokens"]["cache_hit_rate"].as_f64().unwrap() - 0.9).abs() < 1e-6);
    assert_eq!(overview["tokens"]["by_board"][0]["key"], "app");
    assert_eq!(overview["tokens"]["by_model"][0]["key"], "gemini-4-neon");
    assert_eq!(overview["tokens"]["daily"].as_array().unwrap().len(), 1);
    assert_eq!(overview["factory"]["goal_cooldowns"][0]["board"], "app");
    assert_eq!(overview["certs"][0]["kind"], "task_not_before");
}
