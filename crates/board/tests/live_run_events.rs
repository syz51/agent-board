//! End-to-end daemon + server + client integration tests for Slice 4.2 (`agent-board-685`):
//! - Live event flushing (`Runner::poll_events` -> `POST /v1/runs/{id}/events`) with
//!   `last_event_seq` persistence across daemon restarts and live SSE streaming via
//!   `Client::stream_run_events`.
//! - Sticky `steer` and `cancel` wake execution (`POST /v1/runs/{id}/steer`,
//!   `POST /v1/runs/{id}/cancel` -> `Runner::send` / `Runner::cancel`, state transition,
//!   resource ledger + worktree cleanup, and `POST /v1/wakes/{id}/ack`).
//! - Transcript archival via `[archive]` command + offline `telemetry_outbox` exponential
//!   backoff retry + startup/reconnect reconciliation of `orphaned` runs.
//! - Regression test for `agent-board-689` preserving passive `hook.sock` rows in
//!   `resource_ledger` across `Daemon::sync_resource_ledger`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use board::client::Client;
use board::config::{ArchiveConfig, Config, DispatchMode, Home};
use board::daemon::Daemon;
use board::identity::RepoInfo;
use board::local_db::{LocalDb, ResourceEntry, ResourceOrigin};
use board::ops::Ctx;
use board::runner::{FakeRunner, RunEventKind, Runner, RunnerRegistry, RunnerState, RunnerStatus};
use board::t3::{ListedThread, NewWorktree, RecentMessages, T3, ThreadInfo};
use board_proto::{
    AgentRole, BoardSlug, CancelRunRequest, CreateBoard, CreateRun, CreateTask, DeviceName,
    Priority, RegisterAgent, RunEventKind as ProtoRunEventKind, RunEventRecord, RunKind, RunOrigin,
    RunState, Runtime, SteerRunRequest, Timestamp, UpdateBoard, UpdateRun,
};
use board_server::{AppState, Clock, Db, ManualClock, ServerConfig};
use chrono::TimeDelta;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

const REPO: &str = "github.com/org/repo";

fn t0() -> Timestamp {
    "2026-11-24T12:00:00Z".parse().unwrap()
}

#[derive(Clone, Default)]
struct FakeT3 {
    running: Vec<String>,
}

impl T3 for FakeT3 {
    async fn start_thread(
        &self,
        _: &Path,
        _: &str,
        _: &str,
        _: Option<&NewWorktree>,
        _: Option<&board::config::ThreadModel>,
    ) -> Result<String> {
        anyhow::bail!("unexpected T3 start_thread")
    }

    async fn send(&self, _: &str, _: &str) -> Result<()> {
        anyhow::bail!("unexpected T3 send")
    }

    async fn thread_info(&self, _: &str) -> Result<Option<ThreadInfo>> {
        Ok(None)
    }

    async fn recent_messages(&self, _: &str) -> Result<Option<RecentMessages>> {
        Ok(None)
    }

    async fn thread_link(&self, _: &str) -> Result<Option<String>> {
        Ok(None)
    }

    async fn threads_titled(&self, _: &Path, _: &str) -> Result<Vec<ListedThread>> {
        Ok(Vec::new())
    }

    async fn running_threads(&self, _: &Path) -> Result<Vec<String>> {
        Ok(self.running.clone())
    }
}

struct TestServer {
    _dir: tempfile::TempDir,
    url: String,
    state: AppState,
    clock: ManualClock,
    user_token: String,
    server_task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.state.shutdown();
        self.server_task.abort();
    }
}

