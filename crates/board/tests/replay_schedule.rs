//! End-to-end CLI tests for `board admin replay-schedule` and `board stats --scheduler`.

use std::process::Command;

use board_proto::{ReplayComparison, SchedulerStats};
use board_server::{AppState, Db, ManualClock, ServerConfig, app};
use chrono::{TimeDelta, TimeZone, Utc};
use serde_json::json;
use tokio::net::TcpListener;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_board"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_replay_schedule_and_stats_scheduler_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("board.db");
    let db = Db::open(&db_path).unwrap();
    let now = Utc::now();
    let clock = ManualClock::new(now - TimeDelta::hours(2));
    let mut cfg = ServerConfig::new(&db_path);
    cfg.users = vec!["user@example.com".into()];
    let state = AppState::new(db, &cfg, std::sync::Arc::new(clock.clone()));
    let user_token = state
        .create_device(&"mac-pro".parse().unwrap(), true)
        .unwrap();

    // Start in-process HTTP server to populate a realistic fixture DB and serve `board stats --scheduler`.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let serve_state = state.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app(serve_state)).await.unwrap();
    });

    let http = reqwest::Client::new();
    let auth = format!("Bearer {user_token}");
    let repo = "github.com/example/replay-demo";

    // Heartbeat device, create board, register agent, add & run a task.
    http.put(format!("{base_url}/v1/devices/mac-pro/heartbeat"))
        .header("authorization", &auth)
        .json(&json!({
            "os": "macos",
            "arch": "arm64",
            "capacity": { "weight_units": 8, "mem_gib": 32, "cpu_cores": 10 },
            "free_units": 8,
            "capabilities": ["git:cloneable"],
            "runners": ["acp", "deterministic"],
            "repos": [repo]
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    http.post(format!("{base_url}/v1/boards"))
        .header("authorization", &auth)
        .json(&json!({
            "slug": "demo",
            "title": "Demo",
            "repos": [repo],
            "autopilot": true
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    http.post(format!("{base_url}/v1/agents"))
        .header("authorization", &auth)
        .header("x-board-agent", "t3:worker")
        .json(&json!({
            "id": "t3:worker",
            "runtime": "antigravity",
            "device": "mac-pro",
            "repo": repo
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    http.post(format!("{base_url}/v1/tasks"))
        .header("authorization", &auth)
        .header("x-board-agent", "t3:worker")
        .json(&json!({
            "board": "demo",
            "agent": "t3:worker",
            "title": "Demo task",
            "weight": "standard"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    clock.advance(TimeDelta::seconds(30));
    let claim: serde_json::Value = http
        .post(format!("{base_url}/v1/dispatch/claim"))
        .header("authorization", &auth)
        .json(&json!({
            "device": "mac-pro",
            "free_units": 8,
            "runners": ["acp", "deterministic"],
            "capabilities": ["git:cloneable"],
            "repos": [repo]
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let run_id = claim["claimed"][0]["run"]["id"].as_str().unwrap();
    clock.advance(TimeDelta::seconds(10));
    http.patch(format!("{base_url}/v1/runs/{run_id}"))
        .header("authorization", &auth)
        .json(&json!({ "state": "running" }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    clock.advance(TimeDelta::seconds(120));
    http.patch(format!("{base_url}/v1/runs/{run_id}"))
        .header("authorization", &auth)
        .json(&json!({
            "state": "succeeded",
            "outcome": { "warm_cache_hit": true, "first_pass_merge": true }
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // 1. Test `board admin replay-schedule --db <path> --policy <candidate.toml> --json`.
    let policy_path = dir.path().join("candidate.toml");
    std::fs::write(
        &policy_path,
        r#"
version = "v2-candidate"
[weights]
w_kind = 100.0
w_prio = 30.0
w_rank = 10.0
w_age = 24.0
w_fair = 30.0
w_loc = 16.0
w_pref = 1.0
w_backfill = 10.0
"#,
    )
    .unwrap();

    let out = bin()
        .args([
            "admin",
            "replay-schedule",
            "--db",
            db_path.to_str().unwrap(),
            "--policy",
            policy_path.to_str().unwrap(),
            "--days",
            "7",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let cmp: ReplayComparison = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(cmp.baseline.policy_version, "v1-default");
    assert_eq!(cmp.baseline.tasks_scheduled, 1);
    let cand = cmp.candidate.expect("candidate present");
    assert_eq!(cand.policy_version, "v2-candidate");
    assert_eq!(cand.tasks_scheduled, 1);

    // Also verify human-readable table mode works cleanly.
    let table_out = bin()
        .args([
            "admin",
            "replay-schedule",
            "--db",
            db_path.to_str().unwrap(),
            "--policy",
            policy_path.to_str().unwrap(),
            "--since",
            "7d",
        ])
        .output()
        .unwrap();
    assert!(
        table_out.status.success(),
        "{}",
        String::from_utf8_lossy(&table_out.stderr)
    );
    let table_text = String::from_utf8_lossy(&table_out.stdout);
    assert!(table_text.contains("v1-default"), "{table_text}");
    assert!(table_text.contains("v2-candidate"), "{table_text}");
    assert!(table_text.contains("fair_share_gini"), "{table_text}");

    // 2. Test `board stats --scheduler --days 7 --json` against the live server.
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!("server = \"{base_url}\"\ndevice = \"mac-pro\"\n"),
    )
    .unwrap();
    let stats_out = bin()
        .env("BOARD_HOME", &home)
        .env("BOARD_TOKEN", &user_token)
        .args(["stats", "--scheduler", "--days", "7", "--json"])
        .output()
        .unwrap();
    assert!(
        stats_out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&stats_out.stdout),
        String::from_utf8_lossy(&stats_out.stderr)
    );
    let stats: SchedulerStats = serde_json::from_slice(&stats_out.stdout).unwrap();
    assert_eq!(stats.runs_total, 1);
    assert_eq!(stats.per_policy_version[0].policy_version, "v1-default");
    assert_eq!(stats.first_pass_merge_rate, Some(1.0));
    assert_eq!(stats.warm_cache_hit_rate, Some(1.0));

    // Also test non-JSON text rendering of `board stats --scheduler`.
    let stats_text_out = bin()
        .env("BOARD_HOME", &home)
        .env("BOARD_TOKEN", &user_token)
        .args([
            "stats",
            "--scheduler",
            "--days",
            "7",
            "--board",
            "demo",
            "--policy-version",
            "v1-default",
        ])
        .output()
        .unwrap();
    assert!(
        stats_text_out.status.success(),
        "{}",
        String::from_utf8_lossy(&stats_text_out.stderr)
    );
    let stats_text = String::from_utf8_lossy(&stats_text_out.stdout);
    assert!(stats_text.contains("scheduler stats"), "{stats_text}");
    assert!(stats_text.contains("v1-default"), "{stats_text}");

    // Keep unused chrono helper quiet if needed.
    let _ = Utc.with_ymd_and_hms(2026, 11, 24, 0, 0, 0).unwrap();
    state.shutdown();
    server.abort();
}
