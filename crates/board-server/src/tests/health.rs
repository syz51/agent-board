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

#[tokio::test]
async fn rejects_protocol_relative_deep_links_and_preserves_host_collector_precedence() {
    let t = T::new();

    // Protocol-relative `//` deep_link must be rejected.
    let (status, code, _) = t
        .fail(
            "POST",
            "/v1/hosts/mac/schedules",
            None,
            Some(json!({
                "source": "launchd",
                "jobs": [{
                    "id": "bad.link",
                    "name": "Bad link",
                    "schedule_human": "Hourly",
                    "state": "ok",
                    "deep_link": "//attacker.example/phish"
                }]
            })),
        )
        .await;
    assert_eq!((status, code.as_str()), (400, "invalid"));

    // Primary `"host"` collector fields must not be clobbered by secondary collectors.
    t.ok(
        "POST",
        "/v1/hosts/mac/health",
        None,
        Some(json!({
            "collector": "host",
            "ttl_secs": 300,
            "os": "macos",
            "arch": "arm64",
            "uptime_secs": 1000,
            "versions": {"board": "primary-sha"}
        })),
    )
    .await;
    t.ok(
        "POST",
        "/v1/hosts/mac/health",
        None,
        Some(json!({
            "collector": "prometheus",
            "ttl_secs": 300,
            "os": "linux",
            "arch": "amd64",
            "uptime_secs": 5,
            "versions": {"board": "secondary-sha", "prom": "2.55"}
        })),
    )
    .await;

    let overview = t.ok("GET", "/v1/health/overview", None, None).await;
    let host = &overview["hosts"][0];
    assert_eq!(host["os"], "macos");
    assert_eq!(host["arch"], "arm64");
    assert_eq!(host["uptime_secs"], 1000);
    assert_eq!(host["versions"]["board"], "primary-sha");
    assert_eq!(host["versions"]["prom"], "2.55");
}