impl TestServer {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("board.db");
        let db = Db::open(&db_path).unwrap();
        let clock = ManualClock::new(t0());
        let config = ServerConfig::new(db_path);
        let state = AppState::new(db, &config, Arc::new(clock.clone()));
        let user_dev: DeviceName = "admin-user".parse().unwrap();
        let user_token = state.create_device(&user_dev, true).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let app = board_server::app(state.clone());
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let srv = Self {
            _dir: dir,
            url,
            state,
            clock,
            user_token,
            server_task,
        };
        srv.user_client()
            .register(&RegisterAgent {
                id: "t3:admin".parse().unwrap(),
                runtime: Runtime::Antigravity,
                device: user_dev,
                role: AgentRole::Coordinator,
                title: Some("admin".into()),
                repo: None,
                branch: None,
                cwd: None,
                native_ids: BTreeMap::new(),
                forked_from: None,
            })
            .await
            .unwrap();
        srv
    }

    fn now(&self) -> Timestamp {
        self.clock.now()
    }

    fn advance(&self, by: TimeDelta) -> Timestamp {
        self.clock.advance(by);
        self.clock.now()
    }

    fn create_device(&self, name: &str) -> String {
        let dev: DeviceName = name.parse().unwrap();
        self.state.create_device(&dev, false).unwrap()
    }

    fn user_client(&self) -> Client {
        Client::new(
            &self.url,
            self.user_token.clone(),
            Some("t3:admin".parse().unwrap()),
            None,
        )
        .unwrap()
    }

    async fn create_autopilot_board(&self, slug: &str) -> BoardSlug {
        let user = self.user_client();
        let board = user
            .create_board(&CreateBoard {
                slug: slug.parse().unwrap(),
                title: format!("Board {slug}"),
                goal: String::new(),
                repos: vec![REPO.to_owned()],
                autopilot: false,
                home_devices: Vec::new(),
                temporary: false,
            })
            .await
            .unwrap();
        user.update_board(
            &board.slug,
            &UpdateBoard {
                autopilot: Some(true),
                parallel_tasks: Some(4),
                ..UpdateBoard::default()
            },
        )
        .await
        .unwrap();
        board.slug
    }

    async fn create_ready_task(&self, board: &BoardSlug, title: &str) -> board_proto::TaskId {
        let user = self.user_client();
        user.create_task(&CreateTask {
            board: board.clone(),
            agent: "t3:admin".parse().unwrap(),
            title: title.to_owned(),
            body: format!("Task body for {title}"),
            acceptance: "tests pass".into(),
            priority: Priority::P1,
            parent: None,
            repo: Some(REPO.into()),
            blockers: Vec::new(),
            auto: Some(true),
            claim: false,
            not_before: None,
            weight: None,
            resource_units: None,
            requires: Vec::new(),
            tolerations: Vec::new(),
            affinity: None,
            conflict_keys: Vec::new(),
            preferred_runner: None,
            rank: None,
            rank_top: false,
            before: None,
            after: None,
            origin: None,
            size: None,
        })
        .await
        .unwrap()
        .id
    }
}

fn make_daemon(
    srv: &TestServer,
    home_dir: &Path,
    project_dir: &Path,
    device_name: &str,
    token: &str,
    runner: FakeRunner,
    archive: Option<ArchiveConfig>,
) -> Daemon<FakeT3> {
    let home = Home::at(home_dir.to_path_buf());
    std::fs::create_dir_all(home.root()).unwrap();
    let device: DeviceName = device_name.parse().unwrap();
    let mut config = Config {
        server: Some(srv.url.clone()),
        device: Some(device.clone()),
        dispatch_mode: DispatchMode::Pull,
        weight_units: Some(12),
        runners: vec![runner.kind().to_owned()],
        archive: archive.clone(),
        ..Config::default()
    };
    config
        .projects
        .insert(REPO.to_owned(), project_dir.to_path_buf());
    config.save(&home).unwrap();
    let client = Client::new(&srv.url, token.to_owned(), None, None).unwrap();
    let ctx = Ctx {
        home,
        config,
        device,
        client,
        repo: RepoInfo::default(),
        known: None,
    };
    let clock_ref = srv.clock.clone();
    let daemon_clock: Arc<dyn Fn() -> Timestamp + Send + Sync> = Arc::new(move || clock_ref.now());
    let registry = RunnerRegistry::new().with_runner(Arc::new(runner));
    let mut daemon = Daemon::new(ctx, FakeT3::default())
        .with_runner_registry(registry)
        .with_clock(daemon_clock)
        .with_default_run_limit(6);
    if let Some(cfg) = archive {
        daemon = daemon.with_archive_config(Some(cfg));
    }
    daemon
}

