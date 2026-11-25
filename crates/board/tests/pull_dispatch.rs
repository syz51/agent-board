//! Multi-device pull-dispatch integration tests (`agent-board-681`, Slice 2.2).
//!
//! Spins up a real in-process `board-server` (`AppState` + `ManualClock` + SQLite WAL +
//! `axum::serve`) with two concurrent daemons (`mac-pro` with `weight_units=12`, `mac-air`
//! with `weight_units=4`) backed by `FakeRunner`, `FakeGh`, `LocalDb`, and an injectable clock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use board::client::Client;
use board::config::{Config, DispatchMode, Home};
use board::daemon::Daemon;
use board::gh::{Checks, Gh, MergeState, PrState, PullRequest, ReviewDecision, Updated};
use board::identity::RepoInfo;
use board::local_db::LocalRunState;
use board::ops::Ctx;
use board::runner::{FakeRunner, RunnerRegistry, RunnerState, RunnerStatus};
use board::t3::{ListedThread, NewWorktree, RecentMessages, T3, ThreadInfo};
use board_proto::{
    AddNote, AgentId, AgentRole, AgentState, AgentStateUpdate, BoardSlug, CreateAsk, CreateBoard,
    CreateRun, CreateTask, DeviceName, NoteKind, Priority, RegisterAgent, RunFilter, RunKind,
    RunOrigin, RunState, Runtime, TaskId, TaskStatus, Timestamp, UpdateBoard, UpdateRun,
    UpdateTask,
};
use board_server::{AppState, Clock, Db, ManualClock, ServerConfig};
use chrono::TimeDelta;
use futures_util::future::BoxFuture;
use tokio::net::TcpListener;

const REPO: &str = "github.com/org/repo";
const MERGE_COMMIT: &str = "abc123def456";

fn t0() -> Timestamp {
    "2026-11-24T12:00:00Z".parse().unwrap()
}

/// No-op T3 placeholder; pull-dispatch tests use `FakeRunner` in `RunnerRegistry`.
#[derive(Clone, Default)]
struct NoopT3;

impl T3 for NoopT3 {
    async fn start_thread(
        &self,
        _: &Path,
        _: &str,
        _: &str,
        _: Option<&NewWorktree>,
        _: Option<&board::config::ThreadModel>,
    ) -> Result<String> {
        anyhow::bail!("unexpected T3 start_thread call in FakeRunner pull test")
    }

    async fn send(&self, _: &str, _: &str) -> Result<()> {
        anyhow::bail!("unexpected T3 send call in FakeRunner pull test")
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
        Ok(Vec::new())
    }
}

/// Scriptable `Gh` double for testing zero-LLM `DeterministicRunner` merge flows.
#[derive(Clone, Default)]
struct FakeGh {
    prs: Arc<Mutex<BTreeMap<String, PullRequest>>>,
    calls: Arc<Mutex<Vec<String>>>,
    conflicts: Arc<AtomicBool>,
    ready_fails: Arc<AtomicBool>,
}

impl FakeGh {
    fn put(&self, pr: PullRequest) {
        self.prs.lock().unwrap().insert(pr.url.clone(), pr);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Gh for FakeGh {
    fn view<'a>(&'a self, _: &'a Path, pr: &'a str) -> BoxFuture<'a, Result<PullRequest>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("view {pr}"));
            self.prs
                .lock()
                .unwrap()
                .get(pr)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("PR {pr} not found"))
        })
    }

    fn ready<'a>(&'a self, _: &'a Path, pr: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("ready {pr}"));
            if self.ready_fails.load(Ordering::SeqCst) {
                anyhow::bail!("gh pr ready failed: permission denied");
            }
            let mut prs = self.prs.lock().unwrap();
            let entry = prs.get_mut(pr).expect("pr present");
            entry.draft = false;
            if entry.merge_state == MergeState::Waits {
                entry.merge_state = MergeState::Clean;
            }
            Ok(())
        })
    }

    fn update_branch<'a>(&'a self, _: &'a Path, pr: &'a str) -> BoxFuture<'a, Result<Updated>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(format!("update-branch {pr}"));
            if self.conflicts.load(Ordering::SeqCst) {
                return Ok(Updated::Conflict);
            }
            let mut prs = self.prs.lock().unwrap();
            let entry = prs.get_mut(pr).expect("pr present");
            entry.merge_state = MergeState::Waits;
            entry.checks = Checks::Pending;
            Ok(Updated::Done)
        })
    }

    fn merge<'a>(&'a self, _: &'a Path, pr: &'a str, head: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(format!("merge {pr} {head}"));
            let mut prs = self.prs.lock().unwrap();
            let entry = prs.get_mut(pr).expect("pr present");
            entry.state = PrState::Merged;
            entry.merge_commit = Some(MERGE_COMMIT.to_owned());
            Ok(())
        })
    }
}

/// Real in-process `board-server` over a temp SQLite WAL DB and a shared `ManualClock`.
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

    fn device_client(&self, token: &str, agent: Option<AgentId>) -> Client {
        Client::new(&self.url, token.to_owned(), agent, None).unwrap()
    }

    /// Runs one real server sweeper tick (sweeping expired run leases, sticky wakes, etc.)
    /// using the current `ManualClock` time.
    async fn run_sweeper_once(&self) {
        let handle = board_server::spawn_sweeper(self.state.clone());
        // `tokio::time::interval` fires its first tick immediately; yield briefly so the
        // blocking DB transaction completes, then abort the periodic handle.
        tokio::time::sleep(Duration::from_millis(80)).await;
        handle.abort();
        let _ = handle.await;
    }

    async fn create_autopilot_board(&self, slug: &str, parallel_tasks: u32) -> BoardSlug {
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
                parallel_tasks: Some(parallel_tasks),
                ..UpdateBoard::default()
            },
        )
        .await
        .unwrap();
        board.slug
    }

    async fn add_ready_task(&self, board: &BoardSlug, title: &str) -> TaskId {
        let user = self.user_client();
        let task = user
            .create_task(&CreateTask {
                board: board.clone(),
                agent: "t3:admin".parse().unwrap(),
                title: title.to_owned(),
                body: String::new(),
                acceptance: String::new(),
                priority: Priority::P2,
                blockers: Vec::new(),
                auto: Some(true),
                parent: None,
                not_before: None,
                claim: false,
                repo: Some(REPO.to_owned()),
                weight: None,
                resource_units: None,
                requires: Vec::new(),
                tolerations: Vec::new(),
                conflict_keys: Vec::new(),
                affinity: None,
                preferred_runner: None,
                rank: None,
                rank_top: false,
                before: None,
                after: None,
                origin: None,
                size: None,
            })
            .await
            .unwrap();
        task.id
    }
}

fn update_task_base(agent: AgentId, version: i64) -> UpdateTask {
    UpdateTask {
        agent: Some(agent.into()),
        version,
        status: None,
        title: None,
        body: None,
        acceptance: None,
        priority: None,
        blockers: None,
        blocked_reason: None,
        auto: None,
        not_before: None,
        rank: None,
        owner: None,
        reason: None,
        repo: None,
        weight: None,
        resource_units: None,
        requires: None,
        tolerations: None,
        conflict_keys: None,
        affinity: None,
        preferred_runner: None,
        size: None,
        cooled: None,
        before: None,
        after: None,
    }
}

struct DeviceNode {
    _dir: tempfile::TempDir,
    project: PathBuf,
    token: String,
    runner: FakeRunner,
    daemon: Daemon<NoopT3>,
}

fn spawn_device_node(
    server: &TestServer,
    name: &str,
    weight_units: u32,
    reserve_runs: u32,
    shared_gh: Option<FakeGh>,
) -> DeviceNode {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("repo");
    std::fs::create_dir_all(&project).unwrap();
    let home = Home::at(dir.path().join("home"));
    let token = server.create_device(name);
    let mut projects = BTreeMap::new();
    projects.insert(REPO.to_owned(), project.clone());
    let config = Config {
        server: Some(server.url.clone()),
        device: Some(name.parse().unwrap()),
        weight_units: Some(weight_units),
        reserve_runs: Some(reserve_runs),
        projects,
        dispatch_mode: DispatchMode::Auto,
        ..Config::default()
    };
    let ctx = Ctx {
        home: home.clone(),
        device: name.parse().unwrap(),
        client: Client::new(&server.url, token.clone(), None, None).unwrap(),
        config,
        repo: RepoInfo::default(),
        known: None,
    };
    let clock_ref = server.clock.clone();
    let daemon_clock: Arc<dyn Fn() -> Timestamp + Send + Sync> = Arc::new(move || clock_ref.now());
    let runner = FakeRunner::new("acp")
        .with_home(home)
        .with_clock(Arc::clone(&daemon_clock));
    let gh = shared_gh.unwrap_or_default();
    let registry = RunnerRegistry::new().with_runner(Arc::new(runner.clone()));
    let daemon = Daemon::new(ctx, NoopT3)
        .with_runner_registry(registry)
        .with_gh(gh.clone())
        .with_clock(daemon_clock);
    DeviceNode {
        _dir: dir,
        project,
        token,
        runner,
        daemon,
    }
}