#[tokio::test]
async fn launchd_silent_start_interval_tracks_run_count_and_detects_real_stalls() {
    let t = T::new();
    let base = t.clock.now();
    let stale_mtime = base - TimeDelta::hours(10);
    let stale_next = stale_mtime + TimeDelta::minutes(10);

    let post_silent = |runs: u64, state: &str| {
        json!({
            "source": "launchd",
            "jobs": [{
                "id": "com.example.disk-alert",
                "name": "com.example.disk-alert",
                "category": "monitoring",
                "enabled": true,
                "schedule_human": "Every 10m",
                "schedule_raw": "StartInterval=600",
                "expected_interval_secs": 600,
                "state": state,
                "last_run_at": iso(stale_mtime),
                "next_run_at": iso(stale_next),
                "last_exit_code": 0,
                "run_count": runs
            }]
        })
    };

    // 1. First report with stale log mtime & stale next_run_at (simulating an older collector)
    //    must NOT falsely flag overdue; server starts a 1-interval observation window.
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(post_silent(69, "ok")),
    )
    .await;
    let list = t.ok("GET", "/v1/schedules", None, None).await;
    assert_eq!(list["summary"]["overdue"], 0);
    assert_eq!(list["summary"]["ok"], 1);
    assert_eq!(list["jobs"][0]["state"], "ok");
    assert_eq!(
        list["jobs"][0]["next_run_at"],
        iso(base + TimeDelta::minutes(10))
    );

    // Steady-state poll (same run_count=69) emits no spurious event or history entry.
    t.advance(TimeDelta::minutes(1));
    let events_before = t.event_types(0).await.len();
    let ack_steady = t
        .ok(
            "POST",
            "/v1/hosts/mac/schedules",
            None,
            Some(post_silent(69, "ok")),
        )
        .await;
    assert_eq!(ack_steady["history_recorded"], 0);
    assert_eq!(t.event_types(0).await.len(), events_before);

    // 2. Job runs silently (runs 69 -> 70) while stdout/stderr mtime stays stale:
    //    server advances last_run_at, next_run_at, and history timestamp to `now`.
    t.advance(TimeDelta::minutes(7));
    let run70_at = t.clock.now();
    let ack70 = t
        .ok(
            "POST",
            "/v1/hosts/mac/schedules",
            None,
            Some(post_silent(70, "ok")),
        )
        .await;
    assert_eq!(ack70["history_recorded"], 1);
    let list70 = t.ok("GET", "/v1/schedules", None, None).await;
    assert_eq!(list70["jobs"][0]["state"], "ok");
    assert_eq!(list70["jobs"][0]["last_run_at"], iso(run70_at));
    assert_eq!(
        list70["jobs"][0]["next_run_at"],
        iso(run70_at + TimeDelta::minutes(10))
    );

    let hist70 = t
        .ok(
            "GET",
            "/v1/schedules/mac:launchd:com.example.disk-alert/history",
            None,
            None,
        )
        .await;
    assert_eq!(hist70["entries"][0]["run_count"], 70);
    assert_eq!(hist70["entries"][0]["at"], iso(run70_at));

    // 3. Subsequent poll with unchanged runs=70 and stale mtime must NOT regress last_run_at
    //    or slide next_run_at forward.
    t.advance(TimeDelta::minutes(2));
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(post_silent(70, "ok")),
    )
    .await;
    let list_after_poll = t.ok("GET", "/v1/schedules", None, None).await;
    assert_eq!(list_after_poll["jobs"][0]["state"], "ok");
    assert_eq!(list_after_poll["jobs"][0]["last_run_at"], iso(run70_at));
    assert_eq!(
        list_after_poll["jobs"][0]["next_run_at"],
        iso(run70_at + TimeDelta::minutes(10))
    );

    // 4. Real stall: advance past 10m interval + 150s grace while polling with stuck runs=70.
    //    The job MUST flip to overdue.
    t.advance(TimeDelta::minutes(12));
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(post_silent(70, "ok")),
    )
    .await;
    let stalled = t
        .ok("GET", "/v1/schedules?status=overdue", None, None)
        .await;
    assert_eq!(stalled["summary"]["overdue"], 1);
    assert_eq!(stalled["jobs"][0]["id"], "com.example.disk-alert");
    assert_eq!(stalled["jobs"][0]["state"], "overdue");

    // 5. Recovery: runs increments 70 -> 71 silently -> immediately returns to ok.
    t.advance(TimeDelta::minutes(1));
    let run71_at = t.clock.now();
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(post_silent(71, "ok")),
    )
    .await;
    let recovered = t.ok("GET", "/v1/schedules", None, None).await;
    assert_eq!(recovered["summary"]["overdue"], 0);
    assert_eq!(recovered["jobs"][0]["state"], "ok");
    assert_eq!(recovered["jobs"][0]["last_run_at"], iso(run71_at));

    // 6. Host reboot / launchctl reload resets runs 71 -> 1 after running at load.
    t.advance(TimeDelta::minutes(5));
    let reboot_run_at = t.clock.now();
    let ack_reset = t
        .ok(
            "POST",
            "/v1/hosts/mac/schedules",
            None,
            Some(post_silent(1, "ok")),
        )
        .await;
    assert_eq!(ack_reset["history_recorded"], 1);
    let after_reset = t.ok("GET", "/v1/schedules", None, None).await;
    assert_eq!(after_reset["jobs"][0]["state"], "ok");
    assert_eq!(after_reset["jobs"][0]["last_run_at"], iso(reboot_run_at));
    assert_eq!(
        after_reset["jobs"][0]["next_run_at"],
        iso(reboot_run_at + TimeDelta::minutes(10))
    );
}

