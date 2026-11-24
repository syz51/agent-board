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
use board::gh::{Checks, Gh, MergeState, PrState, PullRequest, Updated};
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
            })
            .await
            .unwrap();
        task.id
    }
}

fn update_task_base(agent: AgentId, version: i64) -> UpdateTask {
    UpdateTask {
        agent,
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
        checks,
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
    shared_gh.put(open_pr(
        &pr_url,
        "head-bbb222",
        MergeState::Waits,
        Checks::Failed(vec!["rust".into()]),
    ));
    server.advance(TimeDelta::minutes(1));
    mac_pro.daemon.reconcile(server.now()).await.unwrap();
    server.advance(TimeDelta::minutes(1));
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
    server.advance(TimeDelta::minutes(1));
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
