//! End-to-end integration tests for Slice 4.3 CLI cockpit commands (`agent-board-686`):
//! `board runs`, `board logs` (including `-f/--follow` SSE tail), `board steer`,
//! `board cancel`, and `board attach` (local `daemon.db` + remote SSH/worktree checkout).

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use board::local_db::{LocalDb, LocalRunRecord, LocalRunState};
use board_proto::{
    AgentRole, CreateBoard, CreateRun, CreateTask, DeviceName, NewRunEvent, Priority,
    PushRunEvents, RegisterAgent, RunEventKind, RunEventRecord, RunKind, RunOrigin, RunState,
    Runtime, Timestamp, UpdateRun,
};
use board_server::{AppState, Db, ManualClock, ServerConfig};
use common::{board, stderr, stdout};
use tokio::net::TcpListener;

fn home_for_device(dir: &std::path::Path, server: &str, device: &str) -> PathBuf {
    let home = dir.join("board-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!("server = \"{server}\"\ndevice = \"{device}\"\n"),
    )
    .unwrap();
    home
}

fn t0() -> Timestamp {
    "2026-11-24T12:00:00Z".parse().unwrap()
}

struct RealServer {
    _dir: tempfile::TempDir,
    url: String,
    state: AppState,
    user_token: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for RealServer {
    fn drop(&mut self) {
        self.state.shutdown();
        self.handle.abort();
    }
}

impl RealServer {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("board.db");
        let db = Db::open(&db_path).unwrap();
        let clock = ManualClock::new(t0());
        let config = ServerConfig::new(db_path);
        let state = AppState::new(db, &config, Arc::new(clock));
        let user_dev: DeviceName = "mac-local".parse().unwrap();
        let user_token = state.create_device(&user_dev, true).unwrap();
        let remote_dev: DeviceName = "runner-linux-1".parse().unwrap();
        let _ = state.create_device(&remote_dev, false).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let app = board_server::app(state.clone());
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let srv = Self {
            _dir: dir,
            url,
            state,
            user_token,
            handle,
        };
        let client = srv.client();
        client
            .register(&RegisterAgent {
                id: "t3:user-session".parse().unwrap(),
                runtime: Runtime::Antigravity,
                device: user_dev,
                role: AgentRole::Worker,
                title: Some("Local session".into()),
                repo: Some("github.com/org/repo".into()),
                branch: Some("feat/local".into()),
                cwd: Some("/tmp/local-work".into()),
                native_ids: BTreeMap::from([(
                    "t3_link".into(),
                    "https://t3.example/threads/env-1/local".into(),
                )]),
                forked_from: None,
            })
            .await
            .unwrap();
        client
            .create_board(&CreateBoard {
                slug: "factory".parse().unwrap(),
                title: "Factory".into(),
                goal: String::new(),
                repos: vec!["github.com/org/repo".into()],
                autopilot: true,
                home_devices: vec![],
                temporary: false,
            })
            .await
            .unwrap();
        srv
    }

    fn client(&self) -> board::client::Client {
        board::client::Client::new(
            &self.url,
            self.user_token.clone(),
            Some("t3:user-session".parse().unwrap()),
            None,
        )
        .unwrap()
    }
}