#[tokio::test]
async fn live_event_flushing_sse_stream_and_restart_resume() {
    let srv = TestServer::start().await;
    let board = srv.create_autopilot_board("alpha").await;
    let _task = srv.create_ready_task(&board, "Stream live events").await;

    let token = srv.create_device("mac-pro");
    let home_dir = tempfile::tempdir().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    let clock_ref = srv.clock.clone();
    let runner = FakeRunner::new("acp")
        .with_home(Home::at(home_dir.path().to_path_buf()))
        .with_clock(Arc::new(move || clock_ref.now()));

    let mut daemon = make_daemon(
        &srv,
        home_dir.path(),
        project_dir.path(),
        "mac-pro",
        &token,
        runner.clone(),
        None,
    );
    daemon.send_heartbeat(srv.now()).await.unwrap();
    daemon.reconcile(srv.now()).await.unwrap();

    // Locate the claimed managed run and its FakeRunner native handle.
    // Note: `FakeRunner::spawn` emits seq 1 ("spawned ..."), which `reconcile` already flushed.
    let runs = srv
        .user_client()
        .list_runs(&board_proto::RunFilter {
            origin: Some(RunOrigin::Managed),
            ..board_proto::RunFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    let run_id = runs[0].id.clone();
    let spawns = runner.spawns();
    assert_eq!(spawns.len(), 1);
    let native_id = spawns[0].handle.native_id.clone();

    // Push seq 2 & seq 3 (including oversized metadata that must be truncated below
    // `RUN_EVENT_META_BYTES_MAX` instead of wedging the run) and flush via daemon.
    runner.push_event(
        &native_id,
        RunEventKind::Phase,
        "reading repository",
        BTreeMap::from([("stage".into(), "explore".into())]),
    );
    srv.advance(TimeDelta::seconds(5));
    runner.push_event(
        &native_id,
        RunEventKind::Tool,
        "cargo check",
        BTreeMap::from([("huge".into(), "x".repeat(20_000))]),
    );
    let pushed = daemon.flush_run_events(srv.now()).await.unwrap();
    assert_eq!(pushed, 2);

    // Repeating flush pushes 0 duplicates.
    assert_eq!(daemon.flush_run_events(srv.now()).await.unwrap(), 0);

    // Open a real SSE stream via `Client::stream_run_events` resuming after seq 2.
    let user = srv.user_client();
    let resp = user.stream_run_events(&run_id, Some(2)).await.unwrap();
    let mut sse = resp.bytes_stream().eventsource();
    let first_frame = tokio::time::timeout(Duration::from_secs(5), sse.next())
        .await
        .expect("backlog SSE event arrives")
        .expect("stream open")
        .unwrap();
    assert_eq!(first_frame.id, "3");
    let first_rec: RunEventRecord = serde_json::from_str(&first_frame.data).unwrap();
    assert_eq!(first_rec.seq, 3);
    assert_eq!(first_rec.kind, ProtoRunEventKind::Tool);
    assert_eq!(first_rec.summary, "cargo check");

    // Simulate daemon restart over the same `$BOARD_HOME`: `last_event_seq` reloads from
    // `daemon.db`, so only seq 4 is pushed and delivered live over SSE.
    drop(daemon);
    let mut restarted = make_daemon(
        &srv,
        home_dir.path(),
        project_dir.path(),
        "mac-pro",
        &token,
        runner.clone(),
        None,
    );
    srv.advance(TimeDelta::seconds(5));
    runner.push_event(
        &native_id,
        RunEventKind::Pr,
        "opened PR #7",
        BTreeMap::new(),
    );
    assert_eq!(restarted.flush_run_events(srv.now()).await.unwrap(), 1);

    let live_frame = tokio::time::timeout(Duration::from_secs(5), sse.next())
        .await
        .expect("live SSE event arrives")
        .expect("stream open")
        .unwrap();
    assert_eq!(live_frame.id, "4");
    let live_rec: RunEventRecord = serde_json::from_str(&live_frame.data).unwrap();
    assert_eq!(live_rec.seq, 4);
    assert_eq!(live_rec.kind, ProtoRunEventKind::Pr);
    assert_eq!(live_rec.summary, "opened PR #7");

    let stored = user.list_run_events(&run_id, None, None).await.unwrap();
    assert_eq!(
        stored.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
}

#[tokio::test]
async fn sticky_steer_and_cancel_wakes_execute_on_runner_and_clean_up() {
    let srv = TestServer::start().await;
    let board = srv.create_autopilot_board("beta").await;
    let _task = srv.create_ready_task(&board, "Steerable task").await;

    let token = srv.create_device("mac-pro");
    let home_dir = tempfile::tempdir().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    let home = Home::at(home_dir.path().to_path_buf());
    let clock_ref = srv.clock.clone();
    let runner = FakeRunner::new("acp")
        .with_home(home.clone())
        .with_clock(Arc::new(move || clock_ref.now()));

    let mut daemon = make_daemon(
        &srv,
        home_dir.path(),
        project_dir.path(),
        "mac-pro",
        &token,
        runner.clone(),
        None,
    );
    daemon.send_heartbeat(srv.now()).await.unwrap();
    daemon.reconcile(srv.now()).await.unwrap();

    let user = srv.user_client();
    let runs = user
        .list_runs(&board_proto::RunFilter {
            origin: Some(RunOrigin::Managed),
            ..board_proto::RunFilter::default()
        })
        .await
        .unwrap();
    let run_id = runs[0].id.clone();
    let native_id = runner.spawns()[0].handle.native_id.clone();

    // 1. Steer the active run via `POST /v1/runs/{id}/steer`.
    let steer_resp = user
        .steer_run(
            &run_id,
            &SteerRunRequest {
                message: "focus on error handling first".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(steer_resp.run_id, run_id);
    assert_eq!(steer_resp.device.as_str(), "mac-pro");

    let delivered = daemon.poll_sticky_wakes(srv.now()).await.unwrap();
    assert_eq!(delivered, 1);

    let sends = runner.sends();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].handle.native_id, native_id);
    assert!(
        sends[0]
            .message
            .text
            .contains("focus on error handling first")
    );
    assert!(sends[0].message.digest.is_some());

    // Queue is now empty (wake acked).
    let remaining = user
        .list_device_wakes(&"mac-pro".parse().unwrap())
        .await
        .unwrap();
    assert!(remaining.is_empty(), "{remaining:?}");

    // 2. Cancel the active run via `POST /v1/runs/{id}/cancel`.
    runner.push_event(
        &native_id,
        RunEventKind::Note,
        "last note before cancel",
        BTreeMap::new(),
    );
    let cancel_resp = user
        .cancel_run(
            &run_id,
            &CancelRunRequest {
                reason: Some("user stopped run".into()),
            },
        )
        .await
        .unwrap();
    assert!(cancel_resp.wake_id.is_some());

    let delivered = daemon.poll_sticky_wakes(srv.now()).await.unwrap();
    assert_eq!(delivered, 1);

    let cancels = runner.cancels();
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0].handle.native_id, native_id);
    assert_eq!(cancels[0].reason, "user stopped run");

    // Final event AND `RunEventKind::Cancelled` emitted by `runner.cancel` were both flushed
    // before removing the run, server run transitioned to `cancelled`, `resource_ledger` row
    // was removed, and wake was acked.
    let updated = user.get_run(&run_id).await.unwrap();
    assert_eq!(updated.state, RunState::Cancelled);
    let events = user.list_run_events(&run_id, None, None).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.summary == "last note before cancel"),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == ProtoRunEventKind::Cancelled),
        "expected RunEventKind::Cancelled emitted by runner.cancel to be flushed: {events:?}"
    );
    let local_db = LocalDb::open_for_home(&home).unwrap();
    assert!(
        local_db
            .list_resources()
            .unwrap()
            .iter()
            .all(|r| r.entry_id != format!("run:{run_id}"))
    );
    let remaining = user
        .list_device_wakes(&"mac-pro".parse().unwrap())
        .await
        .unwrap();
    assert!(remaining.is_empty(), "{remaining:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn transcript_archival_retries_offline_failures_and_reconciles_orphans() {
    use std::os::unix::fs::PermissionsExt as _;

    let srv = TestServer::start().await;
    let board = srv.create_autopilot_board("gamma").await;
    let _task = srv.create_ready_task(&board, "Archive transcript").await;

    let token = srv.create_device("mac-pro");
    let home_dir = tempfile::tempdir().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    let home = Home::at(home_dir.path().to_path_buf());
    let script_dir = tempfile::tempdir().unwrap();
    let marker_path = script_dir.path().join("attempt_marker");
    let script_path = script_dir.path().join("archive-uploader.sh");

    // Test archive script: fails on first invocation to exercise `telemetry_outbox`
    // exponential backoff retry, then succeeds on subsequent invocations printing URI.
    let script = format!(
        "#!/bin/sh\n\
         if [ ! -f \"{marker}\" ]; then\n\
           touch \"{marker}\"\n\
           echo \"transient storage error\" >&2\n\
           exit 1\n\
         fi\n\
         if [ ! -f \"$1\" ]; then\n\
           echo \"missing file $1\" >&2\n\
           exit 2\n\
         fi\n\
         echo \"s3://archive-bucket/runs/$3/$2.jsonl\"\n",
        marker = marker_path.display(),
    );
    std::fs::write(&script_path, script).unwrap();
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let archive_cfg = ArchiveConfig {
        enabled: true,
        command: Some(vec![
            script_path.to_string_lossy().into_owned(),
            "{file}".into(),
            "{run_id}".into(),
            "{board}".into(),
        ]),
        uri_prefix: Some("s3://archive-bucket/runs/".into()),
    };

    let clock_ref = srv.clock.clone();
    let runner = FakeRunner::new("acp")
        .with_home(home.clone())
        .with_clock(Arc::new(move || clock_ref.now()));
    let mut daemon = make_daemon(
        &srv,
        home_dir.path(),
        project_dir.path(),
        "mac-pro",
        &token,
        runner.clone(),
        Some(archive_cfg),
    );
    daemon.send_heartbeat(srv.now()).await.unwrap();
    daemon.reconcile(srv.now()).await.unwrap();

    let user = srv.user_client();
    let runs = user
        .list_runs(&board_proto::RunFilter {
            origin: Some(RunOrigin::Managed),
            ..board_proto::RunFilter::default()
        })
        .await
        .unwrap();
    let run_id = runs[0].id.clone();
    let native_id = runner.spawns()[0].handle.native_id.clone();

    // Write local transcript file where ACP/runner stores it.
    let transcript_file = board::state::transcript_path(&home, run_id.as_str());
    std::fs::create_dir_all(transcript_file.parent().unwrap()).unwrap();
    let transcript_bytes = b"{\"seq\":1,\"kind\":\"phase\",\"summary\":\"done\"}\n";
    std::fs::write(&transcript_file, transcript_bytes).unwrap();
    let expected_sha = {
        let mut h = Sha256::new();
        h.update(transcript_bytes);
        let digest = h.finalize();
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };

    // Mark runner ended (`RunnerState::Gone`, `runs: Some(1)`) -> daemon observes terminal
    // `Succeeded` transition, enqueues archive, attempt 1 fails and backs off in
    // `telemetry_outbox`.
    runner.set_status(
        &native_id,
        RunnerStatus {
            state: RunnerState::Gone,
            runnable: false,
            busy: false,
            runtime: Some(Runtime::Antigravity),
            workdir: Some(project_dir.path().to_path_buf()),
            runs: Some(1),
        },
    );
    daemon.renew_runs(srv.now()).await.unwrap();
    let after_first = user.get_run(&run_id).await.unwrap();
    assert_eq!(after_first.state, RunState::Succeeded);
    assert!(after_first.transcript_uri.is_none());

    // Outbox has 1 pending item with retry scheduled in the future (30s backoff).
    let local_db = LocalDb::open_for_home(&home).unwrap();
    let queued = local_db
        .list_telemetry_by_kind("transcript_archive")
        .unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].attempts, 1);

    // Before backoff elapses, flush is a no-op.
    assert_eq!(
        daemon
            .flush_telemetry_outbox(srv.advance(TimeDelta::seconds(10)))
            .await
            .unwrap(),
        0
    );

    // Once backoff elapses, retry succeeds and records `transcript_uri` + `transcript_sha256`.
    assert_eq!(
        daemon
            .flush_telemetry_outbox(srv.advance(TimeDelta::seconds(25)))
            .await
            .unwrap(),
        1
    );
    let archived = user.get_run(&run_id).await.unwrap();
    assert_eq!(
        archived.transcript_uri.as_deref(),
        Some(format!("s3://archive-bucket/runs/gamma/{run_id}.jsonl").as_str())
    );
    assert_eq!(
        archived.transcript_sha256.as_deref(),
        Some(expected_sha.as_str())
    );
    assert!(
        local_db
            .list_telemetry_by_kind("transcript_archive")
            .unwrap()
            .is_empty()
    );

    // Best-effort orphaned run archival on startup/reconnect: create a server-side `orphaned`
    // run with a local partial transcript file and run `reconcile_orphaned_transcripts`.
    let orphan = user
        .create_run(&CreateRun {
            board: Some(board.clone()),
            task_id: None,
            kind: RunKind::Worker,
            origin: RunOrigin::Managed,
            device: "mac-pro".parse().unwrap(),
            runner: "acp".into(),
            agent_id: None,
            worktree_branch: None,
            cwd: None,
            title: Some("orphaned run".into()),
            attach_hint: None,
            cost_units: 2,
            state: Some(RunState::Running),
            policy_version: None,
            lease_ttl_secs: None,
            lease_expires_at: None,
            conflict_keys: Vec::new(),
        })
        .await
        .unwrap();
    user.update_run(
        &orphan.id,
        &UpdateRun {
            state: Some(RunState::Orphaned),
            reason: Some("node went offline".into()),
            ..UpdateRun::default()
        },
    )
    .await
    .unwrap();
    let orphan_file = board::state::transcript_path(&home, orphan.id.as_str());
    let orphan_bytes = b"{\"seq\":1,\"kind\":\"note\",\"summary\":\"partial before crash\"}\n";
    std::fs::write(&orphan_file, orphan_bytes).unwrap();

    let enqueued = daemon.reconcile_orphaned_transcripts(srv.now()).await;
    assert_eq!(enqueued, 1);
    assert_eq!(daemon.flush_telemetry_outbox(srv.now()).await.unwrap(), 1);
    let archived_orphan = user.get_run(&orphan.id).await.unwrap();
    assert_eq!(
        archived_orphan.transcript_uri.as_deref(),
        Some(format!("s3://archive-bucket/runs/gamma/{}.jsonl", orphan.id).as_str())
    );
    assert!(archived_orphan.transcript_sha256.is_some());
}