async fn make_reviewed_task(
    server: &TestServer,
    board: &BoardSlug,
    title: &str,
    pass_note: &str,
    device_name: &str,
    device_token: &str,
) -> TaskId {
    let worker_id: AgentId = "antigravity:worker-done".parse().unwrap();
    let reviewer_id: AgentId = "antigravity:reviewer-1".parse().unwrap();
    let worker_client = server.device_client(device_token, Some(worker_id.clone()));
    let reviewer_client = server.device_client(device_token, Some(reviewer_id.clone()));
    let dispatch_agent = AgentId::dispatch(&device_name.parse().unwrap());
    let dispatch_client = server.device_client(device_token, Some(dispatch_agent));

    worker_client
        .register(&RegisterAgent {
            id: worker_id.clone(),
            runtime: Runtime::Antigravity,
            device: device_name.parse().unwrap(),
            role: AgentRole::Worker,
            title: Some("worker".into()),
            repo: Some(REPO.into()),
            branch: Some("task/demo".into()),
            cwd: None,
            native_ids: BTreeMap::new(),
            forked_from: None,
        })
        .await
        .unwrap();
    reviewer_client
        .register(&RegisterAgent {
            id: reviewer_id.clone(),
            runtime: Runtime::Antigravity,
            device: device_name.parse().unwrap(),
            role: AgentRole::Worker,
            title: Some("reviewer".into()),
            repo: Some(REPO.into()),
            branch: None,
            cwd: None,
            native_ids: BTreeMap::new(),
            forked_from: None,
        })
        .await
        .unwrap();

    let task_id = server.add_ready_task(board, title).await;
    let claimed = worker_client.task_action(&task_id, "claim").await.unwrap();
    worker_client
        .update_task(
            &task_id,
            &UpdateTask {
                status: Some(TaskStatus::Review),
                ..update_task_base(worker_id.clone(), claimed.version)
            },
        )
        .await
        .unwrap();
    // Worker finishes its turn (idle) so merger does not defer on running owner.
    worker_client
        .set_state(
            &worker_id,
            &AgentStateUpdate {
                state: AgentState::Idle,
                paused_until: None,
                said: None,
            },
        )
        .await
        .unwrap();

    // Register a managed review run authored on `device_name` and post the standing pass note.
    let rev_run = dispatch_client
        .create_run(&CreateRun {
            task_id: Some(task_id.clone()),
            board: Some(board.clone()),
            kind: RunKind::Review,
            origin: RunOrigin::Managed,
            device: device_name.parse().unwrap(),
            runner: "acp".to_owned(),
            agent_id: Some(reviewer_id.clone()),
            state: Some(RunState::Running),
            lease_ttl_secs: Some(180),
            lease_expires_at: None,
            worktree_branch: None,
            cwd: None,
            title: Some(format!("review {task_id}")),
            attach_hint: None,
            cost_units: 2,
            policy_version: None,
            conflict_keys: Vec::new(),
        })
        .await
        .unwrap();
    reviewer_client
        .add_note(
            &task_id,
            &AddNote {
                agent: reviewer_id,
                kind: NoteKind::Decision,
                text: pass_note.to_owned(),
            },
        )
        .await
        .unwrap();
    dispatch_client
        .update_run(
            &rev_run.id,
            &UpdateRun {
                state: Some(RunState::Succeeded),
                ..UpdateRun::default()
            },
        )
        .await
        .unwrap();

    task_id
}

fn open_pr(url: &str, head_oid: &str, merge_state: MergeState, checks: Checks) -> PullRequest {
    PullRequest {
        url: url.to_owned(),
        state: PrState::Open,
        draft: false,
        base_ref: "main".to_owned(),
        head_ref: "task/branch".to_owned(),
        head_oid: head_oid.to_owned(),
        cross_repository: false,
        merge_commit: None,
        merge_state,
        review_decision: ReviewDecision::None,
        checks,
        passed_checks: Vec::new(),
    }
}

#[tokio::test]
async fn shared_board_spillover_across_mac_air_and_mac_pro_with_zero_board_lease() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 2).await;
    let t1 = server.add_ready_task(&board, "First task").await;
    let t2 = server.add_ready_task(&board, "Second task").await;

    // mac-air: weight_units=4, reserve_runs=1 (2 reserved) => 2 free units (1 standard worker).
    // mac-pro: weight_units=12, reserve_runs=1 (2 reserved) => 10 free units.
    let mut mac_air = spawn_device_node(&server, "mac-air", 4, 1, None);
    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, None);

    // Heartbeats register both devices' capabilities, runners (including "deterministic"), repos.
    assert!(mac_air.daemon.send_heartbeat(server.now()).await.unwrap());
    assert!(mac_pro.daemon.send_heartbeat(server.now()).await.unwrap());
    let devices = server.user_client().list_devices().await.unwrap();
    for dev_name in ["mac-air", "mac-pro"] {
        let entry = devices
            .iter()
            .find(|d| d.name.as_str() == dev_name)
            .expect("device listed");
        assert!(
            entry.runners.contains(&"deterministic".to_owned()),
            "{dev_name} must advertise deterministic runner: {:?}",
            entry.runners
        );
        assert!(
            entry.runners.contains(&"acp".to_owned()),
            "{dev_name} must advertise acp runner: {:?}",
            entry.runners
        );
    }

    // mac-air pulls first and fills its 2 free units with 1 standard worker.
    mac_air.daemon.reconcile(server.now()).await.unwrap();
    assert_eq!(mac_air.runner.spawns().len(), 1);
    assert_eq!(mac_air.daemon.state.managed_runs.len(), 1);
    assert_eq!(mac_air.daemon.db().free_units(4, 2).unwrap(), 0);
    // Write-before-act local_runs row is persisted in daemon.db.
    let air_local = mac_air.daemon.db().list_active_local_runs().unwrap();
    assert_eq!(air_local.len(), 1);
    assert_eq!(air_local[0].state, LocalRunState::Running);

    // mac-pro pulls from the *same board* with zero 45m board coordinator lease!
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert_eq!(mac_pro.runner.spawns().len(), 1);
    assert_eq!(mac_pro.daemon.state.managed_runs.len(), 1);
    assert_eq!(mac_pro.daemon.db().free_units(12, 2).unwrap(), 8);

    // Verify both distinct tasks were claimed across the two Macs and board has NO coordinator lease.
    let board_after = server.user_client().board(&board).await.unwrap();
    assert_eq!(
        board_after.coordinator, None,
        "pull mode must not hold a 45m board coordinator lease"
    );
    let air_task = mac_air
        .daemon
        .state
        .managed_runs
        .values()
        .next()
        .unwrap()
        .task_id
        .clone()
        .unwrap();
    let pro_task = mac_pro
        .daemon
        .state
        .managed_runs
        .values()
        .next()
        .unwrap()
        .task_id
        .clone()
        .unwrap();
    assert_ne!(air_task, pro_task);
    assert!([t1.clone(), t2.clone()].contains(&air_task));
    assert!([t1, t2].contains(&pro_task));
    assert!(mac_air.project.exists());
}