#[tokio::test]
async fn launchd_migrated_row_heals_once_noisy_preserves_mtime_and_t3_stays_authoritative() {
    let t = T::new();
    let base = t.clock.now();
    let stale_ms = (base - TimeDelta::days(3)).timestamp_millis();
    let stale_next_ms = (base - TimeDelta::days(3) + TimeDelta::hours(1)).timestamp_millis();
    let now_ms = base.timestamp_millis();

    // Seed a pre-migration-0039 row (run_count_observed_at IS NULL) with stale mtime/next_run_at.
    t.exec(&format!(
        "INSERT INTO scheduled_jobs ( \
             job_key, host, source, job_id, name, category, enabled, \
             schedule_human, schedule_raw, expected_interval_secs, state, \
             last_run_at, next_run_at, last_exit_code, run_count, run_count_observed_at, \
             reported_at, updated_at \
         ) VALUES ( \
             'mac:launchd:com.example.sync', 'mac', 'launchd', 'com.example.sync', \
             'com.example.sync', 'agent_ops', 1, 'Hourly', 'StartInterval=3600', 3600, \
             'ok', {stale_ms}, {stale_next_ms}, 0, 301, NULL, {now_ms}, {now_ms} \
         );"
    ));

    // Before the first post-migration report, the seeded row evaluates as overdue.
    let before = t.ok("GET", "/v1/schedules", None, None).await;
    assert_eq!(before["jobs"][0]["state"], "overdue");

    // First post-migration poll with unchanged runs=301 heals the baseline ONCE.
    t.advance(TimeDelta::minutes(1));
    let heal_at = t.clock.now();
    let stale_payload = json!({
        "source": "launchd",
        "jobs": [{
            "id": "com.example.sync",
            "name": "com.example.sync",
            "category": "agent_ops",
            "enabled": true,
            "schedule_human": "Hourly",
            "schedule_raw": "StartInterval=3600",
            "expected_interval_secs": 3600,
            "state": "ok",
            "last_run_at": iso(base - TimeDelta::days(3)),
            "next_run_at": iso(base - TimeDelta::days(3) + TimeDelta::hours(1)),
            "last_exit_code": 0,
            "run_count": 301
        }]
    });
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(stale_payload.clone()),
    )
    .await;
    let healed = t.ok("GET", "/v1/schedules", None, None).await;
    assert_eq!(healed["jobs"][0]["state"], "ok");
    assert_eq!(
        healed["jobs"][0]["next_run_at"],
        iso(heal_at + TimeDelta::hours(1))
    );

    // Polling repeatedly for 80 minutes with stuck runs=301 must NOT slide the healed deadline:
    // after 60m + 15m grace, it MUST become overdue.
    for _ in 0..4 {
        t.advance(TimeDelta::minutes(20));
        t.ok(
            "POST",
            "/v1/hosts/mac/schedules",
            None,
            Some(stale_payload.clone()),
        )
        .await;
    }
    let still_stuck = t.ok("GET", "/v1/schedules", None, None).await;
    assert_eq!(still_stuck["jobs"][0]["state"], "overdue");

    // Silent `running -> ok` transition records `ok` at `reported_at` (after `running`), not stale mtime.
    t.advance(TimeDelta::minutes(1));
    let started_at = t.clock.now();
    let mut running_payload = stale_payload.clone();
    running_payload["jobs"][0]["state"] = json!("running");
    running_payload["jobs"][0]["run_count"] = json!(302);
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(running_payload),
    )
    .await;
    t.advance(TimeDelta::minutes(3));
    let finished_at = t.clock.now();
    let mut done_payload = stale_payload;
    done_payload["jobs"][0]["run_count"] = json!(302);
    t.ok("POST", "/v1/hosts/mac/schedules", None, Some(done_payload))
        .await;
    let hist = t
        .ok(
            "GET",
            "/v1/schedules/mac:launchd:com.example.sync/history",
            None,
            None,
        )
        .await;
    assert_eq!(hist["entries"][0]["state"], "ok");
    assert_eq!(hist["entries"][0]["at"], iso(finished_at));
    assert_eq!(hist["entries"][1]["state"], "running");
    assert_eq!(hist["entries"][1]["at"], iso(started_at));

    // Noisy launchd job: fresh log mtime is preserved exactly instead of overwritten with reported_at.
    t.advance(TimeDelta::minutes(1));
    let noisy_run_at = t.clock.now() - TimeDelta::seconds(30);
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(json!({
            "source": "launchd",
            "jobs": [{
                "id": "com.example.noisy",
                "name": "Noisy sync",
                "category": "sync",
                "enabled": true,
                "schedule_human": "Every 30m",
                "schedule_raw": "StartInterval=1800",
                "expected_interval_secs": 1800,
                "state": "ok",
                "last_run_at": iso(noisy_run_at),
                "next_run_at": iso(noisy_run_at + TimeDelta::minutes(30)),
                "last_exit_code": 0,
                "run_count": 10
            }]
        })),
    )
    .await;
    t.advance(TimeDelta::minutes(30));
    let noisy_run2_at = t.clock.now() - TimeDelta::seconds(12);
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(json!({
            "source": "launchd",
            "jobs": [{
                "id": "com.example.noisy",
                "name": "Noisy sync",
                "category": "sync",
                "enabled": true,
                "schedule_human": "Every 30m",
                "schedule_raw": "StartInterval=1800",
                "expected_interval_secs": 1800,
                "state": "ok",
                "last_run_at": iso(noisy_run2_at),
                "next_run_at": iso(noisy_run2_at + TimeDelta::minutes(30)),
                "last_exit_code": 0,
                "run_count": 11
            }]
        })),
    )
    .await;
    let noisy_list = t
        .ok("GET", "/v1/schedules?q=com.example.noisy", None, None)
        .await;
    assert_eq!(noisy_list["jobs"][0]["last_run_at"], iso(noisy_run2_at));
    assert_eq!(
        noisy_list["jobs"][0]["next_run_at"],
        iso(noisy_run2_at + TimeDelta::minutes(30))
    );

    // Non-launchd (T3) jobs keep authoritative timestamps and flag overdue immediately on first report.
    let t3_now = t.clock.now();
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(json!({
            "source": "t3",
            "jobs": [{
                "id": "sched-t3-late",
                "name": "Late T3 task",
                "category": "agent_ops",
                "enabled": true,
                "schedule_human": "Hourly",
                "schedule_raw": "{\"type\":\"interval\",\"everyMs\":3600000}",
                "expected_interval_secs": 3600,
                "state": "ok",
                "last_run_at": iso(t3_now - TimeDelta::hours(4)),
                "next_run_at": iso(t3_now - TimeDelta::hours(3)),
                "run_count": 5
            }]
        })),
    )
    .await;
    let t3_list = t.ok("GET", "/v1/schedules?source=t3", None, None).await;
    assert_eq!(t3_list["jobs"][0]["state"], "overdue");
}