#[tokio::test]
async fn sync_resource_ledger_preserves_passive_hook_rows_for_t3_and_non_t3_daemons() {
    let srv = TestServer::start().await;
    let token = srv.create_device("mac-pro");
    let home_dir = tempfile::tempdir().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    let home1 = Home::at(home_dir.path().to_path_buf());

    let seed_passive = |db: &LocalDb, now: Timestamp| {
        for (entry_id, agent_id) in [
            ("interactive:claude:c1", Some("claude:c1")),
            ("interactive:codex:x1", Some("codex:x1")),
            ("interactive:opencode:o1", Some("opencode:o1")),
            ("interactive:antigravity:a1", Some("antigravity:a1")),
            ("interactive:t3:stale-thread", Some("t3:stale-thread")),
            ("interactive:legacy-unprefixed", None),
        ] {
            db.upsert_resource(&ResourceEntry {
                entry_id: entry_id.to_owned(),
                origin: ResourceOrigin::Interactive,
                agent_id: agent_id.map(str::to_owned),
                run_id: None,
                cost_units: 2,
                updated_at: now,
            })
            .unwrap();
        }
    };

    // Case 1: Non-T3 (`acp`) daemon — `reconcile` runs `read_running` (which sets
    // `self.running = Some(Running::default())`) followed by `sync_resource_ledger`.
    // Must NEVER wipe passive `hook.sock` rows!
    let acp_runner = FakeRunner::new("acp").with_home(home1.clone());
    let mut acp_daemon = make_daemon(
        &srv,
        home_dir.path(),
        project_dir.path(),
        "mac-pro",
        &token,
        acp_runner,
        None,
    );
    let db1 = LocalDb::open_for_home(&home1).unwrap();
    seed_passive(&db1, srv.now());
    acp_daemon.reconcile(srv.now()).await.unwrap();
    let acp_ids: Vec<String> = db1
        .list_resources()
        .unwrap()
        .into_iter()
        .map(|r| r.entry_id)
        .collect();
    assert!(
        acp_ids.contains(&"interactive:claude:c1".to_owned()),
        "{acp_ids:?}"
    );
    assert!(
        acp_ids.contains(&"interactive:codex:x1".to_owned()),
        "{acp_ids:?}"
    );
    assert!(
        acp_ids.contains(&"interactive:opencode:o1".to_owned()),
        "{acp_ids:?}"
    );
    assert!(
        acp_ids.contains(&"interactive:antigravity:a1".to_owned()),
        "{acp_ids:?}"
    );
    assert!(
        acp_ids.contains(&"interactive:t3:stale-thread".to_owned()),
        "{acp_ids:?}"
    );

    // Case 2: T3 daemon — reconciles T3-scraped running turns under canonical
    // `interactive:t3:<thread>` (dropping stale T3/legacy rows) while preserving ALL passive
    // non-T3 `hook.sock` rows!
    let home_dir2 = tempfile::tempdir().unwrap();
    let home2 = Home::at(home_dir2.path().to_path_buf());
    std::fs::create_dir_all(home2.root()).unwrap();
    let device: DeviceName = "mac-pro".parse().unwrap();
    let mut config2 = Config {
        server: Some(srv.url.clone()),
        device: Some(device.clone()),
        dispatch_mode: DispatchMode::Pull,
        weight_units: Some(12),
        runners: vec!["t3".into()],
        ..Config::default()
    };
    config2
        .projects
        .insert(REPO.to_owned(), project_dir.path().to_path_buf());
    config2.save(&home2).unwrap();
    let client2 = Client::new(&srv.url, token.clone(), None, None).unwrap();
    let ctx2 = Ctx {
        home: home2.clone(),
        config: config2,
        device,
        client: client2,
        repo: RepoInfo::default(),
        known: None,
    };
    let mut t3_daemon = Daemon::new(
        ctx2,
        FakeT3 {
            running: vec!["live-thread-9".into()],
        },
    )
    .with_default_run_limit(6);
    let db2 = LocalDb::open_for_home(&home2).unwrap();
    seed_passive(&db2, srv.now());
    t3_daemon.reconcile(srv.now()).await.unwrap();
    let t3_ids: Vec<String> = db2
        .list_resources()
        .unwrap()
        .into_iter()
        .map(|r| r.entry_id)
        .collect();
    assert_eq!(
        t3_ids,
        vec![
            "interactive:antigravity:a1",
            "interactive:claude:c1",
            "interactive:codex:x1",
            "interactive:opencode:o1",
            "interactive:t3:live-thread-9",
        ]
    );
}