#[tokio::test]
async fn deterministic_runner_merges_and_housekeeping_with_zero_llm_spawns() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 2).await;

    let shared_gh = FakeGh::default();
    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, Some(shared_gh.clone()));
    let mut mac_air = spawn_device_node(&server, "mac-air", 4, 1, Some(shared_gh.clone()));
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();
    mac_air.daemon.send_heartbeat(server.now()).await.unwrap();

    // 1. Cross-device `pr: none` merge: reviewed on mac-pro, merged on mac-air with ZERO LLM spawns!
    let no_pr_task = make_reviewed_task(
        &server,
        &board,
        "Docs task without PR",
        "review: pass. Verified docs.\n- check: cargo test => 0\npr: none",
        "mac-pro",
        &mac_pro.token,
    )
    .await;
    mac_air.daemon.reconcile(server.now()).await.unwrap();
    assert!(
        mac_air.runner.spawns().is_empty(),
        "DeterministicRunner merge must not spawn any LLM runner"
    );
    let done_detail = server.user_client().task(&no_pr_task).await.unwrap();
    assert_eq!(done_detail.task.status, TaskStatus::Done);
    assert_eq!(done_detail.task.summary.as_deref(), Some("Verified docs."));

    // 2. Mocked PR merge flow: Behind -> update-branch -> Pending (releases repo:<repo>:merge)
    //    -> Failed checks (posts finding once) -> Clean + Passed -> merge --match-head-commit -> Done!
    let pr_url = format!("https://{REPO}/pull/42");
    shared_gh.put(open_pr(
        &pr_url,
        "head-aaa111",
        MergeState::Behind,
        Checks::Passed,
    ));
    let pr_task = make_reviewed_task(
        &server,
        &board,
        "Code task with PR",
        &format!("review: pass. Looks good.\n- check: cargo test => 0\npr: {pr_url}"),
        "mac-air",
        &mac_air.token,
    )
    .await;

    // Tick 1: mac-pro claims Merge, sees Behind, calls update-branch, releases repo:<repo>:merge cleanly.
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert!(mac_pro.runner.spawns().is_empty());
    assert!(
        shared_gh
            .calls()
            .iter()
            .any(|c| c == &format!("update-branch {pr_url}")),
        "expected update-branch call: {:?}",
        shared_gh.calls()
    );
    assert_eq!(
        server
            .user_client()
            .task(&pr_task)
            .await
            .unwrap()
            .task
            .status,
        TaskStatus::Review
    );
    // Verify no active merge run holds repo:<repo>:merge while checks are pending.
    let active_merges = server
        .user_client()
        .list_runs(&RunFilter {
            task: Some(pr_task.clone()),
            kind: Some(RunKind::Merge),
            active: Some(true),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert!(
        active_merges.is_empty(),
        "deferred merge must release repo:<repo>:merge lock: {active_merges:?}"
    );

    // Tick 2: checks fail on new head -> finding note posted once across BOTH mac-pro AND
    // mac-air (cross-device deduplication via board notes, not just local merges state!).
    // Advance past the 300s server-side checks-failed cooldown between attempts so both
    // devices actually execute a merge pass.
    shared_gh.put(open_pr(
        &pr_url,
        "head-bbb222",
        MergeState::Waits,
        Checks::Failed(vec!["rust".into()]),
    ));
    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    server.advance(TimeDelta::minutes(5));
    mac_air.daemon.reconcile(server.now()).await.unwrap();
    let notes = server.user_client().task(&pr_task).await.unwrap().notes;
    let failed_findings: Vec<_> = notes
        .iter()
        .filter(|n| n.kind == NoteKind::Finding && n.text.contains("head-bbb222"))
        .collect();
    assert_eq!(
        failed_findings.len(),
        1,
        "failed check finding must be deduped per head_oid across devices: {notes:?}"
    );

    // Tick 3: checks pass and PR is clean -> merges with --match-head-commit and closes task Done!
    shared_gh.put(open_pr(
        &pr_url,
        "head-ccc333",
        MergeState::Clean,
        Checks::Passed,
    ));
    server.advance(TimeDelta::minutes(5));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert!(mac_pro.runner.spawns().is_empty());
    assert!(
        shared_gh
            .calls()
            .iter()
            .any(|c| c == &format!("merge {pr_url} head-ccc333")),
        "expected merge --match-head-commit call: {:?}",
        shared_gh.calls()
    );
    let closed = server.user_client().task(&pr_task).await.unwrap();
    assert_eq!(closed.task.status, TaskStatus::Done);
    assert!(
        closed.evidence.iter().any(|e| e.value == pr_url),
        "PR evidence recorded: {:?}",
        closed.evidence
    );

    // 3. Conflict PR -> posts `review: void` decision note and reopens task to `open`!
    let conflict_url = format!("https://{REPO}/pull/99");
    shared_gh.put(open_pr(
        &conflict_url,
        "head-conf",
        MergeState::Conflict,
        Checks::Passed,
    ));
    let conflict_task = make_reviewed_task(
        &server,
        &board,
        "Conflicting PR task",
        &format!("review: pass. Ship it.\npr: {conflict_url}"),
        "mac-pro",
        &mac_pro.token,
    )
    .await;
    // Deterministic-only node (no LLM runners registered) handles conflict reopen & housekeeping
    // cleanly without immediately pulling the reopened task as an LLM worker.
    let hk_node_dir = tempfile::tempdir().unwrap();
    let hk_project = hk_node_dir.path().join("repo");
    std::fs::create_dir_all(&hk_project).unwrap();
    let hk_token = server.create_device("hk-light");
    let mut hk_projects = BTreeMap::new();
    hk_projects.insert(REPO.to_owned(), hk_project);
    let hk_ctx = Ctx {
        home: Home::at(hk_node_dir.path().join("home")),
        device: "hk-light".parse().unwrap(),
        client: Client::new(&server.url, hk_token, None, None).unwrap(),
        config: Config {
            server: Some(server.url.clone()),
            device: Some("hk-light".parse().unwrap()),
            weight_units: Some(4),
            reserve_runs: Some(0),
            projects: hk_projects,
            dispatch_mode: DispatchMode::Pull,
            ..Config::default()
        },
        repo: RepoInfo::default(),
        known: None,
    };
    let hk_clock = server.clock.clone();
    let mut hk_daemon = Daemon::new(hk_ctx, NoopT3)
        .with_runner_registry(RunnerRegistry::new())
        .with_gh(shared_gh.clone())
        .with_clock(Arc::new(move || hk_clock.now()));
    hk_daemon.send_heartbeat(server.now()).await.unwrap();

    server.advance(TimeDelta::minutes(1));
    hk_daemon.reconcile(server.now()).await.unwrap();
    let reopened = server.user_client().task(&conflict_task).await.unwrap();
    assert_eq!(reopened.task.status, TaskStatus::Open);
    assert!(
        reopened
            .notes
            .iter()
            .any(|n| n.kind == NoteKind::Decision && n.text.contains("review: void")),
        "expected review: void note on conflict: {:?}",
        reopened.notes
    );
    // Clean up conflict_task so housekeeping test below has a clean slate.
    server
        .user_client()
        .update_task(
            &conflict_task,
            &UpdateTask {
                auto: Some(false),
                ..update_task_base("t3:admin".parse().unwrap(), reopened.task.version)
            },
        )
        .await
        .unwrap();

    // 4. Housekeeping: AnswerGoneAsker posts `decision` note with answer and reopens task!
    let gone_asker: AgentId = "antigravity:gone-asker-1".parse().unwrap();
    let gone_client = server.device_client(&mac_pro.token, Some(gone_asker.clone()));
    gone_client
        .register(&RegisterAgent {
            id: gone_asker.clone(),
            runtime: Runtime::Antigravity,
            device: "mac-pro".parse().unwrap(),
            role: AgentRole::Worker,
            title: Some("gone asker".into()),
            repo: Some(REPO.into()),
            branch: None,
            cwd: None,
            native_ids: BTreeMap::new(),
            forked_from: None,
        })
        .await
        .unwrap();
    let ask_task = server
        .add_ready_task(&board, "Task awaiting gone asker")
        .await;
    let claimed = gone_client.task_action(&ask_task, "claim").await.unwrap();
    let ask = gone_client
        .create_ask(&CreateAsk {
            agent: gone_asker.clone(),
            board: board.clone(),
            task: Some(ask_task.clone()),
            kind: board_proto::AskKind::Decision,
            question: "Which mode should we use?".to_owned(),
            options: vec!["Pull".into(), "Legacy".into()],
            change: None,
        })
        .await
        .unwrap();
    gone_client
        .update_task(
            &ask_task,
            &UpdateTask {
                status: Some(TaskStatus::Blocked),
                blocked_reason: Some(format!("waiting on ask #{}", ask.ask.id)),
                ..update_task_base(gone_asker.clone(), claimed.version)
            },
        )
        .await
        .unwrap();
    // Asker ends before user answers.
    gone_client
        .set_state(
            &gone_asker,
            &AgentStateUpdate {
                state: AgentState::Ended,
                paused_until: None,
                said: None,
            },
        )
        .await
        .unwrap();
    // User answers the ask.
    server
        .user_client()
        .answer(ask.ask.id, "Use Pull mode")
        .await
        .unwrap();

    let spawns_before = mac_pro.runner.spawns().len();
    server.advance(TimeDelta::minutes(1));
    hk_daemon.reconcile(server.now()).await.unwrap();

    assert_eq!(mac_pro.runner.spawns().len(), spawns_before);
    let hk_detail = server.user_client().task(&ask_task).await.unwrap();
    assert_eq!(hk_detail.task.status, TaskStatus::Open);
    assert_eq!(hk_detail.task.owner, None);
    assert!(
        hk_detail.notes.iter().any(|n| {
            n.kind == NoteKind::Decision
                && n.text.contains(&format!("ask #{}", ask.ask.id))
                && n.text.contains("Use Pull mode")
        }),
        "expected housekeeping decision note with answer: {:?}",
        hk_detail.notes
    );
    let hk_runs = server
        .user_client()
        .list_runs(&RunFilter {
            task: Some(ask_task),
            kind: Some(RunKind::Housekeeping),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(hk_runs.len(), 1);
    assert_eq!(hk_runs[0].state, RunState::Succeeded);
    assert_eq!(hk_runs[0].runner, "deterministic");
}

#[tokio::test]
async fn fast_3m_failover_orphans_crashed_mac_pro_run_and_mac_air_recovers() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 1).await;
    let task_id = server
        .add_ready_task(&board, "Critical failover task")
        .await;

    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, None);
    let mut mac_air = spawn_device_node(&server, "mac-air", 4, 1, None);
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();
    mac_air.daemon.send_heartbeat(server.now()).await.unwrap();

    // mac-pro claims the task and begins running.
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert_eq!(mac_pro.runner.spawns().len(), 1);
    let pro_run_id = mac_pro
        .daemon
        .state
        .managed_runs
        .keys()
        .next()
        .unwrap()
        .clone();

    // While mac-pro holds the live lease, mac-air cannot double-claim the same task.
    mac_air.daemon.reconcile(server.now()).await.unwrap();
    assert!(mac_air.runner.spawns().is_empty());

    // mac-pro crashes / closes lid (stops heartbeating). Advance clock past the 3m lease TTL!
    server.advance(TimeDelta::seconds(185));
    server.run_sweeper_once().await;

    let orphaned = server.user_client().get_run(&pro_run_id).await.unwrap();
    assert_eq!(
        orphaned.state,
        RunState::Orphaned,
        "sweeper must orphan unrenewed run after 3m"
    );
    assert!(
        orphaned.conflict_locks.is_empty(),
        "orphaned run must release conflict_locks"
    );

    // mac-air immediately pulls recovery for the same task!
    mac_air.daemon.reconcile(server.now()).await.unwrap();
    assert_eq!(mac_air.runner.spawns().len(), 1);
    let air_run = mac_air
        .daemon
        .state
        .managed_runs
        .values()
        .next()
        .expect("mac-air claimed recovery run");
    assert_eq!(air_run.task_id.as_ref(), Some(&task_id));
    assert_ne!(air_run.run_id, pro_run_id);
}

#[tokio::test]
async fn sticky_wake_delivery_via_runner_send_and_ack() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 1).await;
    let task_id = server
        .add_ready_task(&board, "Task that asks a question")
        .await;

    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, None);
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert_eq!(mac_pro.runner.spawns().len(), 1);

    let managed = mac_pro
        .daemon
        .state
        .managed_runs
        .values()
        .next()
        .unwrap()
        .clone();
    let agent_id = managed.agent_id.clone().expect("agent_id recorded on run");
    let worker_client = server.device_client(&mac_pro.token, Some(agent_id.clone()));

    // Spawned worker registers, claims task, and files an ask.
    worker_client
        .register(&RegisterAgent {
            id: agent_id.clone(),
            runtime: Runtime::Antigravity,
            device: "mac-pro".parse().unwrap(),
            role: AgentRole::Worker,
            title: Some("worker".into()),
            repo: Some(REPO.into()),
            branch: None,
            cwd: None,
            native_ids: BTreeMap::new(),
            forked_from: None,
        })
        .await
        .unwrap();
    worker_client.task_action(&task_id, "claim").await.unwrap();
    let ask = worker_client
        .create_ask(&CreateAsk {
            agent: agent_id.clone(),
            board: board.clone(),
            task: Some(task_id.clone()),
            kind: board_proto::AskKind::Decision,
            question: "Should we enable caching?".to_owned(),
            options: vec!["Yes".into(), "No".into()],
            change: None,
        })
        .await
        .unwrap();

    // User answers the ask -> server enqueues sticky wake for mac-pro.
    server
        .user_client()
        .answer(ask.ask.id, "Yes, enable caching")
        .await
        .unwrap();
    let pending_before = server
        .device_client(&mac_pro.token, None)
        .list_device_wakes(&"mac-pro".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(pending_before.len(), 1);
    assert_eq!(pending_before[0].kind, "answered");

    // mac-pro polls sticky wakes, delivers via FakeRunner::send, and acks!
    let delivered = mac_pro
        .daemon
        .poll_sticky_wakes(server.now())
        .await
        .unwrap();
    assert_eq!(delivered, 1);

    let sends = mac_pro.runner.sends();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].handle.native_id, managed.native_id.unwrap());
    assert!(
        sends[0].message.text.contains("Yes, enable caching"),
        "expected answer in steered text: {}",
        sends[0].message.text
    );
    assert!(
        sends[0].message.digest.is_some(),
        "wake digest must be recorded for write-before-send"
    );

    // Sticky wake queue on server is now empty (acknowledged), and local unsure_sends is clear.
    let pending_after = server
        .device_client(&mac_pro.token, None)
        .list_device_wakes(&"mac-pro".parse().unwrap())
        .await
        .unwrap();
    assert!(pending_after.is_empty());
    assert!(mac_pro.daemon.state.unsure_wakes.is_empty());
}