#[tokio::test]
async fn launchd_review_regressions_combo_stall_disabled_reenable_and_multi_run_burst() {
    let t = T::new();
    let base = t.clock.now();
    let far_cal = base + TimeDelta::days(5);
    let stale_mtime = base - TimeDelta::days(3);

    // 1. Combined StartInterval (3600s) + StartCalendarInterval (far future):
    //    silent run must pick min(far_cal, now + 1h) = now + 1h, and when stalled past 1h + grace
    //    the far-future calendar fallback must NOT push next_run_at forward and mask overdue.
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(json!({
            "source": "launchd",
            "jobs": [{
                "id": "com.example.combo",
                "name": "Combo job",
                "category": "agent_ops",
                "enabled": true,
                "schedule_human": "Hourly Mon 09:00",
                "schedule_raw": "StartInterval=3600; StartCalendarInterval={\"Hour\":9,\"Minute\":0,\"Weekday\":1}",
                "expected_interval_secs": 3600,
                "state": "ok",
                "last_run_at": iso(stale_mtime),
                "next_run_at": iso(far_cal),
                "last_exit_code": 0,
                "run_count": 10
            }]
        })),
    )
    .await;
    let combo1 = t
        .ok("GET", "/v1/schedules?q=com.example.combo", None, None)
        .await;
    assert_eq!(
        combo1["jobs"][0]["next_run_at"],
        iso(base + TimeDelta::hours(1))
    );

    // Silent run bumps runs 10 -> 11: next_run_at must be run11_at + 1h, not far_cal.
    t.advance(TimeDelta::minutes(50));
    let run11_at = t.clock.now();
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(json!({
            "source": "launchd",
            "jobs": [{
                "id": "com.example.combo",
                "name": "Combo job",
                "category": "agent_ops",
                "enabled": true,
                "schedule_human": "Hourly Mon 09:00",
                "schedule_raw": "StartInterval=3600; StartCalendarInterval={\"Hour\":9,\"Minute\":0,\"Weekday\":1}",
                "expected_interval_secs": 3600,
                "state": "ok",
                "last_run_at": iso(stale_mtime),
                "next_run_at": iso(far_cal),
                "last_exit_code": 0,
                "run_count": 11
            }]
        })),
    )
    .await;
    let combo2 = t
        .ok("GET", "/v1/schedules?q=com.example.combo", None, None)
        .await;
    assert_eq!(
        combo2["jobs"][0]["next_run_at"],
        iso(run11_at + TimeDelta::hours(1))
    );

    // Stall past run11_at + 1h + 15m grace while collector keeps sending far_cal:
    // pinned deadline must NOT slide to far_cal, so job transitions to overdue.
    t.advance(TimeDelta::minutes(65));
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(json!({
            "source": "launchd",
            "jobs": [{
                "id": "com.example.combo",
                "name": "Combo job",
                "category": "agent_ops",
                "enabled": true,
                "schedule_human": "Hourly Mon 09:00",
                "schedule_raw": "StartInterval=3600; StartCalendarInterval={\"Hour\":9,\"Minute\":0,\"Weekday\":1}",
                "expected_interval_secs": 3600,
                "state": "ok",
                "last_run_at": iso(stale_mtime),
                "next_run_at": iso(far_cal),
                "last_exit_code": 0,
                "run_count": 11
            }]
        })),
    )
    .await;
    t.advance(TimeDelta::minutes(15));
    let combo_stalled = t
        .ok("GET", "/v1/schedules?q=com.example.combo", None, None)
        .await;
    assert_eq!(combo_stalled["jobs"][0]["state"], "overdue");
    assert_eq!(
        combo_stalled["jobs"][0]["next_run_at"],
        iso(run11_at + TimeDelta::hours(1))
    );

    // 2. Disabled launchd job with run_count > 0 must never fabricate next_run_at or emit
    //    spurious SSE events on poll 2, and re-enabling later must start a fresh window.
    let dis_payload = |enabled: bool, state: &str| {
        json!({
            "source": "launchd",
            "jobs": [{
                "id": "com.example.toggle",
                "name": "Toggle job",
                "category": "monitoring",
                "enabled": enabled,
                "schedule_human": "Every 10m",
                "schedule_raw": "StartInterval=600",
                "expected_interval_secs": 600,
                "state": state,
                "last_run_at": iso(stale_mtime),
                "last_exit_code": 0,
                "run_count": 10
            }]
        })
    };
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(dis_payload(false, "disabled")),
    )
    .await;
    t.advance(TimeDelta::minutes(1));
    let events_before_dis2 = t.event_types(0).await.len();
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(dis_payload(false, "disabled")),
    )
    .await;
    assert_eq!(t.event_types(0).await.len(), events_before_dis2);
    let dis_row = t
        .ok("GET", "/v1/schedules?q=com.example.toggle", None, None)
        .await;
    assert_eq!(dis_row["jobs"][0]["state"], "disabled");
    assert!(dis_row["jobs"][0].get("next_run_at").is_none());

    // Re-enable 2 hours later with unchanged run_count=10: must be ok with fresh 10m window.
    t.advance(TimeDelta::hours(2));
    let reenabled_at = t.clock.now();
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(dis_payload(true, "ok")),
    )
    .await;
    let reenabled = t
        .ok("GET", "/v1/schedules?q=com.example.toggle", None, None)
        .await;
    assert_eq!(reenabled["jobs"][0]["state"], "ok");
    assert_eq!(
        reenabled["jobs"][0]["next_run_at"],
        iso(reenabled_at + TimeDelta::minutes(10))
    );

    // 3. Multi-run burst (noisy run at T+60s followed by silent runs at T+120s..T+300s):
    //    when runs jumps 10 -> 15 at T+300s with log mtime stuck at T+60s, the server must
    //    anchor last_run_at to T+300s (not T+60s) so the active job is NOT falsely overdue.
    let burst_base = t.clock.now();
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(json!({
            "source": "launchd",
            "jobs": [{
                "id": "com.example.burst",
                "name": "Fast job",
                "category": "monitoring",
                "enabled": true,
                "schedule_human": "Every 1m",
                "schedule_raw": "StartInterval=60",
                "expected_interval_secs": 60,
                "state": "ok",
                "last_run_at": iso(burst_base),
                "next_run_at": iso(burst_base + TimeDelta::seconds(60)),
                "last_exit_code": 0,
                "run_count": 10
            }]
        })),
    )
    .await;
    t.advance(TimeDelta::seconds(300));
    let burst_now = t.clock.now();
    t.ok(
        "POST",
        "/v1/hosts/mac/schedules",
        None,
        Some(json!({
            "source": "launchd",
            "jobs": [{
                "id": "com.example.burst",
                "name": "Fast job",
                "category": "monitoring",
                "enabled": true,
                "schedule_human": "Every 1m",
                "schedule_raw": "StartInterval=60",
                "expected_interval_secs": 60,
                "state": "ok",
                "last_run_at": iso(burst_base + TimeDelta::seconds(60)),
                "last_exit_code": 0,
                "run_count": 15
            }]
        })),
    )
    .await;
    let burst_row = t
        .ok("GET", "/v1/schedules?q=com.example.burst", None, None)
        .await;
    assert_eq!(burst_row["jobs"][0]["state"], "ok");
    assert_eq!(burst_row["jobs"][0]["last_run_at"], iso(burst_now));
    assert_eq!(
        burst_row["jobs"][0]["next_run_at"],
        iso(burst_now + TimeDelta::seconds(60))
    );
}