#[tokio::test]
async fn cli_runs_logs_steer_cancel_and_attach_work_end_to_end() {
    let srv = RealServer::start().await;
    let client = srv.client();

    let task = client
        .create_task(&CreateTask {
            board: "factory".parse().unwrap(),
            agent: "t3:user-session".parse().unwrap(),
            title: "Build cockpit".into(),
            body: "Test body".into(),
            acceptance: "Tests pass".into(),
            parent: None,
            blockers: vec![],
            priority: Priority::P1,
            auto: Some(true),
            not_before: None,
            claim: false,
            repo: Some("github.com/org/repo".into()),
            weight: None,
            resource_units: None,
            requires: vec![],
            tolerations: vec![],
            conflict_keys: vec![],
            affinity: None,
            preferred_runner: None,
            rank: None,
            rank_top: false,
            before: None,
            after: None,
            origin: None,
            size: None,
            expedited: None,
        })
        .await
        .unwrap();

    // Seed one local managed run and one remote managed run.
    let local_run = client
        .create_run(&CreateRun {
            task_id: Some(task.id.clone()),
            board: Some("factory".parse().unwrap()),
            kind: RunKind::Worker,
            origin: RunOrigin::Managed,
            device: "mac-local".parse().unwrap(),
            runner: "acp".into(),
            agent_id: None,
            state: Some(RunState::Running),
            lease_ttl_secs: Some(300),
            lease_expires_at: None,
            worktree_branch: Some("feat/local-run".into()),
            cwd: Some("/tmp/server-cwd".into()),
            title: Some("Local ACP run".into()),
            attach_hint: None,
            cost_units: 2,
            policy_version: Some("v1-default".into()),
            conflict_keys: vec![],
        })
        .await
        .unwrap();

    let remote_run = client
        .create_run(&CreateRun {
            task_id: Some(task.id.clone()),
            board: Some("factory".parse().unwrap()),
            kind: RunKind::Review,
            origin: RunOrigin::Managed,
            device: "runner-linux-1".parse().unwrap(),
            runner: "acp".into(),
            agent_id: None,
            state: Some(RunState::Running),
            lease_ttl_secs: Some(300),
            lease_expires_at: None,
            worktree_branch: Some("feat/remote-review".into()),
            cwd: Some("/var/lib/board/worktrees/factory/remote".into()),
            title: Some("Remote review run".into()),
            attach_hint: Some("https://t3.example/threads/env-1/remote".into()),
            cost_units: 1,
            policy_version: Some("v1-default".into()),
            conflict_keys: vec![],
        })
        .await
        .unwrap();

    // Push live events to local_run.
    client
        .push_run_events(
            &local_run.id,
            &PushRunEvents {
                events: vec![
                    NewRunEvent {
                        seq: 1,
                        at: Some(t0()),
                        kind: RunEventKind::Phase,
                        summary: "started exploration".into(),
                        meta: None,
                    },
                    NewRunEvent {
                        seq: 2,
                        at: Some(t0()),
                        kind: RunEventKind::Tool,
                        summary: "cargo test --workspace".into(),
                        meta: None,
                    },
                ],
            },
        )
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let home = home_for_device(dir.path(), &srv.url, "mac-local");
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();

    // Seed local daemon.db with a worktree_path + session_id for local_run so `board attach` resolves it.
    let local_db = LocalDb::open_for_home(&board::config::Home::at(home.clone())).unwrap();
    local_db
        .upsert_local_run(&LocalRunRecord {
            run_id: local_run.id.to_string(),
            task_id: Some(task.id.clone()),
            board: "factory".parse().unwrap(),
            kind: "worker".into(),
            runner: "acp".into(),
            session_id: Some("acp-sess-123".into()),
            worktree_path: Some(PathBuf::from("/tmp/local-worktree-path")),
            branch: Some("feat/local-run".into()),
            cost_units: 2,
            pid: None,
            state: LocalRunState::Running,
            lease_expires_at: None,
            created_at: t0(),
            updated_at: t0(),
            meta_json: None,
        })
        .unwrap();

    let home_cl = home.clone();
    let cwd_cl = cwd.clone();
    let token_cl = srv.user_token.clone();
    let run_cli = move |args: Vec<String>| {
        let h = home_cl.clone();
        let c = cwd_cl.clone();
        let tok = token_cl.clone();
        tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            board(&h, &c, &[("BOARD_TOKEN", tok.as_str())], &refs, "")
        })
    };

    // 1. `board runs --json` + filters (`--active`, `--origin managed`, `--device runner-linux-1`).
    let out = run_cli(vec!["runs".into(), "--active".into(), "--json".into()])
        .await
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let runs: Vec<board_proto::Run> = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(
        runs.len() >= 2,
        "expected managed + interactive runs: {runs:?}"
    );

    let out = run_cli(vec![
        "runs".into(),
        "--origin".into(),
        "managed".into(),
        "--device".into(),
        "runner-linux-1".into(),
    ])
    .await
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(remote_run.id.as_str()), "{text}");
    assert!(text.contains("feat/remote-review"), "{text}");
    assert!(!text.contains(local_run.id.as_str()), "{text}");

    // 2. `board logs <id> --json` and text mode.
    let out = run_cli(vec![
        "logs".into(),
        local_run.id.to_string(),
        "--json".into(),
    ])
    .await
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let events: Vec<RunEventRecord> = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].summary, "started exploration");
    assert_eq!(events[1].kind, RunEventKind::Tool);

    let out = run_cli(vec!["logs".into(), local_run.id.to_string()])
        .await
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let log_text = stdout(&out);
    assert!(
        log_text.contains("#1 ") && log_text.contains("[phase] started exploration"),
        "{log_text}"
    );

    // 3. `board steer <id> "msg" --json`
    let out = run_cli(vec![
        "steer".into(),
        local_run.id.to_string(),
        "check error paths first".into(),
        "--json".into(),
    ])
    .await
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let steer_resp: board_proto::SteerRunResponse = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(steer_resp.run_id, local_run.id);
    assert_eq!(steer_resp.device.as_str(), "mac-local");

    // 4. `board attach <local-id> --dry-run --json` resolves local daemon.db worktree + session.
    let out = run_cli(vec![
        "attach".into(),
        local_run.id.to_string(),
        "--dry-run".into(),
        "--json".into(),
    ])
    .await
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let local_plan: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(local_plan["local"], true);
    assert_eq!(local_plan["workdir"], "/tmp/local-worktree-path");
    assert_eq!(local_plan["session_id"], "acp-sess-123");
    assert!(
        local_plan["command"]
            .as_str()
            .unwrap()
            .contains("/tmp/local-worktree-path"),
        "{local_plan}"
    );

    // 5. `board attach <remote-id> --dry-run --json` prints SSH + fresh local git worktree checkout + t3_link.
    let out = run_cli(vec![
        "attach".into(),
        remote_run.id.to_string(),
        "--dry-run".into(),
        "--json".into(),
    ])
    .await
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let remote_plan: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(remote_plan["local"], false);
    assert!(
        remote_plan["ssh_command"]
            .as_str()
            .unwrap()
            .contains("ssh -t runner-linux-1")
            && remote_plan["ssh_command"]
                .as_str()
                .unwrap()
                .contains("cd -- /var/lib/board/worktrees/factory/remote"),
        "{remote_plan}"
    );
    assert!(
        remote_plan["checkout_command"]
            .as_str()
            .unwrap()
            .contains("git fetch origin -- feat/remote-review && git worktree add --"),
        "{remote_plan}"
    );
    assert_eq!(
        remote_plan["t3_link"],
        "https://t3.example/threads/env-1/remote"
    );

    // 6. `board cancel <id> --reason "stop" --json`
    let out = run_cli(vec![
        "cancel".into(),
        remote_run.id.to_string(),
        "--reason".into(),
        "user stopped review".into(),
        "--json".into(),
    ])
    .await
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let cancel_resp: board_proto::CancelRunResponse = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(cancel_resp.run_id, remote_run.id);
    assert_eq!(cancel_resp.device.as_str(), "runner-linux-1");
    assert!(cancel_resp.wake_id.is_some());

    // 7. `board logs <id> -f` tails live SSE updates and exits cleanly once run terminates + prints transcript_uri.
    let follow_task = run_cli(vec![
        "logs".into(),
        local_run.id.to_string(),
        "--follow".into(),
        "--after-seq".into(),
        "1".into(),
    ]);
    tokio::time::sleep(Duration::from_millis(150)).await;
    client
        .update_run(
            &local_run.id,
            &UpdateRun {
                state: Some(RunState::Succeeded),
                transcript_uri: Some(Some("s3://archive-bucket/runs/factory/local.jsonl".into())),
                ..UpdateRun::default()
            },
        )
        .await
        .unwrap();
    client
        .push_run_events(
            &local_run.id,
            &PushRunEvents {
                events: vec![NewRunEvent {
                    seq: 3,
                    at: Some(t0()),
                    kind: RunEventKind::Done,
                    summary: "finished cleanly".into(),
                    meta: None,
                }],
            },
        )
        .await
        .unwrap();

    let follow_out = tokio::time::timeout(Duration::from_secs(5), follow_task)
        .await
        .expect("board logs -f exits after terminal event")
        .unwrap();
    assert!(follow_out.status.success(), "{}", stderr(&follow_out));
    let follow_text = stdout(&follow_out);
    assert!(
        follow_text.contains("#2 ") && follow_text.contains("cargo test --workspace"),
        "{follow_text}"
    );
    assert!(
        follow_text.contains("#3 ") && follow_text.contains("[done] finished cleanly"),
        "{follow_text}"
    );
    assert!(
        follow_text.contains("transcript: s3://archive-bucket/runs/factory/local.jsonl"),
        "{follow_text}"
    );
}