#[tokio::test]
async fn thirty_second_lease_renewal_keeps_long_run_alive_and_completes_cleanly() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 1).await;
    let _task_id = server.add_ready_task(&board, "Long running build").await;

    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, None);
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert_eq!(mac_pro.runner.spawns().len(), 1);

    let managed = mac_pro
        .daemon
        .state
        .managed_runs
        .values()
        .next()
        .unwrap()
        .clone();
    let run_id = managed.run_id.clone();
    let native_id = managed.native_id.clone().unwrap();
    let initial_exp = server
        .user_client()
        .get_run(&run_id)
        .await
        .unwrap()
        .lease_expires_at
        .expect("initial lease expiry");

    // Renew across seven 30-second heartbeat ticks (210s total, past the original 180s expiry)
    // while running the server sweeper on every tick: run must stay Running and never orphan!
    for _ in 0..7 {
        server.advance(TimeDelta::seconds(30));
        assert!(mac_pro.daemon.send_heartbeat(server.now()).await.unwrap());
        server.run_sweeper_once().await;
        let current = server.user_client().get_run(&run_id).await.unwrap();
        assert_eq!(current.state, RunState::Running);
    }
    let renewed = server.user_client().get_run(&run_id).await.unwrap();
    assert!(
        renewed.lease_expires_at.unwrap() > initial_exp + TimeDelta::seconds(180),
        "lease must advance across renewals: initial={initial_exp}, now={:?}",
        renewed.lease_expires_at
    );

    // Runner finishes its turn and returns Idle with `runs: Some(1)` (matching real T3Runner
    // and AcpRunner behavior!) -> next heartbeat transitions Run to Succeeded and frees capacity!
    mac_pro.runner.set_status(
        &native_id,
        RunnerStatus {
            state: RunnerState::Idle,
            runnable: true,
            busy: false,
            runtime: Some(Runtime::Antigravity),
            workdir: Some(mac_pro.project.clone()),
            runs: Some(1),
        },
    );
    server.advance(TimeDelta::seconds(30));
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();

    let finished = server.user_client().get_run(&run_id).await.unwrap();
    assert_eq!(finished.state, RunState::Succeeded);
    assert!(mac_pro.daemon.state.managed_runs.is_empty());
    assert_eq!(mac_pro.daemon.db().free_units(12, 2).unwrap(), 10);
    let local = mac_pro
        .daemon
        .db()
        .get_local_run(run_id.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(local.state, LocalRunState::Finished);
}

#[tokio::test]
async fn sticky_wake_timeout_prevents_duplicate_resend_and_acks_once_verified() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 1).await;
    let task_id = server.add_ready_task(&board, "Task with slow wake").await;

    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, None);
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();
    mac_pro.daemon.reconcile(server.now()).await.unwrap();

    let managed = mac_pro
        .daemon
        .state
        .managed_runs
        .values()
        .next()
        .unwrap()
        .clone();
    let agent_id = managed.agent_id.clone().unwrap();
    let worker_client = server.device_client(&mac_pro.token, Some(agent_id.clone()));
    worker_client
        .register(&RegisterAgent {
            id: agent_id.clone(),
            runtime: Runtime::Antigravity,
            device: "mac-pro".parse().unwrap(),
            role: AgentRole::Worker,
            title: Some("worker".into()),
            repo: Some(REPO.into()),
            branch: None,
            cwd: None,
            native_ids: BTreeMap::new(),
            forked_from: None,
        })
        .await
        .unwrap();
    let claimed = worker_client.task_action(&task_id, "claim").await.unwrap();
    let ask = worker_client
        .create_ask(&CreateAsk {
            agent: agent_id.clone(),
            board: board.clone(),
            task: Some(task_id.clone()),
            kind: board_proto::AskKind::Decision,
            question: "Proceed with rollout?".to_owned(),
            options: vec!["Yes".into(), "No".into()],
            change: None,
        })
        .await
        .unwrap();
    worker_client
        .update_task(
            &task_id,
            &UpdateTask {
                status: Some(TaskStatus::Blocked),
                blocked_reason: Some(format!("waiting on ask #{}", ask.ask.id)),
                ..update_task_base(agent_id.clone(), claimed.version)
            },
        )
        .await
        .unwrap();
    // Worker goes Idle while task is Blocked -> renew_runs transitions Run to WaitingExternal
    // instead of completing it!
    mac_pro.runner.set_status(
        managed.native_id.as_deref().unwrap(),
        RunnerStatus {
            state: RunnerState::Idle,
            runnable: true,
            busy: false,
            runtime: Some(Runtime::Antigravity),
            workdir: Some(mac_pro.project.clone()),
            runs: Some(1),
        },
    );
    server.advance(TimeDelta::seconds(30));
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();
    assert_eq!(
        server
            .user_client()
            .get_run(&managed.run_id)
            .await
            .unwrap()
            .state,
        RunState::WaitingExternal
    );

    // User answers ask while send times out AFTER delivering the message to the runner.
    server
        .user_client()
        .answer(ask.ask.id, "Yes, proceed")
        .await
        .unwrap();
    mac_pro.runner.set_time_out_send(Some(true));

    // Poll 1: send times out, wake stays unacked in unsure_sends.
    assert_eq!(
        mac_pro
            .daemon
            .poll_sticky_wakes(server.now())
            .await
            .unwrap(),
        0
    );
    assert_eq!(mac_pro.runner.sends().len(), 1);
    assert!(!mac_pro.daemon.state.unsure_wakes.is_empty());

    // Poll 2 (3 seconds later): must NOT resend immediately while unsure!
    server.advance(TimeDelta::seconds(3));
    assert_eq!(
        mac_pro
            .daemon
            .poll_sticky_wakes(server.now())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mac_pro.runner.sends().len(),
        1,
        "timed-out sticky wake must not be resent before unsure verification"
    );

    // Poll 3 (after SEND_CHECK_AFTER = 60s): verifies message arrived in runner events,
    // clears unsure_sends, and calls ack_wake without resending!
    server.advance(TimeDelta::seconds(65));
    assert_eq!(
        mac_pro
            .daemon
            .poll_sticky_wakes(server.now())
            .await
            .unwrap(),
        1
    );
    assert_eq!(mac_pro.runner.sends().len(), 1);
    assert!(mac_pro.daemon.state.unsure_wakes.is_empty());
    let remaining = server
        .device_client(&mac_pro.token, None)
        .list_device_wakes(&"mac-pro".parse().unwrap())
        .await
        .unwrap();
    assert!(remaining.is_empty());
}

#[tokio::test]
async fn spawn_failure_marks_run_failed_releases_conflict_locks_and_lets_peer_claim() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 1).await;
    let task_id = server
        .add_ready_task(&board, "Retry after spawn failure")
        .await;

    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, None);
    let mut mac_air = spawn_device_node(&server, "mac-air", 4, 1, None);
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();
    mac_air.daemon.send_heartbeat(server.now()).await.unwrap();

    // mac-pro's runner fails to spawn.
    mac_pro.runner.set_fail_spawn(Some("binary missing"));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();

    // Server run must be marked Failed with conflict_locks released, and mac-pro's free_units restored!
    let runs = server
        .user_client()
        .list_runs(&RunFilter {
            task: Some(task_id.clone()),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert!(!runs.is_empty());
    assert!(runs.iter().all(|r| r.state == RunState::Failed));
    assert!(runs.iter().all(|r| r.conflict_locks.is_empty()));
    assert_eq!(mac_pro.daemon.db().free_units(12, 2).unwrap(), 10);

    // Trail Rule drop was recorded on mac-pro.
    let drops_path = mac_pro._dir.path().join("home/state/daemon-drops.jsonl");
    let drops = std::fs::read_to_string(&drops_path).unwrap();
    assert!(drops.contains("pull_run_failed"), "{drops}");
    assert!(drops.contains("binary missing"), "{drops}");

    // mac-air immediately claims the task with zero delay!
    mac_air.daemon.reconcile(server.now()).await.unwrap();
    assert_eq!(mac_air.runner.spawns().len(), 1);
    let air_run = mac_air
        .daemon
        .state
        .managed_runs
        .values()
        .next()
        .expect("mac-air claimed task after mac-pro spawn failure");
    assert_eq!(air_run.task_id.as_ref(), Some(&task_id));
}

#[tokio::test]
async fn deterministic_runner_marks_passed_draft_pr_ready_then_merges() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 1).await;

    let shared_gh = FakeGh::default();
    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, Some(shared_gh.clone()));
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();

    // 1. Draft PR on a passed task: tick 1 runs `gh pr ready` and defers (releasing the merge
    //    lock); tick 2 merges with --match-head-commit and finishes the task Done.
    let pr_url = format!("https://{REPO}/pull/101");
    let mut draft = open_pr(&pr_url, "head-draft1", MergeState::Waits, Checks::Passed);
    draft.draft = true;
    shared_gh.put(draft);
    let task_id = make_reviewed_task(
        &server,
        &board,
        "Draft PR merge test",
        &format!("review: pass. Verified.\n- check: cargo test => 0\npr: {pr_url}"),
        "mac-pro",
        &mac_pro.token,
    )
    .await;

    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert_eq!(
        shared_gh.calls(),
        [format!("view {pr_url}"), format!("ready {pr_url}")]
    );
    assert_eq!(
        server
            .user_client()
            .task(&task_id)
            .await
            .unwrap()
            .task
            .status,
        TaskStatus::Review
    );
    let active_merges = server
        .user_client()
        .list_runs(&RunFilter {
            task: Some(task_id.clone()),
            kind: Some(RunKind::Merge),
            active: Some(true),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert!(
        active_merges.is_empty(),
        "deferred draft-ready must release repo:<repo>:merge lock: {active_merges:?}"
    );

    // Advance past PR_READ_EVERY so mac-pro retries the merge now that the PR is ready.
    server.advance(TimeDelta::minutes(3));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert!(
        shared_gh
            .calls()
            .iter()
            .any(|c| c == &format!("merge {pr_url} head-draft1")),
        "expected merge after draft became ready: {:?}",
        shared_gh.calls()
    );
    let closed = server.user_client().task(&task_id).await.unwrap();
    assert_eq!(closed.task.status, TaskStatus::Done);
    assert!(closed.evidence.iter().any(|e| e.value == pr_url));

    // 2. Conflicting draft PR: reopened with `review: void` without calling `ready`.
    let conflict_url = format!("https://{REPO}/pull/102");
    let mut conflict_draft = open_pr(
        &conflict_url,
        "head-draft-conf",
        MergeState::Conflict,
        Checks::Passed,
    );
    conflict_draft.draft = true;
    shared_gh.put(conflict_draft);
    let conflict_task = make_reviewed_task(
        &server,
        &board,
        "Conflicting draft PR test",
        &format!("review: pass. Verified.\npr: {conflict_url}"),
        "mac-pro",
        &mac_pro.token,
    )
    .await;

    server.advance(TimeDelta::minutes(3));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert!(
        !shared_gh
            .calls()
            .iter()
            .any(|c| c == &format!("ready {conflict_url}")),
        "conflicting draft PR must not be marked ready: {:?}",
        shared_gh.calls()
    );
    let reopened = server.user_client().task(&conflict_task).await.unwrap();
    assert_eq!(reopened.task.status, TaskStatus::Open);
    assert!(
        reopened
            .notes
            .iter()
            .any(|n| n.kind == NoteKind::Decision && n.text.starts_with("decision: review: void"))
    );
}

#[tokio::test]
async fn deterministic_runner_reopens_after_failed_checks_persistent_draft_and_waits_hold() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 1).await;

    let shared_gh = FakeGh::default();
    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, Some(shared_gh.clone()));
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();

    // 1. Failed checks hold for PR_HOLD_WAIT -> finding noted once, then pass voided & task opened.
    let pr1_url = format!("https://{REPO}/pull/201");
    shared_gh.put(open_pr(
        &pr1_url,
        "head-fail-1",
        MergeState::Clean,
        Checks::Failed(vec!["rust".into(), "docker".into()]),
    ));
    let t1 = make_reviewed_task(
        &server,
        &board,
        "Persistent red checks",
        &format!("review: pass. Verified.\npr: {pr1_url}"),
        "mac-pro",
        &mac_pro.token,
    )
    .await;

    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let mid = server.user_client().task(&t1).await.unwrap();
    assert_eq!(mid.task.status, TaskStatus::Review);
    assert_eq!(
        mid.notes
            .iter()
            .filter(|n| n.kind == NoteKind::Finding)
            .count(),
        1
    );

    server.advance(board::daemon::PR_HOLD_WAIT);
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let reopened = server.user_client().task(&t1).await.unwrap();
    assert_eq!(reopened.task.status, TaskStatus::Open);
    assert!(reopened.notes.iter().any(|n| {
        n.kind == NoteKind::Decision
            && n.text.starts_with("decision: review: void")
            && n.text.contains("rust, docker")
    }));
    // Close t1 so it does not compete with subsequent tasks.
    server
        .user_client()
        .update_task(
            &t1,
            &UpdateTask {
                status: Some(TaskStatus::Dropped),
                reason: Some("test cleanup".to_owned()),
                ..update_task_base("t3:admin".parse().unwrap(), reopened.task.version)
            },
        )
        .await
        .unwrap();

    // 2. Persistent draft (gh pr ready fails) reopens after PR_HOLD_WAIT.
    let pr2_url = format!("https://{REPO}/pull/202");
    let mut draft = open_pr(
        &pr2_url,
        "head-draft-stuck",
        MergeState::Waits,
        Checks::Passed,
    );
    draft.draft = true;
    shared_gh.put(draft);
    shared_gh.ready_fails.store(true, Ordering::SeqCst);
    let t2 = make_reviewed_task(
        &server,
        &board,
        "Persistent draft PR",
        &format!("review: pass. Verified.\npr: {pr2_url}"),
        "mac-pro",
        &mac_pro.token,
    )
    .await;

    server.advance(TimeDelta::minutes(3));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert_eq!(
        server.user_client().task(&t2).await.unwrap().task.status,
        TaskStatus::Review
    );
    server.advance(board::daemon::PR_HOLD_WAIT);
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let reopened2 = server.user_client().task(&t2).await.unwrap();
    assert_eq!(reopened2.task.status, TaskStatus::Open);
    assert!(reopened2.notes.iter().any(|n| {
        n.kind == NoteKind::Decision
            && n.text.starts_with("decision: review: void")
            && n.text.contains("draft")
    }));
    server
        .user_client()
        .update_task(
            &t2,
            &UpdateTask {
                status: Some(TaskStatus::Dropped),
                reason: Some("test cleanup".to_owned()),
                ..update_task_base("t3:admin".parse().unwrap(), reopened2.task.version)
            },
        )
        .await
        .unwrap();

    // 3. MergeState::Waits (generic hold after checks passed) reopens after PR_HOLD_WAIT.
    let pr3_url = format!("https://{REPO}/pull/203");
    shared_gh.put(open_pr(
        &pr3_url,
        "head-waits-1",
        MergeState::Waits,
        Checks::Passed,
    ));
    let t3 = make_reviewed_task(
        &server,
        &board,
        "Stuck MergeState::Waits PR",
        &format!("review: pass. Verified.\npr: {pr3_url}"),
        "mac-pro",
        &mac_pro.token,
    )
    .await;

    server.advance(TimeDelta::minutes(3));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert_eq!(
        server.user_client().task(&t3).await.unwrap().task.status,
        TaskStatus::Review
    );
    server.advance(board::daemon::PR_HOLD_WAIT);
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let reopened3 = server.user_client().task(&t3).await.unwrap();
    assert_eq!(reopened3.task.status, TaskStatus::Open);
    assert!(reopened3.notes.iter().any(|n| {
        n.kind == NoteKind::Decision
            && n.text.starts_with("decision: review: void")
            && n.text.contains(&pr3_url)
    }));
}

#[tokio::test]
async fn deterministic_runner_wakes_user_for_required_review_and_resumes_on_answer() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 1).await;

    let shared_gh = FakeGh::default();
    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, Some(shared_gh.clone()));
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();

    // Task 1: human review missing -> wakes user with Action ask + blocks task;
    // answering "Approved — resume merge" returns the task to review.
    let pr1_url = format!("https://{REPO}/pull/210");
    let mut pr1 = open_pr(&pr1_url, "head-rev-1", MergeState::Waits, Checks::Passed);
    pr1.review_decision = ReviewDecision::ReviewRequired;
    shared_gh.put(pr1);
    let t1 = make_reviewed_task(
        &server,
        &board,
        "Needs human review approval",
        &format!("review: pass. Verified.\npr: {pr1_url}"),
        "mac-pro",
        &mac_pro.token,
    )
    .await;

    // First read starts the hold timer without creating an ask.
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert_eq!(
        server.user_client().task(&t1).await.unwrap().task.status,
        TaskStatus::Review
    );
    assert!(
        server
            .user_client()
            .asks(Some("open"), Some(&board), None)
            .await
            .unwrap()
            .is_empty()
    );

    // At PR_HOLD_WAIT: creates Action ask and moves task to blocked.
    server.advance(board::daemon::PR_HOLD_WAIT);
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let blocked = server.user_client().task(&t1).await.unwrap();
    assert_eq!(blocked.task.status, TaskStatus::Blocked);
    let asks = server
        .user_client()
        .asks(Some("open"), Some(&board), None)
        .await
        .unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].task.as_ref(), Some(&t1));
    assert!(asks[0].question.contains("Reason: only-me"));
    assert!(
        blocked
            .task
            .blocked_reason
            .as_deref()
            .unwrap_or("")
            .contains(&format!("ask #{}", asks[0].id))
    );

    // User approves on GitHub and answers the ask -> housekeeping returns the task to review
    // while preserving the standing pass, then the next reconcile merges directly with ZERO
    // new LLM review judge spawns and closes the task Done!
    let mut pr1_approved = open_pr(&pr1_url, "head-rev-1", MergeState::Clean, Checks::Passed);
    pr1_approved.review_decision = ReviewDecision::Approved;
    shared_gh.put(pr1_approved);
    server
        .user_client()
        .answer(asks[0].id, "Approved — resume merge")
        .await
        .unwrap();
    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let finished = server.user_client().task(&t1).await.unwrap();
    assert_eq!(
        finished.task.status,
        TaskStatus::Done,
        "resumed task must merge and finish Done without re-review: {finished:?}"
    );
    assert!(
        mac_pro.runner.spawns().is_empty(),
        "must not spawn a new LLM review judge after human-review approval: {:?}",
        mac_pro.runner.spawns()
    );
    assert!(
        shared_gh
            .calls()
            .iter()
            .any(|c| c == &format!("merge {pr1_url} head-rev-1")),
        "expected merge after approval: {:?}",
        shared_gh.calls()
    );
    assert!(finished.notes.iter().any(|n| {
        n.kind == NoteKind::Decision
            && n.text.contains(&format!("ask #{}", asks[0].id))
            && n.text.contains("Approved — resume merge")
    }));

    // Task 2: answering "Reopen for a worker" voids the pass and sets the task open.
    let pr2_url = format!("https://{REPO}/pull/211");
    let mut pr2 = open_pr(&pr2_url, "head-rev-2", MergeState::Waits, Checks::Passed);
    pr2.review_decision = ReviewDecision::ReviewRequired;
    shared_gh.put(pr2);
    let t2 = make_reviewed_task(
        &server,
        &board,
        "Needs human review reopen",
        &format!("review: pass. Verified.\npr: {pr2_url}"),
        "mac-pro",
        &mac_pro.token,
    )
    .await;

    server.advance(TimeDelta::minutes(3));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    server.advance(board::daemon::PR_HOLD_WAIT);
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let asks2 = server
        .user_client()
        .asks(Some("open"), Some(&board), None)
        .await
        .unwrap();
    assert_eq!(asks2.len(), 1);
    assert_eq!(asks2[0].task.as_ref(), Some(&t2));

    server
        .user_client()
        .answer(asks2[0].id, "Reopen for a worker")
        .await
        .unwrap();
    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let reopened = server.user_client().task(&t2).await.unwrap();
    assert_eq!(reopened.task.status, TaskStatus::Open);
    assert!(reopened.notes.iter().any(|n| {
        n.kind == NoteKind::Decision
            && n.text.starts_with("decision: review: void")
            && n.text.contains(&format!("ask #{}", asks2[0].id))
    }));
    server
        .user_client()
        .update_task(
            &t2,
            &UpdateTask {
                status: Some(TaskStatus::Dropped),
                reason: Some("test cleanup".to_owned()),
                ..update_task_base("t3:admin".parse().unwrap(), reopened.task.version)
            },
        )
        .await
        .unwrap();

    // Task 3: an unowned blocked task with an answered dispatch:* ask cleanly moves to Open
    // via update_task (no 403 release), and an unrelated blocked_reason is never unblocked.
    let t3 = server
        .add_ready_task(&board, "Unowned dispatch ask reopen")
        .await;
    let t3_detail = server.user_client().task(&t3).await.unwrap();
    let dispatch_client =
        server.device_client(&mac_pro.token, Some("dispatch:mac-pro".parse().unwrap()));
    let ask3 = dispatch_client
        .create_ask(&CreateAsk {
            agent: "dispatch:mac-pro".parse().unwrap(),
            board: board.clone(),
            task: Some(t3.clone()),
            kind: board_proto::AskKind::Action,
            question: "Approve PR".to_owned(),
            options: vec!["Approved — resume merge".to_owned()],
            change: None,
        })
        .await
        .unwrap();
    server
        .user_client()
        .update_task(
            &t3,
            &UpdateTask {
                status: Some(TaskStatus::Blocked),
                blocked_reason: Some(format!(
                    "waiting on ask #{}: required human review",
                    ask3.ask.id
                )),
                ..update_task_base("t3:admin".parse().unwrap(), t3_detail.task.version)
            },
        )
        .await
        .unwrap();
    server
        .user_client()
        .answer(ask3.ask.id, "Approved — resume merge")
        .await
        .unwrap();
    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let t3_after = server.user_client().task(&t3).await.unwrap();
    assert_eq!(
        t3_after.task.status,
        TaskStatus::Open,
        "unowned blocked task must move to Open without 403 release: {t3_after:?}"
    );
    server
        .user_client()
        .update_task(
            &t3,
            &UpdateTask {
                status: Some(TaskStatus::Dropped),
                reason: Some("test cleanup".to_owned()),
                ..update_task_base("t3:admin".parse().unwrap(), t3_after.task.version)
            },
        )
        .await
        .unwrap();

    // Task 4: answered dispatch ask must NOT unblock a task whose blocked_reason is for a
    // different ask or has another open ask.
    let t4 = server
        .add_ready_task(&board, "Do not unblock unrelated ask")
        .await;
    let t4_detail = server.user_client().task(&t4).await.unwrap();
    let ask4a = dispatch_client
        .create_ask(&CreateAsk {
            agent: "dispatch:mac-pro".parse().unwrap(),
            board: board.clone(),
            task: Some(t4.clone()),
            kind: board_proto::AskKind::Action,
            question: "Old ask".to_owned(),
            options: vec!["Approved — resume merge".to_owned()],
            change: None,
        })
        .await
        .unwrap();
    let ask4b = dispatch_client
        .create_ask(&CreateAsk {
            agent: "dispatch:mac-pro".parse().unwrap(),
            board: board.clone(),
            task: Some(t4.clone()),
            kind: board_proto::AskKind::Action,
            question: "Newer open ask".to_owned(),
            options: vec!["Yes".to_owned()],
            change: None,
        })
        .await
        .unwrap();
    server
        .user_client()
        .update_task(
            &t4,
            &UpdateTask {
                status: Some(TaskStatus::Blocked),
                blocked_reason: Some(format!("waiting on ask #{}: unrelated", ask4b.ask.id)),
                ..update_task_base("t3:admin".parse().unwrap(), t4_detail.task.version)
            },
        )
        .await
        .unwrap();
    server
        .user_client()
        .answer(ask4a.ask.id, "Approved — resume merge")
        .await
        .unwrap();
    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let t4_after = server.user_client().task(&t4).await.unwrap();
    assert_eq!(
        t4_after.task.status,
        TaskStatus::Blocked,
        "answered old dispatch ask must not unblock task held by another ask"
    );
}

#[tokio::test]
async fn merge_hol_unclear_and_deferred_do_not_starve_other_merges() {
    let server = TestServer::start().await;
    let alpha = server.create_autopilot_board("alpha", 2).await;

    // Create a second autopilot board `beta` with its own repository so we verify
    // zero cross-board Head-of-Line blocking in a single reconcile tick.
    const BETA_REPO: &str = "github.com/org/beta";
    let user = server.user_client();
    let beta_board = user
        .create_board(&CreateBoard {
            slug: "beta".parse().unwrap(),
            title: "Board beta".to_owned(),
            goal: String::new(),
            repos: vec![BETA_REPO.to_owned()],
            autopilot: false,
            home_devices: Vec::new(),
            temporary: false,
        })
        .await
        .unwrap();
    user.update_board(
        &beta_board.slug,
        &UpdateBoard {
            autopilot: Some(true),
            parallel_tasks: Some(2),
            ..UpdateBoard::default()
        },
    )
    .await
    .unwrap();
    let beta = beta_board.slug;

    let shared_gh = FakeGh::default();
    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, Some(shared_gh.clone()));
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();

    // 1. p1 unclear task on alpha (foreign PR outside alpha's repos -> MergeOutcome::Unclear).
    let unclear_task = make_reviewed_task(
        &server,
        &alpha,
        "Foreign PR unclear task",
        "review: pass. Verified.\npr: https://github.com/other/foreign/pull/37",
        "mac-pro",
        &mac_pro.token,
    )
    .await;
    let u_detail = user.task(&unclear_task).await.unwrap();
    user.update_task(
        &unclear_task,
        &UpdateTask {
            priority: Some(Priority::P1),
            ..update_task_base("t3:admin".parse().unwrap(), u_detail.task.version)
        },
    )
    .await
    .unwrap();

    // 2. p1 failing-CI task on alpha (-> MergeOutcome::Deferred with "checks failed ...").
    let failing_pr = format!("https://{REPO}/pull/500");
    shared_gh.put(open_pr(
        &failing_pr,
        "head-red-500",
        MergeState::Waits,
        Checks::Failed(vec!["rust".into()]),
    ));
    let failing_task = make_reviewed_task(
        &server,
        &alpha,
        "Failing CI p1 task",
        &format!("review: pass. LGTM.\npr: {failing_pr}"),
        "mac-pro",
        &mac_pro.token,
    )
    .await;
    let f_detail = user.task(&failing_task).await.unwrap();
    user.update_task(
        &failing_task,
        &UpdateTask {
            priority: Some(Priority::P1),
            ..update_task_base("t3:admin".parse().unwrap(), f_detail.task.version)
        },
    )
    .await
    .unwrap();

    // 3. p2 clean ready PR on beta (scores BELOW both p1 tasks!).
    let ready_pr = format!("https://{BETA_REPO}/pull/42");
    shared_gh.put(open_pr(
        &ready_pr,
        "head-clean-42",
        MergeState::Clean,
        Checks::Passed,
    ));
    let ready_task = make_reviewed_task(
        &server,
        &beta,
        "Ready clean PR on beta",
        &format!("review: pass. Ship it.\npr: {ready_pr}"),
        "mac-pro",
        &mac_pro.token,
    )
    .await;
    let r_detail = user.task(&ready_task).await.unwrap();
    user.update_task(
        &ready_task,
        &UpdateTask {
            repo: Some(Some(BETA_REPO.to_owned())),
            ..update_task_base("t3:admin".parse().unwrap(), r_detail.task.version)
        },
    )
    .await
    .unwrap();

    // ONE single reconcile tick must process unclear_task, failing_task, AND merge ready_task!
    mac_pro.daemon.reconcile(server.now()).await.unwrap();

    let ready_after = user.task(&ready_task).await.unwrap();
    assert_eq!(
        ready_after.task.status,
        TaskStatus::Done,
        "clean p2 PR on beta must merge in the very same tick despite p1 unclear/deferred tasks: {ready_after:?}"
    );
    assert!(
        shared_gh
            .calls()
            .iter()
            .any(|c| c == &format!("merge {ready_pr} head-clean-42")),
        "expected beta PR merge: {:?}",
        shared_gh.calls()
    );

    // unclear_task must complete as RunState::Succeeded with outcome status "unclear" (NEVER
    // Failed!) and post exactly ONE finding note.
    let unclear_runs = user
        .list_runs(&RunFilter {
            task: Some(unclear_task.clone()),
            kind: Some(RunKind::Merge),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(unclear_runs.len(), 1, "{unclear_runs:?}");
    assert_eq!(unclear_runs[0].state, RunState::Succeeded);
    assert_eq!(
        unclear_runs[0]
            .outcome
            .as_ref()
            .and_then(|o| o.get("status"))
            .and_then(serde_json::Value::as_str),
        Some("unclear"),
        "{:?}",
        unclear_runs[0].outcome
    );
    let unclear_after = user.task(&unclear_task).await.unwrap();
    assert_eq!(unclear_after.task.status, TaskStatus::Review);
    let unclear_findings: Vec<_> = unclear_after
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::Finding && n.text.contains("unclear pr:"))
        .collect();
    assert_eq!(
        unclear_findings.len(),
        1,
        "expected exactly one finding note for unclear PR: {:?}",
        unclear_after.notes
    );

    // Reconcile again 30s later and run sweeper: NO repeat merge runs for unclear_task or
    // failing_task, NO duplicate finding note, NO Failed runs, NO spawn_fail signal!
    server.advance(TimeDelta::seconds(30));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    server.run_sweeper_once().await;

    let unclear_runs_after = user
        .list_runs(&RunFilter {
            task: Some(unclear_task.clone()),
            kind: Some(RunKind::Merge),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(
        unclear_runs_after.len(),
        1,
        "unclear merge must stay suppressed with no retry loop: {unclear_runs_after:?}"
    );
    let failing_runs_after = user
        .list_runs(&RunFilter {
            task: Some(failing_task.clone()),
            kind: Some(RunKind::Merge),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(
        failing_runs_after.len(),
        1,
        "checks-failed merge must stay in 300s cooldown at 30s: {failing_runs_after:?}"
    );
    let all_failed = user
        .list_runs(&RunFilter {
            state: Some(RunState::Failed),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert!(
        all_failed.is_empty(),
        "MergeOutcome::Unclear must never record Failed runs: {all_failed:?}"
    );

    // Posting a newer `review: pass` note with `pr: none` re-offers unclear_task and finishes it!
    let reviewer_id: AgentId = "antigravity:reviewer-1".parse().unwrap();
    let reviewer_client = server.device_client(&mac_pro.token, Some(reviewer_id.clone()));
    reviewer_client
        .add_note(
            &unclear_task,
            &AddNote {
                agent: reviewer_id,
                kind: NoteKind::Decision,
                text: "review: pass. Corrected PR line.\npr: none".to_owned(),
            },
        )
        .await
        .unwrap();
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let unclear_done = user.task(&unclear_task).await.unwrap();
    assert_eq!(
        unclear_done.task.status,
        TaskStatus::Done,
        "newer review: pass note must re-offer and finish previously unclear task: {unclear_done:?}"
    );
}

#[tokio::test]
async fn pull_dispatch_runs_after_merge_review_judge_before_finishing_task() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 2).await;

    let shared_gh = FakeGh::default();
    let pr_url = format!("https://{REPO}/pull/205");
    shared_gh.put(open_pr(
        &pr_url,
        "head-after1",
        MergeState::Clean,
        Checks::Passed,
    ));

    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, Some(shared_gh.clone()));
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();

    let task_id = make_reviewed_task(
        &server,
        &board,
        "Deploy-checked feature",
        &format!(
            "review: pass. Feature code and unit tests pass.\ncheck: cargo test --workspace => 0\nafter merge: deploy/upgrade.sh --yes and verify /healthz\npr: {pr_url}"
        ),
        "mac-pro",
        &mac_pro.token,
    )
    .await;

    // Tick 1: pull dispatch claims RunKind::Merge -> DeterministicRunner merges the PR,
    // does NOT finish the task, writes the Trail Rule handoff note, and releases the lock.
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    assert!(
        shared_gh
            .calls()
            .iter()
            .any(|c| c == &format!("merge {pr_url} head-after1")),
        "expected PR merge call: {:?}",
        shared_gh.calls()
    );
    let after_merge_detail = server.user_client().task(&task_id).await.unwrap();
    assert_eq!(
        after_merge_detail.task.status,
        TaskStatus::Review,
        "task must stay in review until after-merge checks pass"
    );
    assert!(
        after_merge_detail.notes.iter().any(|n| {
            n.text
                .starts_with("note: merged; waiting for after-merge checks")
        }),
        "expected Trail Rule after-merge handoff note: {:?}",
        after_merge_detail.notes
    );

    // Tick 2 (or same reconcile loop): server now offers RunKind::Review ("after-merge ...")
    // and daemon spawns an LLM review judge with the AfterMerge brief.
    if mac_pro.runner.spawns().is_empty() {
        server.advance(TimeDelta::minutes(1));
        mac_pro.daemon.reconcile(server.now()).await.unwrap();
    }
    let spawns = mac_pro.runner.spawns();
    assert_eq!(
        spawns.len(),
        1,
        "expected one after-merge review spawn: {spawns:?}"
    );
    assert!(
        spawns[0].spec.title.starts_with("after-merge "),
        "unexpected title: {}",
        spawns[0].spec.title
    );
    assert!(
        spawns[0].spec.prompt.contains("## After-merge review")
            && spawns[0].spec.prompt.contains("deploy/upgrade.sh"),
        "expected AfterMerge brief: {}",
        spawns[0].spec.prompt
    );

    // Identify the spawned after-merge review run's agent id and have it post a clean
    // `review: pass` with post-merge check evidence (no `after merge:` lines).
    let active_reviews = server
        .user_client()
        .list_runs(&RunFilter {
            task: Some(task_id.clone()),
            kind: Some(RunKind::Review),
            active: Some(true),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(active_reviews.len(), 1, "{active_reviews:?}");
    let after_run = &active_reviews[0];
    let after_agent = after_run.agent_id.clone().expect("review run has agent_id");

    let judge_client = Client::new(
        &server.url,
        mac_pro.token.clone(),
        Some(after_agent.clone()),
        None,
    )
    .unwrap();
    judge_client
        .register(&RegisterAgent {
            id: after_agent.clone(),
            device: "mac-pro".parse().unwrap(),
            runtime: Runtime::Antigravity,
            role: AgentRole::Worker,
            repo: Some(REPO.to_owned()),
            branch: Some("main".to_owned()),
            cwd: None,
            title: None,
            forked_from: None,
            native_ids: BTreeMap::new(),
        })
        .await
        .unwrap();
    judge_client
        .add_note(
            &task_id,
            &AddNote {
                agent: after_agent,
                kind: NoteKind::Decision,
                text: format!(
                    "review: pass. Deployed and verified /healthz.\ncheck: deploy/upgrade.sh --yes {MERGE_COMMIT} => 0\npr: {pr_url}"
                ),
            },
        )
        .await
        .unwrap();
    server
        .user_client()
        .update_run(
            &after_run.id,
            &UpdateRun {
                state: Some(RunState::Succeeded),
                ..UpdateRun::default()
            },
        )
        .await
        .unwrap();

    // Tick 3: server offers RunKind::Merge again -> DeterministicRunner sees clean pass +
    // merged PR and finishes the task with BOTH pre-merge and post-merge check evidence!
    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let done_detail = server.user_client().task(&task_id).await.unwrap();
    assert_eq!(done_detail.task.status, TaskStatus::Done);
    assert!(
        done_detail.evidence.iter().any(|e| e.value == pr_url),
        "expected PR evidence: {:?}",
        done_detail.evidence
    );
    assert!(
        done_detail
            .evidence
            .iter()
            .any(|e| e.value == "cargo test --workspace" && e.exit_code == Some(0)),
        "expected preserved pre-merge check evidence: {:?}",
        done_detail.evidence
    );
    assert!(
        done_detail.evidence.iter().any(|e| {
            e.value == format!("deploy/upgrade.sh --yes {MERGE_COMMIT}") && e.exit_code == Some(0)
        }),
        "expected post-merge check evidence: {:?}",
        done_detail.evidence
    );
}

#[tokio::test]
async fn pull_dispatch_after_merge_handles_pr_none_and_reopens_on_loop_bypass() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 2).await;

    let shared_gh = FakeGh::default();
    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 1, Some(shared_gh.clone()));
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();

    // 1. `pr: none` task with `after merge:` checks -> writes handoff note (`pr: none`),
    //    runs after-merge judge, then finishes with both pre- and post-merge checks even
    //    if the after-merge pass omits the `pr:` line.
    let no_pr_task = make_reviewed_task(
        &server,
        &board,
        "Config-only post-merge check",
        "review: pass. Config verified.\ncheck: cargo check => 0\nafter merge: restart board watch on home device\npr: none",
        "mac-pro",
        &mac_pro.token,
    )
    .await;

    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let mid = server.user_client().task(&no_pr_task).await.unwrap();
    assert_eq!(mid.task.status, TaskStatus::Review);
    assert!(
        mid.notes.iter().any(|n| {
            n.text
                .starts_with("note: merged; waiting for after-merge checks")
                && n.text.contains("pr: none")
        }),
        "expected pr: none handoff note: {:?}",
        mid.notes
    );
    if mac_pro.runner.spawns().is_empty() {
        server.advance(TimeDelta::minutes(1));
        mac_pro.daemon.reconcile(server.now()).await.unwrap();
    }
    assert_eq!(mac_pro.runner.spawns().len(), 1);
    let active = server
        .user_client()
        .list_runs(&RunFilter {
            task: Some(no_pr_task.clone()),
            kind: Some(RunKind::Review),
            active: Some(true),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    let run1 = &active[0];
    let agent1 = run1.agent_id.clone().unwrap();
    let c1 = Client::new(
        &server.url,
        mac_pro.token.clone(),
        Some(agent1.clone()),
        None,
    )
    .unwrap();
    c1.register(&RegisterAgent {
        id: agent1.clone(),
        device: "mac-pro".parse().unwrap(),
        runtime: Runtime::Antigravity,
        role: AgentRole::Worker,
        repo: Some(REPO.to_owned()),
        branch: Some("main".to_owned()),
        cwd: None,
        title: None,
        forked_from: None,
        native_ids: BTreeMap::new(),
    })
    .await
    .unwrap();
    c1.add_note(
        &no_pr_task,
        &AddNote {
            agent: agent1,
            kind: NoteKind::Decision,
            text: "review: pass. Restarted board watch.\ncheck: launchctl kickstart => 0"
                .to_owned(),
        },
    )
    .await
    .unwrap();
    server
        .user_client()
        .update_run(
            &run1.id,
            &UpdateRun {
                state: Some(RunState::Succeeded),
                ..UpdateRun::default()
            },
        )
        .await
        .unwrap();

    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let done = server.user_client().task(&no_pr_task).await.unwrap();
    assert_eq!(done.task.status, TaskStatus::Done);
    assert!(done.evidence.iter().any(|e| e.value == "cargo check"));
    assert!(
        done.evidence
            .iter()
            .any(|e| e.value == "launchctl kickstart")
    );

    // 2. Loop guard in pull dispatch: after PR #206 merged, if the after-merge judge still
    //    writes `after merge:` AND switches to `pr: none`, DeterministicRunner voids the
    //    pass, adds a finding, reopens the task, and releases the merge lock.
    let pr_url = format!("https://{REPO}/pull/206");
    shared_gh.put(open_pr(
        &pr_url,
        "head-loop1",
        MergeState::Clean,
        Checks::Passed,
    ));
    let loop_task = make_reviewed_task(
        &server,
        &board,
        "Loop guard pull-dispatch check",
        &format!(
            "review: pass. Code ok.\ncheck: cargo test => 0\nafter merge: deploy/upgrade.sh --yes\npr: {pr_url}"
        ),
        "mac-pro",
        &mac_pro.token,
    )
    .await;

    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    if mac_pro.runner.spawns().len() < 2 {
        server.advance(TimeDelta::minutes(1));
        mac_pro.daemon.reconcile(server.now()).await.unwrap();
    }
    let active2 = server
        .user_client()
        .list_runs(&RunFilter {
            task: Some(loop_task.clone()),
            kind: Some(RunKind::Review),
            active: Some(true),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    let run2 = &active2[0];
    let agent2 = run2.agent_id.clone().unwrap();
    let c2 = Client::new(
        &server.url,
        mac_pro.token.clone(),
        Some(agent2.clone()),
        None,
    )
    .unwrap();
    c2.register(&RegisterAgent {
        id: agent2.clone(),
        device: "mac-pro".parse().unwrap(),
        runtime: Runtime::Antigravity,
        role: AgentRole::Worker,
        repo: Some(REPO.to_owned()),
        branch: Some("main".to_owned()),
        cwd: None,
        title: None,
        forked_from: None,
        native_ids: BTreeMap::new(),
    })
    .await
    .unwrap();
    c2.add_note(
        &loop_task,
        &AddNote {
            agent: agent2,
            kind: NoteKind::Decision,
            // Attempts to bypass by switching `pr:` to `none` while leaving `after merge:`.
            text: "review: pass. Not deployed yet.\nafter merge: deploy/upgrade.sh --yes\npr: none"
                .to_owned(),
        },
    )
    .await
    .unwrap();
    server
        .user_client()
        .update_run(
            &run2.id,
            &UpdateRun {
                state: Some(RunState::Succeeded),
                ..UpdateRun::default()
            },
        )
        .await
        .unwrap();

    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let reopened = server.user_client().task(&loop_task).await.unwrap();
    assert_eq!(reopened.task.status, TaskStatus::Open);
    assert!(
        reopened
            .notes
            .iter()
            .any(|n| n.kind == NoteKind::Finding && n.text.contains("left post-merge checks unrun")),
        "{:?}",
        reopened.notes
    );
    assert!(
        reopened
            .notes
            .iter()
            .any(|n| n.kind == NoteKind::Decision && n.text.starts_with("decision: review: void")),
        "{:?}",
        reopened.notes
    );
    let active_merges = server
        .user_client()
        .list_runs(&RunFilter {
            task: Some(loop_task),
            kind: Some(RunKind::Merge),
            active: Some(true),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert!(active_merges.is_empty(), "{active_merges:?}");
}

#[tokio::test]
async fn pull_dispatch_prepacks_worker_review_verdict_and_goal_with_static_prefix() {
    let server = TestServer::start().await;
    let board = server.create_autopilot_board("factory", 2).await;
    server
        .user_client()
        .update_board(
            &board,
            &UpdateBoard {
                goal: Some("Zero-waste autopilot runtime.".to_owned()),
                ..UpdateBoard::default()
            },
        )
        .await
        .unwrap();

    let shared_gh = FakeGh::default();
    let pr_url = format!("https://{REPO}/pull/77");
    shared_gh.put(PullRequest {
        url: pr_url.clone(),
        state: PrState::Open,
        draft: true,
        base_ref: "main".to_owned(),
        head_ref: "task/factory-2".to_owned(),
        head_oid: "cafebabe00112233".to_owned(),
        cross_repository: false,
        merge_commit: None,
        merge_state: MergeState::Clean,
        review_decision: ReviewDecision::None,
        checks: Checks::Passed,
        passed_checks: vec!["rust".to_owned(), "web".to_owned()],
    });

    let mut mac_pro = spawn_device_node(&server, "mac-pro", 12, 2, Some(shared_gh.clone()));
    mac_pro.daemon.send_heartbeat(server.now()).await.unwrap();

    // 1. Worker task: body, acceptance, and finding note must be pre-packed, without
    //    conditional Climbs/Revisits or `t3_thread_organize` on ACP runner.
    let worker_task = server
        .user_client()
        .create_task(&CreateTask {
            board: board.clone(),
            agent: "t3:admin".parse().unwrap(),
            title: "Optimize prepack pipeline".to_owned(),
            body: "Pre-pack task details for Turn 1 execution.".to_owned(),
            acceptance: "cargo test --workspace passes".to_owned(),
            priority: Priority::P1,
            blockers: Vec::new(),
            auto: Some(true),
            parent: None,
            not_before: None,
            claim: false,
            repo: Some(REPO.to_owned()),
            weight: None,
            resource_units: None,
            requires: Vec::new(),
            tolerations: Vec::new(),
            conflict_keys: Vec::new(),
            affinity: None,
            preferred_runner: None,
            rank: None,
            rank_top: false,
            before: None,
            after: None,
            origin: None,
            size: None,
        })
        .await
        .unwrap();
    let worker_agent: AgentId = "antigravity:seed-1".parse().unwrap();
    let seed_client = Client::new(
        &server.url,
        mac_pro.token.clone(),
        Some(worker_agent.clone()),
        None,
    )
    .unwrap();
    seed_client
        .register(&RegisterAgent {
            id: worker_agent.clone(),
            device: "mac-pro".parse().unwrap(),
            runtime: Runtime::Antigravity,
            role: AgentRole::Worker,
            repo: Some(REPO.to_owned()),
            branch: Some("main".to_owned()),
            cwd: None,
            title: None,
            forked_from: None,
            native_ids: BTreeMap::new(),
        })
        .await
        .unwrap();
    seed_client
        .add_note(
            &worker_task.id,
            &AddNote {
                agent: worker_agent.clone(),
                kind: NoteKind::Finding,
                text: "Preserve static-before-dynamic KV prefix".to_owned(),
            },
        )
        .await
        .unwrap();

    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let spawns = mac_pro.runner.spawns();
    assert_eq!(spawns.len(), 1, "{spawns:?}");
    let worker_prompt = &spawns[0].spec.prompt;
    assert!(
        worker_prompt.contains("Pre-pack task details for Turn 1 execution."),
        "{worker_prompt}"
    );
    assert!(
        worker_prompt.contains("Preserve static-before-dynamic KV prefix"),
        "{worker_prompt}"
    );
    assert!(
        !worker_prompt.contains("## Climbs")
            && !worker_prompt.contains("## Revisits")
            && !worker_prompt.contains("t3_thread_organize"),
        "non-climb/non-revisit ACP worker must omit conditional sections and t3 settle: {worker_prompt}"
    );
    let assign_idx = worker_prompt.find("## Assignment").unwrap();
    let prepack_idx = worker_prompt.find("<prepacked_board_data>").unwrap();
    assert!(assign_idx < prepack_idx);
    assert!(!worker_prompt[..assign_idx].contains(worker_task.id.as_str()));

    // Complete worker task and move a second task to `review` with PR #77 so Review judge runs.
    let active_runs = server
        .user_client()
        .list_runs(&RunFilter {
            active: Some(true),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    for run in active_runs {
        server
            .user_client()
            .update_run(
                &run.id,
                &UpdateRun {
                    state: Some(RunState::Succeeded),
                    ..UpdateRun::default()
                },
            )
            .await
            .unwrap();
    }
    let claimed_worker = seed_client
        .task_action(&worker_task.id, "claim")
        .await
        .unwrap();
    seed_client
        .done(
            &worker_task.id,
            &board_proto::DoneTask {
                agent: worker_agent.clone(),
                version: claimed_worker.version,
                evidence: vec![board_proto::NewEvidence {
                    kind: board_proto::EvidenceKind::Commit,
                    value: "abc1234".to_owned(),
                    exit_code: None,
                }],
                summary: Some("Shipped worker pre-packing.".to_owned()),
                take: false,
            },
        )
        .await
        .unwrap();

    // 2. Review task with PR metadata (`headRefOid`, `checks: passed`).
    let review_task = server
        .user_client()
        .create_task(&CreateTask {
            board: board.clone(),
            agent: "t3:admin".parse().unwrap(),
            title: "Verify PR prepack".to_owned(),
            body: "Check PR metadata in review brief.".to_owned(),
            acceptance: "CI passes on headRefOid".to_owned(),
            priority: Priority::P1,
            blockers: Vec::new(),
            auto: Some(true),
            parent: None,
            not_before: None,
            claim: false,
            repo: Some(REPO.to_owned()),
            weight: None,
            resource_units: None,
            requires: Vec::new(),
            tolerations: Vec::new(),
            conflict_keys: Vec::new(),
            affinity: None,
            preferred_runner: None,
            rank: None,
            rank_top: false,
            before: None,
            after: None,
            origin: None,
            size: None,
        })
        .await
        .unwrap();
    let claimed_review = seed_client
        .task_action(&review_task.id, "claim")
        .await
        .unwrap();
    let review_task = seed_client
        .update_task(
            &review_task.id,
            &UpdateTask {
                status: Some(TaskStatus::Review),
                ..update_task_base(worker_agent.clone(), claimed_review.version)
            },
        )
        .await
        .unwrap();
    seed_client
        .add_note(
            &review_task.id,
            &AddNote {
                agent: worker_agent.clone(),
                kind: NoteKind::Note,
                text: format!("parked in review; PR {pr_url}"),
            },
        )
        .await
        .unwrap();
    seed_client
        .set_state(
            &worker_agent,
            &board_proto::AgentStateUpdate {
                state: board_proto::AgentState::Idle,
                paused_until: None,
                said: None,
            },
        )
        .await
        .unwrap();

    // 3. Pending p1 proposal so Verdict judge runs immediately (bypasses debounce).
    let proposal = server
        .user_client()
        .create_task(&CreateTask {
            board: board.clone(),
            agent: "t3:admin".parse().unwrap(),
            title: "Trim stale snapshots".to_owned(),
            body:
                "Proposal: drop volatile minute drift. Evidence: logs. Change: snapshot.rs. Size S."
                    .to_owned(),
            acceptance: "snapshot tests pass".to_owned(),
            priority: Priority::P1,
            blockers: Vec::new(),
            auto: Some(false),
            parent: None,
            not_before: None,
            claim: false,
            repo: Some(REPO.to_owned()),
            weight: None,
            resource_units: None,
            requires: Vec::new(),
            tolerations: Vec::new(),
            conflict_keys: Vec::new(),
            affinity: None,
            preferred_runner: None,
            rank: None,
            rank_top: false,
            before: None,
            after: None,
            origin: None,
            size: None,
        })
        .await
        .unwrap();

    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let spawns = mac_pro.runner.spawns();
    let review_spawn = spawns
        .iter()
        .find(|s| s.spec.metadata.get("kind").map(String::as_str) == Some("review"))
        .expect("review judge spawned");
    assert!(
        review_spawn
            .spec
            .prompt
            .contains("headRefOid: cafebabe00112233")
            && review_spawn.spec.prompt.contains("checks: passed")
            && review_spawn.spec.prompt.contains(&pr_url),
        "review judge brief must pre-pack gh.view PR metadata: {}",
        review_spawn.spec.prompt
    );
    assert!(!review_spawn.spec.prompt.contains("t3_thread_organize"));

    let verdict_spawn = spawns
        .iter()
        .find(|s| s.spec.metadata.get("kind").map(String::as_str) == Some("verdict"))
        .expect("verdict judge spawned");
    assert!(
        verdict_spawn
            .spec
            .prompt
            .contains("### Board goal\nZero-waste autopilot runtime.")
            && verdict_spawn.spec.prompt.contains(proposal.id.as_str()),
        "verdict judge brief must pre-pack board.goal: {}",
        verdict_spawn.spec.prompt
    );
    assert!(!verdict_spawn.spec.prompt.contains("t3_thread_organize"));

    // 4. Close review + proposal and finish active runs so Goal judge triggers and receives
    //    pre-packed board.goal, counts, and recently finished tasks.
    let active_runs = server
        .user_client()
        .list_runs(&RunFilter {
            active: Some(true),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    for run in active_runs {
        server
            .user_client()
            .update_run(
                &run.id,
                &UpdateRun {
                    state: Some(RunState::Succeeded),
                    ..UpdateRun::default()
                },
            )
            .await
            .unwrap();
    }
    seed_client
        .update_task(
            &review_task.id,
            &UpdateTask {
                status: Some(TaskStatus::Dropped),
                reason: Some("test cleanup".to_owned()),
                ..update_task_base(worker_agent.clone(), review_task.version)
            },
        )
        .await
        .unwrap();
    server
        .user_client()
        .update_task(
            &proposal.id,
            &UpdateTask {
                status: Some(TaskStatus::Dropped),
                reason: Some("test cleanup".to_owned()),
                ..update_task_base("t3:admin".parse().unwrap(), proposal.version)
            },
        )
        .await
        .unwrap();

    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    let spawns = mac_pro.runner.spawns();
    let goal_spawn = spawns
        .iter()
        .find(|s| s.spec.metadata.get("kind").map(String::as_str) == Some("goal"))
        .expect("goal judge spawned");
    assert!(
        goal_spawn
            .spec
            .prompt
            .contains("### Board goal\nZero-waste autopilot runtime.")
            && goal_spawn.spec.prompt.contains(worker_task.id.as_str())
            && goal_spawn
                .spec
                .prompt
                .contains("Shipped worker pre-packing."),
        "goal judge brief must pre-pack board.goal and recent done tasks: {}",
        goal_spawn.spec.prompt
    );
    assert!(!goal_spawn.spec.prompt.contains("t3_thread_organize"));
}
