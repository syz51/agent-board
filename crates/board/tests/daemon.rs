//! The device daemon's start and wake decisions, with a fake `t3`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use board::client::Client;
use board::config::{Config, Home, JudgeModels, ThreadModel};
use board::daemon::{Daemon, RELEASE_GRACE};
use board::gh::{Checks, Gh, MergeState, PrState, PullRequest, ReviewDecision, Updated};
use board::identity::RepoInfo;
use board::ops::Ctx;
use board::t3::{
    ListedThread, NewWorktree, RecentMessages, SendMode, T3, ThreadInfo, TimedOut, UserMessage,
};
use board_proto::{
    AgentState, Ask, AskKind, AskStatus, Board, Event, EventType, Note, Runtime, TaskDetail,
    TaskStatus, Timestamp,
};
use common::*;
use futures_util::future::BoxFuture;
use serde_json::json;

const REPO: &str = "github.com/syz51/agent-board";
const NEW_THREAD: &str = "thread:project:p1:new";
const ORPHAN: &str = "thread:project:p1:orphan";
const MERGE_COMMIT: &str = "5f0c2d9a41b7";

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Start {
        project: PathBuf,
        title: String,
        message: String,
        worktree: Option<NewWorktree>,
        model: Option<ThreadModel>,
    },
    Send {
        thread: String,
        message: String,
        mode: SendMode,
    },
    List {
        project: PathBuf,
        title: String,
    },
    /// `t3 thread list`, read for the threads that run.
    Running(PathBuf),
}

/// `message` without the `(wake-up <digest>)` line a wake-up message ends with.
fn unmarked(message: &str) -> &str {
    match message.rsplit_once('\n') {
        Some((text, last)) if last.starts_with("(wake-up ") && last.ends_with(')') => text,
        _ => message,
    }
}

#[derive(Clone, Default)]
struct FakeT3 {
    calls: Arc<Mutex<Vec<Call>>>,
    threads: Arc<Mutex<BTreeMap<String, ThreadInfo>>>,
    /// `t3 thread start` fails while set.
    fail_start: Arc<AtomicBool>,
    /// `t3 thread start` fails in these projects.
    fail_start_in: Arc<Mutex<Vec<PathBuf>>>,
    /// `t3 thread start` never answers in these projects, as when git waits on a macOS
    /// prompt; `start_hung` is told once it hangs.
    hang_start_in: Arc<Mutex<Vec<PathBuf>>>,
    start_hung: Arc<tokio::sync::Notify>,
    /// How long `t3 thread start` takes before it answers.
    start_takes: Arc<Mutex<Duration>>,
    /// While set, `t3 thread start` creates [`ORPHAN`] and then fails, as when the daemon is
    /// killed or `t3` times out mid-start; the value says whether its message got through.
    die_after_create: Arc<Mutex<Option<bool>>>,
    /// `t3 thread list` and `t3 thread send` fail while set.
    fail_list: Arc<AtomicBool>,
    fail_send: Arc<AtomicBool>,
    /// `t3 thread send` runs out of time while set, after it may have delivered.
    time_out_send: Arc<AtomicBool>,
    /// Whether a send that ran out of time delivered its message first.
    time_out_delivers: Arc<AtomicBool>,
    /// The messages each thread got, oldest first, as `t3 thread show --messages` lists them.
    received: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
    /// Threads with a run active or queued.
    running: Arc<Mutex<Vec<String>>>,
    /// `t3 thread show --messages` fails while set.
    fail_messages: Arc<AtomicBool>,
    /// How many times the daemon read a thread's messages.
    message_reads: Arc<AtomicUsize>,
    /// Threads whose `t3 thread show` never answers.
    hang_info: Arc<Mutex<Vec<String>>>,
    /// `t3 thread show` fails while set.
    fail_info: Arc<AtomicBool>,
    /// How many times `t3 thread show` ran.
    infos: Arc<AtomicUsize>,
    /// How many `t3 thread show` run now, and the most that ever ran at once.
    infos_running: Arc<AtomicUsize>,
    infos_at_once: Arc<AtomicUsize>,
    /// Threads whose `t3 thread send` never answers.
    hang_send: Arc<Mutex<Vec<String>>>,
    /// Threads whose `t3 thread send` fails, as for an archived thread.
    refuse_send: Arc<Mutex<Vec<String>>>,
    /// What `t3 thread list` shows, by title.
    listed: Arc<Mutex<Vec<(String, ListedThread)>>>,
    /// When T3 creates a thread.
    clock: Arc<Mutex<Timestamp>>,
    /// Each thread's `runCount`, as `t3 thread show` gives it.
    runs: Arc<Mutex<BTreeMap<String, u64>>>,
    /// The ids of the next threads `t3 thread start` makes; [`NEW_THREAD`] once empty.
    ids: Arc<Mutex<std::collections::VecDeque<String>>>,
    /// The pull requests the daemon built on this fake reads and merges.
    gh: FakeGh,
}

/// GitHub's pull requests, by URL, and the `gh` commands the daemon ran on them.
#[derive(Clone, Default)]
struct FakeGh {
    prs: Arc<Mutex<BTreeMap<String, PullRequest>>>,
    calls: Arc<Mutex<Vec<String>>>,
    /// `gh pr update-branch` finds a conflict while set.
    conflicts: Arc<AtomicBool>,
    /// `gh pr ready` fails (leaving the PR a draft) while set.
    ready_fails: Arc<AtomicBool>,
}

impl FakeGh {
    fn put(&self, pr: PullRequest) {
        self.prs.lock().unwrap().insert(pr.url.clone(), pr);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn ran(&self, command: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(command))
            .count()
    }
}

impl Gh for FakeGh {
    fn view<'a>(&'a self, _: &'a Path, pr: &'a str) -> BoxFuture<'a, Result<PullRequest>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("view {pr}"));
            let prs = self.prs.lock().unwrap();
            prs.get(pr)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("gh pr view {pr}: not found"))
        })
    }

    fn ready<'a>(&'a self, _: &'a Path, pr: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("ready {pr}"));
            if self.ready_fails.load(Ordering::SeqCst) {
                anyhow::bail!("gh pr ready failed: permission denied");
            }
            let mut prs = self.prs.lock().unwrap();
            let pr = prs.get_mut(pr).unwrap();
            pr.draft = false;
            if pr.merge_state == MergeState::Waits {
                pr.merge_state = MergeState::Clean;
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
            // GitHub runs the checks again on the new head.
            let mut prs = self.prs.lock().unwrap();
            let pr = prs.get_mut(pr).unwrap();
            pr.merge_state = MergeState::Waits;
            pr.checks = Checks::Pending;
            Ok(Updated::Done)
        })
    }

    fn merge<'a>(&'a self, _: &'a Path, pr: &'a str, head: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(format!("merge {pr} at {head}"));
            let mut prs = self.prs.lock().unwrap();
            let pr = prs.get_mut(pr).unwrap();
            pr.state = PrState::Merged;
            pr.merge_commit = Some(MERGE_COMMIT.to_owned());
            Ok(())
        })
    }
}

impl FakeT3 {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// The messages sent, without the marker line that ends each wake-up message, which
    /// names a digest of its text and send time; [`Self::raw_sends`] keeps it.
    fn sends(&self) -> Vec<(String, String)> {
        self.raw_sends()
            .into_iter()
            .map(|(thread, message)| (thread, unmarked(&message).to_owned()))
            .collect()
    }

    fn raw_sends(&self) -> Vec<(String, String)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Send {
                    thread, message, ..
                } => Some((thread, message)),
                _ => None,
            })
            .collect()
    }

    fn sends_with_mode(&self) -> Vec<(String, String, SendMode)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Send {
                    thread,
                    message,
                    mode,
                } => Some((thread, unmarked(&message).to_owned(), mode)),
                _ => None,
            })
            .collect()
    }

    fn list(&self, title: &str, id: &str, created_at: Timestamp, ran: bool) {
        self.listed.lock().unwrap().push((
            title.to_owned(),
            ListedThread {
                id: id.to_owned(),
                created_at,
                ran,
            },
        ));
    }

    fn live(&self, thread: &str) {
        self.threads.lock().unwrap().insert(
            thread.to_owned(),
            ThreadInfo {
                archived: false,
                runtime: Runtime::Claude,
                worktree: None,
                runs: None,
                busy: false,
            },
        );
    }
}

impl T3 for FakeT3 {
    async fn start_thread(
        &self,
        project: &Path,
        title: &str,
        message: &str,
        worktree: Option<&NewWorktree>,
        model: Option<&ThreadModel>,
    ) -> Result<String> {
        self.calls.lock().unwrap().push(Call::Start {
            project: project.to_path_buf(),
            title: title.to_owned(),
            message: message.to_owned(),
            worktree: worktree.cloned(),
            model: model.cloned(),
        });
        if self
            .hang_start_in
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == project)
        {
            self.start_hung.notify_one();
            std::future::pending::<()>().await;
        }
        let takes = *self.start_takes.lock().unwrap();
        tokio::time::sleep(takes).await;
        if self.fail_start.load(Ordering::SeqCst)
            || self
                .fail_start_in
                .lock()
                .unwrap()
                .iter()
                .any(|p| p == project)
        {
            anyhow::bail!("t3 thread start failed: no such project");
        }
        let now = *self.clock.lock().unwrap();
        if let Some(ran) = *self.die_after_create.lock().unwrap() {
            self.live(ORPHAN);
            self.list(title, ORPHAN, now, ran);
            anyhow::bail!("t3 thread start timed out");
        }
        let id = self.ids.lock().unwrap().pop_front();
        let id = id.unwrap_or_else(|| NEW_THREAD.to_owned());
        self.live(&id);
        self.list(title, &id, now, true);
        Ok(id)
    }

    async fn send(&self, thread: &str, message: &str) -> Result<()> {
        self.send_with_mode(thread, message, SendMode::Queue).await
    }

    async fn send_with_mode(&self, thread: &str, message: &str, mode: SendMode) -> Result<()> {
        self.calls.lock().unwrap().push(Call::Send {
            thread: thread.to_owned(),
            message: message.to_owned(),
            mode,
        });
        if self.hang_send.lock().unwrap().iter().any(|t| t == thread) {
            std::future::pending::<()>().await;
        }
        if self.fail_send.load(Ordering::SeqCst) {
            anyhow::bail!("t3 thread send failed: server unavailable");
        }
        if self.refuse_send.lock().unwrap().iter().any(|t| t == thread) {
            anyhow::bail!("t3 thread send failed: thread {thread} is archived");
        }
        let timed_out = self.time_out_send.load(Ordering::SeqCst);
        if !timed_out || self.time_out_delivers.load(Ordering::SeqCst) {
            let mut received = self.received.lock().unwrap();
            received
                .entry(thread.to_owned())
                .or_default()
                .push(message.to_owned());
        }
        if timed_out {
            return Err(TimedOut(format!("t3 thread send {thread}")).into());
        }
        for (_, t) in self.listed.lock().unwrap().iter_mut() {
            if t.id == thread {
                t.ran = true;
            }
        }
        Ok(())
    }

    async fn thread_info(&self, thread: &str) -> Result<Option<ThreadInfo>> {
        self.infos.fetch_add(1, Ordering::SeqCst);
        let running = self.infos_running.fetch_add(1, Ordering::SeqCst) + 1;
        self.infos_at_once.fetch_max(running, Ordering::SeqCst);
        // Counted down when the check ends or is dropped on its timeout.
        struct Ended(Arc<AtomicUsize>);
        impl Drop for Ended {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _ended = Ended(self.infos_running.clone());
        if self.hang_info.lock().unwrap().iter().any(|t| t == thread) {
            std::future::pending::<()>().await;
        }
        if self.fail_info.load(Ordering::SeqCst) {
            anyhow::bail!("t3 thread show failed: server not running");
        }
        let runs = self.runs.lock().unwrap().get(thread).copied();
        let running = self.running.lock().unwrap().iter().any(|t| t == thread);
        Ok(self
            .threads
            .lock()
            .unwrap()
            .get(thread)
            .cloned()
            .map(|info| ThreadInfo {
                runs,
                busy: info.busy || running,
                ..info
            }))
    }

    async fn recent_messages(&self, thread: &str) -> Result<Option<RecentMessages>> {
        self.message_reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_messages.load(Ordering::SeqCst) {
            anyhow::bail!("t3 thread show failed: server not running");
        }
        if !self.threads.lock().unwrap().contains_key(thread) {
            return Ok(None);
        }
        Ok(Some(RecentMessages {
            busy: self.running.lock().unwrap().iter().any(|t| t == thread),
            // Each message started its own run.
            user: self
                .received
                .lock()
                .unwrap()
                .get(thread)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(i, text)| UserMessage {
                    text,
                    run: Some(format!("run:{i}")),
                })
                .collect(),
        }))
    }

    async fn thread_link(&self, thread: &str) -> Result<Option<String>> {
        Ok(self
            .threads
            .lock()
            .unwrap()
            .contains_key(thread)
            .then(|| format!("https://t3.example.test/threads/{thread}")))
    }

    async fn threads_titled(&self, project: &Path, title: &str) -> Result<Vec<ListedThread>> {
        self.calls.lock().unwrap().push(Call::List {
            project: project.to_path_buf(),
            title: title.to_owned(),
        });
        if self.fail_list.load(Ordering::SeqCst) {
            anyhow::bail!("t3 thread list failed: server not running");
        }
        Ok(self
            .listed
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == title)
            .map(|(_, thread)| thread.clone())
            .collect())
    }

    async fn running_threads(&self, project: &Path) -> Result<Vec<String>> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Running(project.to_path_buf()));
        if self.fail_list.load(Ordering::SeqCst) {
            anyhow::bail!("t3 thread list failed: server not running");
        }
        Ok(self.running.lock().unwrap().clone())
    }
}

struct Env {
    _dir: tempfile::TempDir,
    stub: Stub,
    t3: FakeT3,
    daemon: Daemon<FakeT3>,
}

/// A project folder named `name` that exists, since the daemon starts no thread in a
/// missing one; the same path on every call.
fn src(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("daemon-projects")
        .join(name);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn env() -> Env {
    env_with_projects(projects())
}

fn env_with_projects(projects: BTreeMap<String, PathBuf>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let t3 = FakeT3::default();
    let daemon = daemon_at(dir.path(), &stub, projects, &t3);
    Env {
        _dir: dir,
        stub,
        t3,
        daemon,
    }
}

/// A daemon whose home is under `dir`; called again on the same `dir`, a restarted one.
fn daemon_at(
    dir: &Path,
    stub: &Stub,
    projects: BTreeMap<String, PathBuf>,
    t3: &FakeT3,
) -> Daemon<FakeT3> {
    daemon_timed(dir, stub, projects, t3, None)
}

/// [`daemon_at`] with a client that gives up on an answer after `timeout`.
fn daemon_timed(
    dir: &Path,
    stub: &Stub,
    projects: BTreeMap<String, PathBuf>,
    t3: &FakeT3,
    timeout: Option<Duration>,
) -> Daemon<FakeT3> {
    let config = Config {
        server: Some(stub.url.clone()),
        device: Some(DEVICE.parse().unwrap()),
        projects,
        ..Config::default()
    };
    daemon_configured(dir, stub, t3, timeout, config)
}

/// A daemon whose home is under `dir`, with `config`.
fn daemon_configured(
    dir: &Path,
    stub: &Stub,
    t3: &FakeT3,
    timeout: Option<Duration>,
    config: Config,
) -> Daemon<FakeT3> {
    let ctx = Ctx {
        home: Home::at(dir.join("home")),
        device: DEVICE.parse().unwrap(),
        client: Client::new(&stub.url, TOKEN.into(), None, timeout).unwrap(),
        config,
        repo: RepoInfo::default(),
        known: None,
    };
    Daemon::new(ctx, t3.clone()).with_gh(t3.gh.clone())
}

fn autopilot(coordinator: Option<&str>, homes: &[&str]) -> Board {
    let mut board = board_fixture("factory", &[REPO]);
    board.autopilot = true;
    board.coordinator = coordinator.map(|c| c.parse().unwrap());
    board.home_devices = homes.iter().map(|d| d.parse().unwrap()).collect();
    board
}

fn agent_path(id: &str) -> String {
    format!("/v1/agents/{}", enc(id))
}

#[tokio::test]
async fn nothing_is_dispatched_off_home_devices_or_after_a_refused_claim() {
    // Not a home device.
    let mut e = env();
    e.stub.on("GET /v1/boards", [autopilot(None, &["mac-air"])]);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());
    assert!(dispatch_claims(&e).is_empty());

    // Another device holds the lease.
    let mut e = env();
    e.stub.on(
        "GET /v1/boards",
        [autopilot(Some("dispatch:mac-air"), &[DEVICE])],
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    e.stub.on_error(
        "POST /v1/boards/factory/coordinator",
        409,
        "claim_refused",
        "mac-air holds it",
        "wait",
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());
    assert_eq!(dispatch_claims(&e).len(), 1);
    assert!(e.stub.requests_to("GET /v1/tasks").is_empty());
}

/// An agent `t3:owner` of this device in the state `state`, whose thread `owner` is live, on a
/// board this device does not dispatch.
fn owner_env(state: AgentState, paused_until: Option<Timestamp>) -> (Env, String) {
    let e = env();
    let agent = "t3:owner".to_owned();
    e.t3.live("owner");
    let mut board = autopilot(None, &[DEVICE]);
    board.autopilot = false;
    e.stub.on("GET /v1/boards", [board]);
    let mut info = agent_fixture(&agent, DEVICE, state);
    info.paused_until = paused_until;
    e.stub.on(&format!("GET {}", agent_path(&agent)), &info);
    (e, agent)
}

fn event(seq: i64, kind: EventType, task: Option<&str>, actor: &str) -> Event {
    Event {
        seq,
        at: at(),
        board: Some("factory".parse().unwrap()),
        actor: actor.into(),
        event_type: kind,
        task: task.map(|t| t.parse().unwrap()),
        payload: json!({}),
    }
}

fn detail(id: &str, ready: bool, blocks: &[&str]) -> TaskDetail {
    let mut task = task_fixture(id, "Task");
    task.ready = ready;
    TaskDetail {
        task,
        notes: vec![],
        evidence: vec![],
        blocks: blocks.iter().map(|b| b.parse().unwrap()).collect(),
        children: vec![],
        child_tasks: vec![],
        history: vec![],
        handles: Default::default(),
    }
}

/// An env whose board is hosted elsewhere, with `factory-7` in doing, owned by the idle T3
/// thread `worker` on this device, and the server's notice that it has been away since
/// 40 minutes before [`at`].
fn idle_owner_env() -> (Env, Event, String) {
    let e = env();
    let owner = "t3:worker".to_owned();
    e.t3.live("worker");
    e.stub.on("GET /v1/boards", [autopilot(None, &["mac-air"])]);
    let mut doing = detail("factory-7", false, &[]);
    doing.task.status = TaskStatus::Doing;
    doing.task.owner = Some(owner.parse().unwrap());
    e.stub.on("GET /v1/tasks/factory-7", &doing);
    e.stub.on(
        &format!("GET {}", agent_path(&owner)),
        agent_fixture(&owner, DEVICE, AgentState::Idle),
    );
    let since = at() - chrono::Duration::minutes(40);
    let mut notice = event(30, EventType::TaskUpdated, Some("factory-7"), "server");
    notice.payload = json!({ "fields": [], "idle_doing": since, "away": owner });
    (e, notice, owner)
}

#[tokio::test]
async fn the_servers_idle_notice_wakes_the_owner_away_from_its_doing_task() {
    let logs = Logs::default();
    let _guard = logs.capture_at(tracing::Level::INFO);
    let (mut e, notice, _) = idle_owner_env();
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.on_event(&notice, at()).await.unwrap();
    e.daemon.flush(at()).await;
    let since = (at() - chrono::Duration::minutes(40)).format("%Y-%m-%d %H:%MZ");
    assert_eq!(
        e.t3.sends(),
        [(
            "worker".to_owned(),
            format!(
                "board factory: your factory-7 is in doing, and you have been away from it since \
                 {since}. Continue it, or park it by what it waits on: review with a note naming \
                 the PR, blocked with the reason, or open with your handoff\n\
                 Continue from this and the board delta in this prompt, if there is one."
            )
        )]
    );
    assert!(
        logs.text().contains("woke t3:worker: idle"),
        "{}",
        logs.text()
    );
    // The daemon reads no task list of its own: the server finds idle owners.
    assert!(e.stub.requests_to("GET /v1/tasks").is_empty());
}

#[tokio::test]
async fn an_owner_back_at_work_or_off_its_task_gets_no_idle_line() {
    // Back at work before the line went out: it no longer holds.
    let (mut e, notice, owner) = idle_owner_env();
    e.daemon.reconcile(at()).await.unwrap();
    let mut running = agent_fixture(&owner, DEVICE, AgentState::Running);
    running.last_seen = at();
    e.stub.on(&format!("GET {}", agent_path(&owner)), &running);
    e.daemon.on_event(&notice, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty(), "held while it runs");
    let mut back = event(31, EventType::AgentStateChanged, None, &owner);
    back.payload = json!({ "from": "idle", "to": "running", "paused_until": null });
    e.daemon.on_event(&back, at()).await.unwrap();
    let mut stopped = event(32, EventType::AgentStateChanged, None, &owner);
    stopped.payload = json!({ "from": "running", "to": "idle", "paused_until": null });
    e.daemon.on_event(&stopped, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());

    // The task left doing, or another agent owns it, before the send.
    for change in ["review", "handed"] {
        let (mut e, notice, _) = idle_owner_env();
        e.daemon.reconcile(at()).await.unwrap();
        let mut moved = detail("factory-7", false, &[]);
        if change == "review" {
            moved.task.status = TaskStatus::Review;
            moved.task.owner = Some("t3:worker".parse().unwrap());
        } else {
            moved.task.status = TaskStatus::Doing;
            moved.task.owner = Some("t3:other".parse().unwrap());
        }
        e.stub.on("GET /v1/tasks/factory-7", &moved);
        e.daemon.on_event(&notice, at()).await.unwrap();
        e.daemon.flush(at()).await;
        assert!(e.t3.sends().is_empty(), "{change}");
    }
}

#[tokio::test]
async fn an_idle_notice_for_another_devices_or_a_non_t3_owner_is_not_sent_here() {
    // Another device's owner: its own daemon wakes it.
    let (mut e, notice, owner) = idle_owner_env();
    e.stub.on(
        &format!("GET {}", agent_path(&owner)),
        agent_fixture(&owner, "mac-air", AgentState::Idle),
    );
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.on_event(&notice, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty());

    // A Claude Code owner has no thread to send to: nothing is queued.
    let (mut e, mut notice, _) = idle_owner_env();
    notice.payload["away"] = json!("claude:abc");
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.on_event(&notice, at()).await.unwrap();
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn an_owner_waiting_for_a_usage_limit_reset_gets_no_idle_line_at_the_reset() {
    for state in [AgentState::Paused, AgentState::Ended] {
        let (mut e, notice, owner) = idle_owner_env();
        let reset = at() + chrono::Duration::hours(1);
        let mut paused = agent_fixture(&owner, DEVICE, state);
        paused.paused_until = Some(reset);
        e.stub.on(&format!("GET {}", agent_path(&owner)), &paused);
        e.daemon.reconcile(at()).await.unwrap();
        e.daemon.on_event(&notice, at()).await.unwrap();
        e.daemon.flush(at()).await;
        assert!(e.daemon.state.wakes.is_empty(), "{state:?}");
        e.daemon.flush(reset + chrono::Duration::minutes(1)).await;
        assert!(e.t3.sends().is_empty(), "{state:?}");
    }
}

#[tokio::test]
async fn an_owner_left_running_that_shows_activity_after_the_notice_gets_no_idle_line() {
    // Marked running all along, so no state change announces it is back.
    for (seen, sent) in [
        (chrono::Duration::minutes(1), 0),
        (-chrono::Duration::minutes(40), 1),
    ] {
        let (mut e, notice, owner) = idle_owner_env();
        let mut running = agent_fixture(&owner, DEVICE, AgentState::Running);
        running.last_seen = at() + seen;
        e.stub.on(&format!("GET {}", agent_path(&owner)), &running);
        e.daemon.reconcile(at()).await.unwrap();
        e.daemon.on_event(&notice, at()).await.unwrap();
        // Held while it runs, then sent anyway unless it was seen since the notice.
        e.daemon.flush(at()).await;
        assert!(e.t3.sends().is_empty());
        e.daemon.flush(at() + chrono::Duration::minutes(4)).await;
        assert_eq!(e.t3.sends().len(), sent, "seen {seen}");
    }
}

#[tokio::test]
async fn idle_notice_is_deferred_while_t3_reports_busy_and_sent_once_idle() {
    use board::daemon::WAKE_HOLD_MAX;

    // 1. Worker is marked running past WAKE_HOLD_MAX (long command without board calls) and T3
    //    reports an active run (ThreadInfo::busy = true): defer instead of queueing an away notice.
    let (mut e, notice, owner) = idle_owner_env();
    let mut running = agent_fixture(&owner, DEVICE, AgentState::Running);
    running.last_seen = at() - chrono::Duration::minutes(40);
    e.stub.on(&format!("GET {}", agent_path(&owner)), &running);
    e.t3.running.lock().unwrap().push("worker".to_owned());
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.on_event(&notice, at()).await.unwrap();

    // First flush holds for WAKE_HOLD_MAX without querying T3 yet.
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty());
    assert_eq!(e.t3.infos.load(Ordering::SeqCst), 0);

    // Past WAKE_HOLD_MAX: checks T3::thread_info, sees busy=true, defers the idle wake and
    // records one Trail Rule hold entry.
    let t1 = at() + WAKE_HOLD_MAX;
    e.daemon.flush(t1).await;
    assert!(e.t3.sends().is_empty(), "must not queue behind active turn");
    assert_eq!(e.t3.infos.load(Ordering::SeqCst), 1);
    assert_eq!(e.daemon.state.wakes.len(), 1, "idle wake stays deferred");
    let drops: Vec<_> = read_daemon_drops(&e)
        .into_iter()
        .filter(|d| d["kind"] == "wake_idle_deferred_busy")
        .collect();
    assert_eq!(
        drops.len(),
        1,
        "expected one Trail Rule hold record: {drops:?}"
    );
    assert_eq!(drops[0]["task"], "factory-7");
    assert_eq!(drops[0]["agent"], owner);

    // Flushing again before WAKE_HOLD_MAX elapses does not re-poll T3 or duplicate the record.
    e.daemon.flush(t1 + chrono::Duration::seconds(30)).await;
    assert!(e.t3.sends().is_empty());
    assert_eq!(e.t3.infos.load(Ordering::SeqCst), 1);

    // At t1 + WAKE_HOLD_MAX while still busy: re-checks T3, stays deferred, still one record.
    let t2 = t1 + WAKE_HOLD_MAX;
    e.daemon.flush(t2).await;
    assert!(e.t3.sends().is_empty());
    assert_eq!(e.t3.infos.load(Ordering::SeqCst), 2);
    let drops_after: Vec<_> = read_daemon_drops(&e)
        .into_iter()
        .filter(|d| d["kind"] == "wake_idle_deferred_busy")
        .collect();
    assert_eq!(drops_after.len(), 1, "once per busy spell: {drops_after:?}");

    // Once T3 reports the turn finished (busy=false), the deferred idle wake sends on the next
    // re-check (using default Queue delivery).
    e.t3.running.lock().unwrap().clear();
    let t3 = t2 + WAKE_HOLD_MAX;
    e.daemon.flush(t3).await;
    let sent = e.t3.sends_with_mode();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, "worker");
    assert_eq!(sent[0].2, SendMode::Queue);
    assert!(sent[0].1.contains("you have been away from it"), "{sent:?}");
    assert!(e.daemon.state.wakes.is_empty());

    // 2. If the agent is marked Idle or Ended on the server while T3 still reports busy=true,
    //    defer immediately; if the agent reports activity before going idle, drop without sending.
    for state in [AgentState::Idle, AgentState::Ended] {
        let (mut e, notice, owner) = idle_owner_env();
        let mut info = agent_fixture(&owner, DEVICE, state);
        info.last_seen = at() - chrono::Duration::minutes(40);
        e.stub.on(&format!("GET {}", agent_path(&owner)), &info);
        e.t3.running.lock().unwrap().push("worker".to_owned());
        e.daemon.reconcile(at()).await.unwrap();
        e.daemon.on_event(&notice, at()).await.unwrap();

        e.daemon.flush(at()).await;
        assert!(e.t3.sends().is_empty(), "{state:?}: deferred while busy");
        assert_eq!(e.daemon.state.wakes.len(), 1, "{state:?}");

        // Agent emits Idle state change (clearing idle_busy_until) while T3 finishes, and
        // was seen after the notice -> back_since suppresses the stale away notice.
        e.t3.running.lock().unwrap().clear();
        let mut active = agent_fixture(&owner, DEVICE, AgentState::Running);
        active.last_seen = at() + chrono::Duration::seconds(10);
        e.stub.on(&format!("GET {}", agent_path(&owner)), &active);
        let stopped = state_changed(31, &owner, AgentState::Idle, None);
        e.daemon
            .on_event(&stopped, at() + chrono::Duration::seconds(15))
            .await
            .unwrap();
        e.daemon.flush(at() + chrono::Duration::seconds(15)).await;
        assert!(e.t3.sends().is_empty(), "{state:?}: suppressed once back");
        assert!(e.daemon.state.wakes.is_empty(), "{state:?}");
    }
}

#[tokio::test]
async fn urgent_ask_answer_bypasses_running_hold_and_uses_auto_mode_while_idle_wake_stays_deferred()
{
    // Case A: Agent is Running (inside WAKE_HOLD_MAX) with both an Idle notice and an Answered
    // ask. The ask answer is delivered immediately with SendMode::Auto; the idle notice stays
    // deferred and is not bundled into the steer message.
    let (mut e, notice, owner) = idle_owner_env();
    let mut running = agent_fixture(&owner, DEVICE, AgentState::Running);
    running.last_seen = at() - chrono::Duration::minutes(40);
    e.stub.on(&format!("GET {}", agent_path(&owner)), &running);
    e.t3.running.lock().unwrap().push("worker".to_owned());
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.on_event(&notice, at()).await.unwrap();
    let ans = answered(&e, 31, 9, &owner);
    e.daemon.on_event(&ans, at()).await.unwrap();

    e.daemon.flush(at()).await;
    let sent = e.t3.sends_with_mode();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, "worker");
    assert_eq!(sent[0].2, SendMode::Auto);
    assert!(
        sent[0].1.contains("the user answered ask #9"),
        "{}",
        sent[0].1
    );
    assert!(
        !sent[0].1.contains("you have been away"),
        "idle notice must not bundle with urgent steer: {}",
        sent[0].1
    );
    assert_eq!(
        e.daemon.state.wakes[&owner.parse().unwrap()].wakes.len(),
        1,
        "idle wake remains queued"
    );

    // Case B: Agent is Idle on the board while T3 still reports busy=true, with both Idle and
    // Answered wakes queued: Answered sends with SendMode::Auto while Idle is deferred_busy.
    let (mut e, notice, owner) = idle_owner_env();
    e.t3.running.lock().unwrap().push("worker".to_owned());
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.on_event(&notice, at()).await.unwrap();
    let ans = answered(&e, 31, 10, &owner);
    e.daemon.on_event(&ans, at()).await.unwrap();

    e.daemon.flush(at()).await;
    let sent = e.t3.sends_with_mode();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, "worker");
    assert_eq!(sent[0].2, SendMode::Auto);
    assert!(
        sent[0].1.contains("the user answered ask #10"),
        "{}",
        sent[0].1
    );
    assert!(
        !sent[0].1.contains("you have been away"),
        "busy idle notice must not bundle with ask answer: {}",
        sent[0].1
    );
    assert_eq!(
        e.daemon.state.wakes[&owner.parse().unwrap()].wakes.len(),
        1,
        "busy idle wake stays deferred"
    );
}

/// A blocker on another board: its done event comes from that board, and wakes the
/// owner of a task blocked on it.
#[tokio::test]
async fn a_blocker_done_on_another_board_wakes_the_owner_of_a_task_it_blocked() {
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Idle),
    );
    let mut done = detail(
        "other-3",
        false,
        &["factory-12", "factory-13", "factory-14"],
    );
    done.task.status = board_proto::TaskStatus::Done;
    e.stub.on("GET /v1/tasks/other-3", &done);
    e.stub
        .on("GET /v1/tasks/factory-12", detail("factory-12", true, &[]));
    // Blocked by the worker on other-3 alone; and one that still waits on factory-2.
    let mut blocked = detail("factory-13", false, &[]);
    blocked.task.status = board_proto::TaskStatus::Blocked;
    blocked.task.owner = Some("t3:worker".parse().unwrap());
    blocked.task.blockers = vec!["other-3".parse().unwrap()];
    e.stub.on("GET /v1/tasks/factory-13", &blocked);
    let mut still = blocked.clone();
    still.task.id = "factory-14".parse().unwrap();
    still.task.blockers = vec!["other-3".parse().unwrap(), "factory-2".parse().unwrap()];
    e.stub.on("GET /v1/tasks/factory-14", &still);
    e.stub
        .on("GET /v1/tasks/factory-2", detail("factory-2", false, &[]));

    let mut ev = event(20, EventType::TaskDone, Some("other-3"), "t3:elsewhere");
    ev.board = Some("other".parse().unwrap());
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends(),
        [(
            "worker".to_owned(),
            "board factory: every blocker of your blocked factory-13 is done; resume it (task_update factory-13 status=doing)\nContinue from this and the board delta in this prompt, if there is one."
                .to_owned()
        )]
    );
    assert!(e.daemon.state.wakes.is_empty());

    // Checked again when sent: an owner who resumed already is not woken.
    e.daemon.on_event(&ev, at()).await.unwrap();
    blocked.task.status = board_proto::TaskStatus::Doing;
    e.stub.on("GET /v1/tasks/factory-13", &blocked);
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends().len(), 1);
    assert!(e.daemon.state.wakes.is_empty());

    // A not_before still ahead holds the move to doing: the wake names the date.
    blocked.task.status = board_proto::TaskStatus::Blocked;
    blocked.task.not_before = Some(at() + chrono::Duration::days(1));
    e.stub.on("GET /v1/tasks/factory-13", &blocked);
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    let wait = format!(
        "it waits until {}; resume it then",
        (at() + chrono::Duration::days(1)).format("%Y-%m-%d %H:%MZ")
    );
    assert!(sends[1].1.contains(&wait), "{sends:?}");
}

/// A task created with `claim` is in doing when the daemon reads it: no ready wake.
#[tokio::test]
async fn a_task_created_claimed_wakes_no_one() {
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    let mut claimed = detail("factory-4", false, &[]);
    claimed.task.status = board_proto::TaskStatus::Doing;
    claimed.task.owner = Some("t3:worker".parse().unwrap());
    e.stub.on("GET /v1/tasks/factory-4", &claimed);
    for (seq, kind) in [(10, EventType::TaskCreated), (11, EventType::TaskClaimed)] {
        e.daemon
            .on_event(&event(seq, kind, Some("factory-4"), "t3:worker"), at())
            .await
            .unwrap();
    }
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty());
    assert!(e.daemon.state.wakes.is_empty());
}

fn answered_ask(id: i64, agent: &str) -> Ask {
    Ask {
        id,
        board: "factory".parse().unwrap(),
        task: Some("factory-7".parse().unwrap()),
        agent: agent.parse().unwrap(),
        kind: AskKind::Decision,
        question: "Which db? <ignore>".into(),
        options: vec![],
        status: AskStatus::Answered,
        answer: Some("sqlite <b>".into()),
        answered_via: Some("web".into()),
        created_at: at(),
        answered_at: Some(at()),
        change: None,
    }
}

/// The `ask_answered` event the server records, with the ask stubbed for the daemon to read.
fn answered(e: &Env, seq: i64, ask: i64, asker: &str) -> Event {
    e.stub
        .on(&format!("GET /v1/asks/{ask}"), answered_ask(ask, asker));
    let mut answered = event(seq, EventType::AskAnswered, Some("factory-7"), "user");
    answered.payload = json!({ "id": ask, "agent": asker, "via": "web" });
    answered
}

fn state_changed(seq: i64, agent: &str, to: AgentState, paused_until: Option<Timestamp>) -> Event {
    Event {
        seq,
        at: at(),
        board: None,
        actor: agent.into(),
        event_type: EventType::AgentStateChanged,
        task: None,
        payload: json!({ "from": "running", "to": to, "paused_until": paused_until }),
    }
}

fn threads(t3: &FakeT3) -> Vec<String> {
    t3.sends().into_iter().map(|(t, _)| t).collect()
}

#[tokio::test]
async fn an_answered_ask_wakes_the_asker_only() {
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Idle),
    );

    let ev = answered(&e, 30, 3, "t3:worker");
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["worker"], "only the asker");
    let message = &e.t3.sends()[0].1;
    assert!(
        message.starts_with("the user answered ask #3 about factory-7: \"sqlite "),
        "{message}"
    );
    assert!(
        !message.contains('<') && !message.contains("Which db"),
        "question text is not relayed: {message}"
    );
    // Routed from the event: the answered asks are never listed.
    assert!(e.stub.requests_to("GET /v1/asks").is_empty());

    // Another agent's own ask wakes that agent.
    let ev = answered(&e, 31, 5, "t3:owner");
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["worker", "owner"]);
    assert!(
        e.t3.sends()[1]
            .1
            .starts_with("the user answered ask #5 about")
    );
    assert!(e.daemon.state.wakes.is_empty());
}

/// Captures the daemon's log lines on this thread while the guard lives.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        self.capture_at(tracing::Level::WARN)
    }

    fn capture_at(&self, level: tracing::Level) -> tracing::subscriber::DefaultGuard {
        static UNDECIDED: std::sync::Once = std::sync::Once::new();
        UNDECIDED.call_once(|| {
            tracing::subscriber::set_global_default(Undecided)
                .expect("no other global subscriber in this test binary");
        });
        let logs = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || logs.clone())
            .with_ansi(false)
            .with_max_level(level)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// The global subscriber while tests capture logs: it records nothing and leaves every
/// callsite's interest at "sometimes", so each event asks its own thread's subscriber.
///
/// tracing caches each callsite's interest for the whole process. While a single scoped
/// subscriber is registered, tracing computes that interest from the default of whichever
/// thread hits the callsite first. A test thread without a capture caches `never` there,
/// and the capturing test then loses the line. A global subscriber that is always
/// registered keeps tracing from taking that shortcut.
struct Undecided;

impl tracing::Subscriber for Undecided {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }

    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        false
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_capture_keeps_a_line_another_thread_without_one_logged_first() {
    fn line() {
        tracing::info!("logged from both threads");
    }
    let logs = Logs::default();
    let _guard = logs.capture_at(tracing::Level::INFO);
    std::thread::spawn(line).join().unwrap();
    line();
    assert!(
        logs.text().contains("logged from both threads"),
        "{}",
        logs.text()
    );
}

#[tokio::test]
async fn an_ended_agent_whose_thread_t3_still_has_is_woken() {
    // T3 closes a Claude session left idle for 30 minutes, which marks its agent ended; the
    // thread lives on, and a message resumes it.
    let (mut e, _) = owner_env(AgentState::Ended, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-4"], at()).await;
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends(),
        [(
            "owner".to_owned(),
            "board factory: every blocker of your blocked factory-4 is done; resume it (task_update factory-4 status=doing)\nContinue from this and the board delta in this prompt, if there is one.".to_owned()
        )]
    );

    // An ended asker whose thread is live hears its own answer.
    e.t3.live("worker");
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Ended),
    );
    let ev = answered(&e, 11, 3, "t3:worker");
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["owner", "worker"]);
    assert!(
        e.t3.sends()[1]
            .1
            .starts_with("the user answered ask #3 about factory-7")
    );
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn an_ended_agent_whose_thread_is_archived_is_not_woken_and_says_so() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let (mut e, _) = owner_env(AgentState::Ended, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.t3.threads.lock().unwrap().insert(
        "owner".into(),
        ThreadInfo {
            archived: true,
            runtime: Runtime::Claude,
            worktree: None,
            runs: None,
            busy: false,
        },
    );
    queue_unblocked(&mut e, 10, &["factory-4"], at()).await;
    // Its board is not dispatched here, so the answer to its ask has no one to go to.
    let ev = answered(&e, 11, 3, "t3:owner");
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty());
    assert!(e.daemon.state.wakes.is_empty());
    let text = logs.text();
    let gone =
        "waking t3:owner: T3 no longer has its thread, archived it, or the thread's folder is gone";
    assert_eq!(
        text.matches(&format!("{gone}; 1 wake-ups dropped")).count(),
        1,
        "{text}"
    );
    assert_eq!(
        text.matches(&format!(
            "{gone}; 1 answers to its asks or idle notices dropped: this device does not \
             dispatch their boards"
        ))
        .count(),
        1,
        "{text}"
    );
}

#[tokio::test]
async fn an_ended_thread_t3_cannot_check_is_tried_and_not_asked_about_each_flush() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Ended),
    );
    let ev = answered(&e, 11, 3, "t3:worker");
    e.daemon.on_event(&ev, at()).await.unwrap();
    // T3 is down: the check fails, the send is tried and fails, and it backs off.
    e.t3.fail_info.store(true, Ordering::SeqCst);
    e.t3.fail_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["worker"]);
    e.daemon.flush(at() + chrono::Duration::seconds(3)).await;
    let text = logs.text();
    assert_eq!(
        text.matches("waking t3:worker: checking its ended thread: ")
            .count(),
        1,
        "asked once until its state changes: {text}"
    );

    // T3 is back: the send after the back-off resumes the thread.
    e.t3.fail_info.store(false, Ordering::SeqCst);
    e.t3.fail_send.store(false, Ordering::SeqCst);
    e.t3.live("worker");
    e.daemon.flush(at() + chrono::Duration::seconds(31)).await;
    assert_eq!(threads(&e.t3), ["worker", "worker"]);
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn a_paused_askers_answer_is_kept_until_its_reset() {
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    // A usage limit that resets well after the wake-ups' time to live.
    let reset = at() + board::daemon::WAKE_TTL * 2;
    let mut paused = agent_fixture("t3:worker", DEVICE, AgentState::Paused);
    paused.paused_until = Some(reset);
    e.stub
        .on(&format!("GET {}", agent_path("t3:worker")), &paused);

    let ev = answered(&e, 30, 3, "t3:worker");
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty(), "kept");

    // Kept past the time to live while the limit holds, and survives a restart.
    let before = reset - chrono::Duration::minutes(1);
    e.daemon.flush(before).await;
    assert!(e.t3.sends().is_empty());
    e.daemon.save().unwrap();
    let mut restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    restarted.reconcile(at()).await.unwrap();
    restarted.flush(before).await;
    assert!(e.t3.sends().is_empty(), "kept after a restart");

    // After the reset the asker gets it.
    restarted.flush(reset + chrono::Duration::minutes(1)).await;
    assert_eq!(threads(&e.t3), ["worker"]);
    assert!(
        e.t3.sends()[0]
            .1
            .starts_with("the user answered ask #3 about factory-7")
    );
    assert!(restarted.state.wakes.is_empty());
}

fn task_now(id: &str, status: board_proto::TaskStatus, ready: bool, auto: bool) -> TaskDetail {
    let mut d = detail(id, ready, &[]);
    d.task.status = status;
    d.task.auto = auto;
    d
}

/// Stubs the done task `blocker` as the one blocker of `id`, a blocked task of `t3:owner`.
fn unblocks(e: &Env, blocker: &str, id: &str) {
    let mut done = detail(blocker, false, &[id]);
    done.task.status = TaskStatus::Done;
    e.stub.on(&format!("GET /v1/tasks/{blocker}"), &done);
    let mut blocked = detail(id, false, &[]);
    blocked.task.status = TaskStatus::Blocked;
    blocked.task.owner = Some("t3:owner".parse().unwrap());
    blocked.task.blockers = vec![blocker.parse().unwrap()];
    e.stub.on(&format!("GET /v1/tasks/{id}"), &blocked);
}

/// Queues a wake-up for `t3:owner` per task of `ids` at `now`: its blocked task, whose one
/// blocker is done.
async fn queue_unblocked(e: &mut Env, first_seq: i64, ids: &[&str], now: Timestamp) {
    for (seq, id) in (first_seq..).zip(ids) {
        let blocker = format!("factory-9{seq}");
        unblocks(e, &blocker, id);
        e.daemon
            .on_event(
                &event(seq, EventType::TaskDone, Some(&blocker), "t3:worker"),
                now,
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_queued_line_is_checked_again_when_sent() {
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    let ids = ["factory-1", "factory-2", "factory-3", "factory-4"];
    queue_unblocked(&mut e, 10, &ids, at()).await;

    // Before the flush: one resumed, one done, and one handed to another agent.
    e.stub.on(
        "GET /v1/tasks/factory-1",
        task_now("factory-1", TaskStatus::Doing, false, true),
    );
    e.stub.on(
        "GET /v1/tasks/factory-2",
        task_now("factory-2", TaskStatus::Done, false, true),
    );
    let mut handed = task_now("factory-3", TaskStatus::Blocked, false, true);
    handed.task.owner = Some("t3:worker".parse().unwrap());
    e.stub.on("GET /v1/tasks/factory-3", &handed);
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends(),
        [(
            "owner".to_owned(),
            "board factory: every blocker of your blocked factory-4 is done; resume it (task_update factory-4 status=doing)\nContinue from this and the board delta in this prompt, if there is one."
                .to_owned()
        )]
    );

    // Nothing left: a later flush sends nothing.
    assert!(e.daemon.state.wakes.is_empty());
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends().len(), 1);
}

#[tokio::test]
async fn lines_for_a_running_agent_are_held_and_sent_as_one_message_when_it_goes_idle() {
    let (mut e, agent) = owner_env(AgentState::Running, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.stub.clear();
    let info = format!("GET {}", agent_path(&agent));
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;
    e.daemon.flush(at()).await;
    queue_unblocked(&mut e, 11, &["factory-2"], at()).await;
    e.daemon.flush(at() + chrono::Duration::seconds(3)).await;
    assert!(e.t3.sends().is_empty(), "held while it runs");
    // Held lines survive a restart.
    e.daemon.save().unwrap();
    assert_eq!(
        saved_state(&e).wakes[&agent.parse().unwrap()].wakes.len(),
        2
    );

    // The event stream reports it idle: the next flush sends, without asking the server.
    let soon = at() + chrono::Duration::minutes(1);
    e.daemon
        .on_event(&state_changed(12, &agent, AgentState::Idle, None), soon)
        .await
        .unwrap();
    e.daemon.flush(soon).await;
    assert_eq!(
        e.t3.sends(),
        [(
            "owner".to_owned(),
            "board factory: every blocker of your blocked factory-1 is done; resume it (task_update factory-1 status=doing)\nboard factory: every blocker of your blocked factory-2 is done; resume it (task_update factory-2 status=doing)\nContinue from this and the board delta in this prompt, if there is one."
                .to_owned()
        )]
    );
    assert!(e.daemon.state.wakes.is_empty());
    assert_eq!(e.stub.requests_to(&info).len(), 1, "read once, not polled");
}

#[tokio::test]
async fn a_thread_left_marked_running_is_woken_after_a_few_minutes() {
    use board::daemon::WAKE_HOLD_MAX;
    assert!(WAKE_HOLD_MAX <= chrono::Duration::minutes(5));
    // A turn that ended on an API error or an interrupt leaves no Stop hook to mark it idle.
    let (mut e, _) = owner_env(AgentState::Running, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;
    e.daemon.flush(at()).await;
    e.daemon
        .flush(at() + WAKE_HOLD_MAX - chrono::Duration::seconds(1))
        .await;
    assert!(e.t3.sends().is_empty());
    e.daemon.flush(at() + WAKE_HOLD_MAX).await;
    assert_eq!(e.t3.sends().len(), 1);
}

#[tokio::test]
async fn a_failed_send_backs_off_and_stops_once_the_thread_is_gone() {
    use board::daemon::WAKE_RETRY_FIRST;
    use board_proto::TaskStatus;
    let (mut e, agent) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1", "factory-2"], at()).await;
    e.t3.fail_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends().len(), 1, "tried once");
    // Not on every flush.
    e.daemon.flush(at() + chrono::Duration::seconds(3)).await;
    assert_eq!(e.t3.sends().len(), 1, "backing off");

    // Meanwhile factory-1 was claimed: the retry carries factory-2 only.
    e.t3.fail_send.store(false, Ordering::SeqCst);
    e.stub.on(
        "GET /v1/tasks/factory-1",
        task_now("factory-1", TaskStatus::Doing, false, true),
    );
    let next = at() + WAKE_RETRY_FIRST;
    e.daemon.flush(next).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!(
        sends[1].1,
        "board factory: every blocker of your blocked factory-2 is done; resume it (task_update factory-2 status=doing)\nContinue from this and the board delta in this prompt, if there is one."
    );
    e.daemon.flush(next).await;
    assert_eq!(e.t3.sends().len(), 2, "sent once");

    // A send that keeps failing waits twice as long each time.
    e.t3.fail_send.store(true, Ordering::SeqCst);
    queue_unblocked(&mut e, 12, &["factory-2"], next).await;
    e.daemon.flush(next).await;
    assert_eq!(e.t3.sends().len(), 3);
    let backoff = |e: &Env| {
        e.daemon.state.wakes[&agent.parse().unwrap()]
            .backoff
            .clone()
            .unwrap()
    };
    assert_eq!(backoff(&e).retry_at, next + WAKE_RETRY_FIRST);
    e.daemon.flush(next + WAKE_RETRY_FIRST).await;
    assert_eq!(e.t3.sends().len(), 4);
    assert_eq!(backoff(&e).retry_at, next + WAKE_RETRY_FIRST * 3);
    e.daemon.flush(next + WAKE_RETRY_FIRST * 2).await;
    assert_eq!(e.t3.sends().len(), 4);

    // A thread T3 no longer has is not tried again.
    e.t3.threads.lock().unwrap().remove("owner");
    e.daemon.flush(next + WAKE_RETRY_FIRST * 3).await;
    assert_eq!(e.t3.sends().len(), 5);
    assert!(e.daemon.state.wakes.is_empty());
}

/// The timed-out wake-up message the daemon waits to look for in `agent`'s thread.
fn unsure_wakes(e: &Env, agent: &str) -> Option<board::daemon::UnsureWakes> {
    e.daemon
        .state
        .unsure_wakes
        .get(&agent.parse().unwrap())
        .cloned()
}

#[tokio::test]
async fn a_timed_out_wake_up_the_thread_never_got_is_sent_again_once() {
    let (mut e, agent) = owner_env(AgentState::Idle, None);
    let reads = |e: &Env| e.t3.message_reads.load(Ordering::SeqCst);
    let line = "board factory: every blocker of your blocked factory-1 is done; resume it (task_update factory-1 status=doing)\nContinue from this and the board delta in this prompt, if there is one.";
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends(), [("owner".to_owned(), line.to_owned())]);
    // Recorded by what it was about and a digest its marker names, never its text.
    let unsure = unsure_wakes(&e, &agent).expect("looked for later");
    assert_eq!(unsure.messages.len(), 1);
    let unsure = &unsure.messages[0];
    assert_eq!(unsure.since, at());
    assert_eq!(unsure.wakes.len(), 1);
    assert!(
        e.t3.raw_sends()[0]
            .1
            .ends_with(&format!("\n{}", board::daemon::wake_marker(&unsure.digest)))
    );
    let saved = serde_json::to_string(&e.daemon.state).unwrap();
    assert!(!saved.contains("is ready"), "{saved}");
    assert!(e.daemon.state.wakes.is_empty());

    // Not looked for while T3 may still handle the send.
    e.daemon.flush(at() + chrono::Duration::seconds(30)).await;
    assert_eq!((e.t3.sends().len(), reads(&e)), (1, 0));

    // Missing a minute later, with no run that might still take it: sent again, once.
    e.daemon.flush(minute(1)).await;
    assert_eq!(reads(&e), 1);
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!(sends[1].1, line);
    let raw = e.t3.raw_sends();
    assert_ne!(raw[0].1, raw[1].1, "each message has its own marker");
    // That send timed out too: it counts as delivered.
    assert_eq!(unsure_wakes(&e, &agent), None);
    e.daemon.flush(minute(30)).await;
    assert_eq!((e.t3.sends().len(), reads(&e)), (2, 1));
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn a_timed_out_wake_up_that_reached_the_thread_is_not_sent_twice() {
    let (mut e, agent) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.t3.time_out_delivers.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    assert!(unsure_wakes(&e, &agent).is_some());

    // A line queued meanwhile waits for the check, so it goes with the lost lines if any.
    let soon = at() + chrono::Duration::seconds(10);
    queue_unblocked(&mut e, 11, &["factory-2"], soon).await;
    e.daemon.flush(soon).await;
    assert_eq!(e.t3.sends().len(), 1);

    // Found: settled, and only the new line goes.
    e.daemon.flush(minute(1)).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!(
        sends[1].1,
        "board factory: every blocker of your blocked factory-2 is done; resume it (task_update factory-2 status=doing)\nContinue from this and the board delta in this prompt, if there is one."
    );
    assert_eq!(unsure_wakes(&e, &agent), None);
    e.daemon.flush(minute(30)).await;
    assert_eq!(e.t3.sends().len(), 2);
}

#[tokio::test]
async fn a_timed_out_wake_up_waits_for_a_busy_thread_and_a_failed_check_sends_nothing() {
    use board::daemon::WAKE_RETRY_FIRST;
    let (mut e, agent) = owner_env(AgentState::Idle, None);
    let reads = |e: &Env| e.t3.message_reads.load(Ordering::SeqCst);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    e.t3.time_out_send.store(false, Ordering::SeqCst);

    // A run active or queued may still take it: looked for again a minute later, not on
    // every flush.
    e.t3.running.lock().unwrap().push("owner".to_owned());
    e.daemon.flush(minute(1)).await;
    assert_eq!((e.t3.sends().len(), reads(&e)), (1, 1));
    assert_eq!(
        unsure_wakes(&e, &agent).unwrap().read_after,
        Some(minute(2))
    );
    e.daemon
        .flush(minute(1) + chrono::Duration::seconds(30))
        .await;
    assert_eq!((e.t3.sends().len(), reads(&e)), (1, 1));
    e.t3.running.lock().unwrap().clear();

    // A failed check backs off and sends nothing.
    e.t3.fail_messages.store(true, Ordering::SeqCst);
    e.daemon.flush(minute(2)).await;
    assert_eq!((e.t3.sends().len(), reads(&e)), (1, 2));
    let unsure = unsure_wakes(&e, &agent).unwrap();
    assert_eq!(unsure.read_after, Some(minute(2) + WAKE_RETRY_FIRST));
    assert_eq!(
        unsure.failed_delay_secs,
        Some(WAKE_RETRY_FIRST.num_seconds())
    );
    e.t3.fail_messages.store(false, Ordering::SeqCst);
    // While it backs off, nothing is read, and new lines are no longer held for it; their
    // delivered send leaves it to be looked for.
    let soon = minute(2) + chrono::Duration::seconds(10);
    queue_unblocked(&mut e, 11, &["factory-2"], soon).await;
    e.daemon.flush(soon).await;
    assert_eq!(reads(&e), 2);
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!(
        sends[1].1,
        "board factory: every blocker of your blocked factory-2 is done; resume it (task_update factory-2 status=doing)\nContinue from this and the board delta in this prompt, if there is one."
    );
    assert_eq!(unsure_wakes(&e, &agent), Some(unsure));

    // Its retry finds it missing: its line is sent again.
    e.daemon.flush(minute(2) + WAKE_RETRY_FIRST).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 3, "{sends:?}");
    assert_eq!(
        sends[2].1,
        "board factory: every blocker of your blocked factory-1 is done; resume it (task_update factory-1 status=doing)\nContinue from this and the board delta in this prompt, if there is one."
    );
    assert_eq!(unsure_wakes(&e, &agent), None);
}

#[tokio::test]
async fn a_timed_out_wake_up_is_looked_for_after_a_restart() {
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    e.daemon.save().unwrap();
    // Simulate migrating a legacy watch.json with an unreadable entry: that entry is skipped,
    // not the rest of the state.
    let state_dir = home_of(&e).state_dir();
    let mut saved = serde_json::to_value(&e.daemon.state).unwrap();
    saved["unsure_wakes"]["t3:other"] = json!({ "messages": 5 });
    for suffix in ["daemon.db", "daemon.db-wal", "daemon.db-shm"] {
        let _ = std::fs::remove_file(state_dir.join(suffix));
    }
    std::fs::write(
        state_dir.join("watch.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();

    let mut restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    assert_eq!(restarted.state.unsure_wakes, e.daemon.state.unsure_wakes);
    assert_eq!(restarted.state.unsure_wakes.len(), 1);
    restarted.reconcile(minute(1)).await.unwrap();
    restarted.flush(minute(1)).await;
    assert_eq!(e.t3.sends().len(), 2);
    assert!(restarted.state.unsure_wakes.is_empty());
}

#[tokio::test]
async fn wake_ups_past_their_time_to_live_are_dropped_before_sending() {
    use board::daemon::WAKE_TTL;
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;
    e.daemon.flush(at() + WAKE_TTL).await;
    assert!(e.t3.sends().is_empty(), "not sent at the boundary");
    assert!(e.daemon.state.wakes.is_empty());

    // Saved, then read by a daemon restarted hours later.
    queue_unblocked(&mut e, 11, &["factory-2"], at()).await;
    e.daemon.save().unwrap();
    let mut restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    restarted.reconcile(at()).await.unwrap();
    restarted.flush(at() + chrono::Duration::hours(5)).await;
    assert!(e.t3.sends().is_empty());
    assert!(restarted.state.wakes.is_empty());
}

#[tokio::test]
async fn a_wake_up_this_version_cannot_read_leaves_the_rest_of_the_state() {
    let (mut e, agent) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;
    e.daemon.state.last_seq = Some(42);
    e.daemon.save().unwrap();
    let state_dir = home_of(&e).state_dir();
    for suffix in ["daemon.db", "daemon.db-wal", "daemon.db-shm"] {
        let _ = std::fs::remove_file(state_dir.join(suffix));
    }
    let path = state_dir.join("watch.json");
    let mut saved = serde_json::to_value(&e.daemon.state).unwrap();
    // A kind from a newer version, a damaged queue and an id no agent has.
    saved["wakes"][&agent]["wakes"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "board": "factory",
            "about": { "kind": "reviewed", "task": "factory-2" },
            "queued_at": at(),
        }));
    saved["wakes"]["t3:other"] = json!({ "wakes": "not a list" });
    saved["wakes"]["not an id"] = json!({ "wakes": [] });
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();

    let state = board::daemon::load_watch_state(&home_of(&e));
    assert_eq!(state.last_seq, Some(42));
    assert_eq!(state.wakes.len(), 1);
    assert_eq!(
        state.wakes[&agent.parse().unwrap()].wakes,
        e.daemon.state.wakes[&agent.parse().unwrap()].wakes
    );
    // A state file from before wake-ups were saved still loads.
    std::fs::write(&path, br#"{"last_seq": 7}"#).unwrap();
    let state = board::daemon::load_watch_state(&home_of(&e));
    assert_eq!(state.last_seq, Some(7));
    assert!(state.wakes.is_empty());
}

fn factory() -> board_proto::BoardSlug {
    "factory".parse().unwrap()
}

#[tokio::test]
async fn releasing_the_coordinator_reads_the_board_the_server_returns() {
    let stub = Stub::start();
    stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    let client = Client::new(&stub.url, TOKEN.into(), None, None).unwrap();
    let board = client.release_coordinator(&factory()).await.unwrap();
    assert_eq!(board.slug, factory());
    assert_eq!(board.coordinator, None);
}

#[tokio::test]
async fn a_device_without_the_project_never_claims_the_lease() {
    let mut e = env_with_projects(BTreeMap::new());
    e.daemon = e
        .daemon
        .with_dispatch_mode(board::config::DispatchMode::Legacy);
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    for minutes in [0, 1, 5, 60] {
        e.daemon
            .reconcile(at() + chrono::Duration::minutes(minutes))
            .await
            .unwrap();
    }
    assert!(
        e.stub
            .requests()
            .iter()
            .all(|r| r.method == "GET" && ["/v1/boards", "/v1/usage"].contains(&r.path.as_str())),
        "only board and usage reads: {:?}",
        e.stub.requests()
    );
    assert!(e.t3.calls().is_empty());
}

fn releases(e: &Env) -> usize {
    e.stub
        .requests_to("DELETE /v1/boards/factory/coordinator")
        .len()
}

fn saved_state(e: &Env) -> board::daemon::WatchState {
    board::daemon::load_watch_state(&Home::at(e._dir.path().join("home")))
}

fn projects() -> BTreeMap<String, PathBuf> {
    [(REPO.to_owned(), src("agent-board"))].into()
}

#[tokio::test]
async fn an_event_that_failed_on_a_transient_error_is_retried() {
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.state.last_seq = Some(9);
    let page = |events: Vec<Event>, next_since: i64| board_proto::EventsPage { events, next_since };
    e.stub.on(
        "GET /v1/events",
        page(
            vec![event(10, EventType::TaskDone, Some("factory-4"), "user")],
            10,
        ),
    );
    e.stub.on_error(
        "GET /v1/tasks/factory-4",
        503,
        "internal",
        "restarting",
        "retry",
    );
    e.daemon.catch_up(at()).await.unwrap();
    assert_eq!(e.daemon.state.last_seq, Some(10));
    assert_eq!(e.daemon.state.retry.len(), 1);
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty());

    // The server is back: the wake-up goes out.
    unblocks(&e, "factory-4", "factory-1");
    e.daemon
        .retry_failed(at() + chrono::Duration::minutes(1))
        .await;
    assert!(e.daemon.state.retry.is_empty());
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends().len(), 1);

    // A refused request is not retried.
    e.stub.on(
        "GET /v1/events",
        page(
            vec![event(11, EventType::TaskDone, Some("factory-5"), "user")],
            11,
        ),
    );
    e.daemon.catch_up(at()).await.unwrap();
    assert_eq!(e.daemon.state.last_seq, Some(11));
    assert!(e.daemon.state.retry.is_empty(), "a 404 is final");

    // A failure that outlasts the retry window is dropped.
    e.stub.on_error(
        "GET /v1/tasks/factory-4",
        503,
        "internal",
        "restarting",
        "retry",
    );
    e.daemon.state.retry = vec![event(10, EventType::TaskDone, Some("factory-4"), "user")];
    e.stub.clear();
    e.daemon
        .retry_failed(at() + chrono::Duration::minutes(31))
        .await;
    assert!(e.daemon.state.retry.is_empty());
    assert!(e.stub.requests().is_empty(), "dropped without a request");
}

/// An event type a newer server added is read as unknown and passed over: the daemon keeps
/// its place in the feed and still handles the events after it.
#[tokio::test]
async fn an_event_type_from_a_newer_server_is_passed_over() {
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.state.last_seq = Some(9);
    let mut newer =
        serde_json::to_value(event(10, EventType::TaskDone, Some("factory-4"), "user")).unwrap();
    newer["type"] = json!("board_renamed");
    let known =
        serde_json::to_value(event(11, EventType::TaskDone, Some("factory-5"), "user")).unwrap();
    e.stub.on(
        "GET /v1/events",
        json!({ "events": [newer, known], "next_since": 11 }),
    );
    unblocks(&e, "factory-5", "factory-1");
    e.daemon.catch_up(at()).await.unwrap();
    assert_eq!(e.daemon.state.last_seq, Some(11));
    assert!(e.stub.requests_to("GET /v1/tasks/factory-4").is_empty());
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends().len(),
        1,
        "the known event still wakes the owner"
    );
}

#[cfg(unix)]
#[test]
fn watch_backs_off_reconnecting_and_stops_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    stub.on("GET /healthz", json!({"ok": true, "seq": 5}));
    stub.on("GET /v1/boards", Vec::<Board>::new());
    // Not an event stream: every connection ends at once.
    stub.on(
        "GET /v1/events",
        board_proto::EventsPage {
            events: vec![],
            next_since: 5,
        },
    );
    let home = home_for(dir.path(), &stub.url);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_board"))
        .arg("watch")
        .env("BOARD_HOME", &home)
        .env("BOARD_TOKEN", TOKEN)
        .env("BOARD_KEYCHAIN", "0")
        .env_remove("T3CODE_THREAD_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("OPENCODE_SESSION_ID")
        .env_remove("ANTIGRAVITY_CONVERSATION_ID")
        .env_remove("BOARD_AGENT")
        .env_remove("BOARD_LOG")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let streams = || stub.requests_to("GET /v1/events").len();
    let deadline = Instant::now() + Duration::from_secs(10);
    while streams() == 0 {
        assert!(Instant::now() < deadline, "the daemon never connected");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(2500));
    let reconnects = streams();
    assert!(reconnects <= 3, "reconnects back off: {reconnects} in 2.5s");

    let killed = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the daemon ignored SIGTERM");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status:?}");
    let saved = board::daemon::load_watch_state(&Home::at(home));
    assert_eq!(saved.last_seq, Some(5), "state saved on the way out");
    // What it did is in watch.log (its stderr) at the default log level.
    let mut log = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut log).unwrap();
    for line in [
        "watching as device mac-test",
        "event stream open after #5",
        "event stream ended; reconnecting in 1.0s",
        "event stream ended; reconnecting in 2.0s",
        "stopping",
    ] {
        assert!(log.contains(line), "{line:?} not in:\n{log}");
    }
}

fn home_of(e: &Env) -> Home {
    Home::at(e._dir.path().join("home"))
}

/// Reconciles once with `config` on a home device of the autopilot `board`, whose lease is
/// free and whose task `<slug>-1` is ready, and returns the fake `t3` and the stub.
async fn start_with(board: Board, config: Config) -> (FakeT3, Stub) {
    let (t3, stub, _, _) = start_daemon(board, config).await;
    (t3, stub)
}

/// [`start_with`], keeping the daemon and its home.
async fn start_daemon(
    mut board: Board,
    config: Config,
) -> (FakeT3, Stub, Daemon<FakeT3>, tempfile::TempDir) {
    board.autopilot = true;
    board.home_devices = vec![DEVICE.parse().unwrap()];
    let slug = board.slug.clone();
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    stub.on("GET /v1/boards", [board.clone()]);
    board.coordinator = Some(DISPATCH.parse().unwrap());
    stub.on(&format!("POST /v1/boards/{slug}/coordinator"), board);
    stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    let task = ready_task(&format!("{slug}-1"), board_proto::Priority::P2, 0);
    stub.on(
        &format!("GET /v1/tasks/{}", task.id),
        task_detail_for(task.clone()),
    );
    stub.on(&format!("POST /v1/tasks/{}/notes", task.id), note(&task.id));
    stub.on("GET /v1/tasks", [task]);
    let t3 = FakeT3::default();
    let ctx = Ctx {
        home: Home::at(dir.path().join("home")),
        device: DEVICE.parse().unwrap(),
        client: Client::new(&stub.url, TOKEN.into(), None, None).unwrap(),
        config: Config {
            server: Some(stub.url.clone()),
            device: Some(DEVICE.parse().unwrap()),
            ..config
        },
        repo: RepoInfo::default(),
        known: None,
    };
    let mut daemon = Daemon::new(ctx, t3.clone());
    daemon.reconcile(at()).await.unwrap();
    (t3, stub, daemon, dir)
}

fn started(t3: &FakeT3) -> (PathBuf, String) {
    let calls = t3.calls();
    let [
        Call::Start {
            project, message, ..
        },
    ] = calls.as_slice()
    else {
        panic!("{calls:?}");
    };
    (project.clone(), message.clone())
}

#[tokio::test]
async fn a_board_without_a_mapped_repo_starts_in_its_board_projects_path() {
    let config = Config {
        board_projects: [("board-feedback".parse().unwrap(), src("feedback"))].into(),
        ..Config::default()
    };
    let (t3, _) = start_with(board_fixture("board-feedback", &[]), config).await;
    let (project, _) = started(&t3);
    assert_eq!(project, src("feedback"));

    // A board repo under [projects] wins over [board_projects].
    let config = Config {
        projects: projects(),
        board_projects: [(factory(), src("elsewhere"))].into(),
        ..Config::default()
    };
    let (t3, _) = start_with(board_fixture("factory", &[REPO]), config).await;
    assert_eq!(started(&t3).0, src("agent-board"));
}

/// Runs git in `dir` with no user or system config, and panics when it fails.
fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

#[tokio::test]
async fn a_git_project_starts_the_worker_in_its_own_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "--quiet", "--initial-branch=trunk"]);
    git(
        &origin,
        &["commit", "--quiet", "--allow-empty", "-m", "first"],
    );
    git(dir.path(), &["clone", "--quiet", "origin", "repo"]);
    let repo = dir.path().join("repo");

    let config = Config {
        projects: [(REPO.to_owned(), repo.clone())].into(),
        ..Config::default()
    };
    let (t3, _) = start_with(board_fixture("factory", &[REPO]), config.clone()).await;
    let calls = t3.calls();
    let [
        Call::Start {
            project, worktree, ..
        },
    ] = calls.as_slice()
    else {
        panic!("{calls:?}");
    };
    assert_eq!(project, &repo, "T3 finds the project by its folder");
    // The remote's default branch, not `main`, and a stable branch per task.
    let expected = NewWorktree {
        base: "origin/trunk".into(),
        branch: "task/factory-1".into(),
    };
    assert_eq!(worktree.as_ref(), Some(&expected));

    // A re-start leaves the branch and worktree an earlier worker left, which a live thread
    // may still use, and takes a branch of its own.
    let old = dir.path().join("old");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "task/factory-1",
            old.to_str().unwrap(),
            "origin/trunk",
        ],
    );
    let (t3, _) = start_with(board_fixture("factory", &[REPO]), config).await;
    let calls = t3.calls();
    let [Call::Start { worktree, .. }] = calls.as_slice() else {
        panic!("{calls:?}");
    };
    let branch = worktree.as_ref().map(|w| w.branch.as_str()).unwrap_or("");
    assert!(branch.starts_with("task/factory-1-"), "{worktree:?}");
    assert!(old.exists());

    // A folder outside git starts in the folder itself.
    let config = Config {
        board_projects: [(factory(), dir.path().join("plain"))].into(),
        ..Config::default()
    };
    std::fs::create_dir(dir.path().join("plain")).unwrap();
    let (t3, _) = start_with(board_fixture("factory", &[]), config).await;
    let calls = t3.calls();
    let [Call::Start { worktree, .. }] = calls.as_slice() else {
        panic!("{calls:?}");
    };
    assert_eq!(worktree, &None);
}

/// An env whose daemon has no project for the factory board yet, nor a `config.toml`.
fn unmapped_env() -> Env {
    let e = env_with_projects(BTreeMap::new());
    let task = ready_task("factory-1", board_proto::Priority::P2, 0);
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(Some(DISPATCH), &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    e.stub.on(
        &format!("GET /v1/tasks/{}", task.id),
        task_detail_for(task.clone()),
    );
    e.stub
        .on(&format!("POST /v1/tasks/{}/notes", task.id), note(&task.id));
    e.stub.on("GET /v1/tasks", [task]);
    e
}

/// Writes the env daemon's `config.toml` with the given projects.
fn write_config(e: &Env, projects: BTreeMap<String, PathBuf>) {
    Config {
        server: Some(e.stub.url.clone()),
        device: Some(DEVICE.parse().unwrap()),
        projects,
        ..Config::default()
    }
    .save(&Home::at(e._dir.path().join("home")))
    .unwrap();
}

#[tokio::test]
async fn a_project_added_to_config_toml_starts_the_worker_without_a_restart() {
    let mut e = unmapped_env();

    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty(), "no project: nothing to start");
    assert!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .is_empty()
    );

    // Mapped while the daemon runs: the next reconcile starts it, at the same instant, so
    // the failed start's back-off does not hold it either.
    write_config(&e, projects());
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(started(&e.t3).0, src("agent-board"));
}

#[tokio::test]
async fn an_unreadable_config_toml_keeps_the_projects_in_use() {
    let mut e = unmapped_env();
    write_config(&e, BTreeMap::new());
    let home = Home::at(e._dir.path().join("home"));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());

    std::fs::write(home.config_path(), "projects = [not toml").unwrap();
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty(), "an invalid file changes nothing");
    std::fs::remove_file(home.config_path()).unwrap();
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty(), "a missing file changes nothing");

    // Fixed: read again.
    write_config(&e, projects());
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(started(&e.t3).0, src("agent-board"));
}

/// An env whose daemon follows the stub's event stream after seq 5, with a client that
/// waits `request` for an answer and steps short enough for a test.
fn following_env(request: Duration) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let t3 = FakeT3::default();
    let mut daemon = daemon_timed(dir.path(), &stub, projects(), &t3, Some(request));
    daemon.state.last_seq = Some(5);
    daemon.timing = board::daemon::Timing {
        reconcile_every: Duration::from_secs(60),
        flush_every: Duration::from_millis(100),
        heartbeat_every: Duration::from_secs(60),
        stream_idle: Duration::from_secs(60),
        reconcile_budget: Duration::from_secs(60),
        flush_budget: Duration::from_millis(300),
        stop_budget: Duration::from_secs(1),
        thread_check: Duration::from_secs(10),
        reconnect_first: Duration::from_secs(1),
    };
    stub.on("GET /v1/boards", Vec::<Board>::new());
    Env {
        _dir: dir,
        stub,
        t3,
        daemon,
    }
}

/// Follows the stream until `until` holds (checked every 20 ms) or `give_up` passes, then
/// asks the daemon to stop; returns how long that took and how long stopping took.
async fn follow_until(
    e: &mut Env,
    give_up: Duration,
    until: impl Fn(&Stub, &FakeT3) -> bool,
) -> (Duration, Duration) {
    let (stopper, mut shutdown) = board::daemon::Shutdown::manual();
    let started = Instant::now();
    let (stub, t3) = (e.stub.clone(), e.t3.clone());
    let watch = async {
        while !until(&stub, &t3) && started.elapsed() < give_up {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let reached = started.elapsed();
        stopper.stop();
        (reached, Instant::now())
    };
    let (followed, (reached, asked)) =
        tokio::time::timeout(give_up + Duration::from_secs(5), async {
            tokio::join!(e.daemon.follow(&mut shutdown), watch)
        })
        .await
        .expect("the daemon did not stop");
    followed.unwrap();
    (reached, asked.elapsed())
}

#[tokio::test]
async fn a_stalled_server_still_lets_wake_ups_through_and_the_daemon_stop() {
    let mut e = following_env(Duration::from_millis(500));
    // The reconcile that runs first hangs on the board list for an hour.
    e.stub.delay("GET /v1/boards", Duration::from_secs(3600));
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Idle),
    );
    let ev = answered(&e, 6, 3, "t3:worker");
    e.stub.on_stream("GET /v1/events", vec![ev]);

    let (reached, stopping) = follow_until(&mut e, Duration::from_secs(10), |_, t3| {
        !t3.sends().is_empty()
    })
    .await;
    assert_eq!(threads(&e.t3), ["worker"]);
    assert!(
        reached < Duration::from_secs(3),
        "woken {reached:?} after start, despite the hanging reconcile"
    );
    assert!(stopping < Duration::from_secs(2), "stopped in {stopping:?}");
    assert_eq!(saved_state(&e).last_seq, Some(6));
}

#[tokio::test]
async fn a_stop_during_a_hanging_reconcile_and_flush_exits_and_keeps_the_wake_ups() {
    // Long enough that only the race with the stop request ends the hanging steps.
    let mut e = following_env(Duration::from_secs(3600));
    e.stub.delay("GET /v1/boards", Duration::from_secs(3600));
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Idle),
    );
    e.stub.delay(
        &format!("GET {}", agent_path("t3:worker")),
        Duration::from_secs(3600),
    );
    e.stub.on_stream("GET /v1/events", vec![]);
    let ev = answered(&e, 6, 3, "t3:worker");
    e.daemon.on_event(&ev, at()).await.unwrap();

    let (_, stopping) = follow_until(&mut e, Duration::from_millis(500), |_, _| false).await;
    assert!(stopping < Duration::from_secs(3), "stopped in {stopping:?}");
    assert!(e.t3.sends().is_empty());
    let saved = saved_state(&e);
    assert_eq!(
        saved.wakes.keys().map(|a| a.as_str()).collect::<Vec<_>>(),
        ["t3:worker"],
        "the wake-up stays saved for the next start"
    );
}

#[tokio::test]
async fn a_silent_stream_is_opened_again_after_the_last_event() {
    let mut e = following_env(Duration::from_secs(5));
    e.daemon.timing.stream_idle = Duration::from_millis(300);
    let mut ev = event(6, EventType::TaskUpdated, None, "t3:someone");
    ev.board = None;
    e.stub.on_stream("GET /v1/events", vec![ev]);

    let (reached, _) = follow_until(&mut e, Duration::from_secs(10), |stub, _| {
        stub.requests_to("GET /v1/events").len() >= 2
    })
    .await;
    assert!(
        reached < Duration::from_secs(5),
        "reconnected after {reached:?}"
    );
    let opened = e.stub.requests_to("GET /v1/events");
    assert_eq!(opened[0].query_pairs()["since"], "5");
    assert_eq!(
        opened[1].query_pairs()["since"],
        "6",
        "resumes after the event"
    );
}

#[tokio::test]
async fn a_failed_agent_lookup_waits_for_its_back_off() {
    use board::daemon::WAKE_RETRY_FIRST;
    let mut e = env();
    let route = format!("GET {}", agent_path("t3:worker"));
    e.stub
        .on_error(&route, 503, "unavailable", "restarting", "try again");
    let ev = answered(&e, 6, 3, "t3:worker");
    e.daemon.on_event(&ev, at()).await.unwrap();

    e.daemon.flush(at()).await;
    assert_eq!(e.stub.requests_to(&route).len(), 1);
    e.daemon.flush(at() + chrono::Duration::seconds(3)).await;
    e.daemon.flush(at() + chrono::Duration::seconds(20)).await;
    assert_eq!(
        e.stub.requests_to(&route).len(),
        1,
        "not before the back-off ends"
    );

    e.stub
        .on(&route, agent_fixture("t3:worker", DEVICE, AgentState::Idle));
    e.daemon.flush(at() + WAKE_RETRY_FIRST).await;
    assert_eq!(e.stub.requests_to(&route).len(), 2);
    assert_eq!(threads(&e.t3), ["worker"]);
}

#[test]
fn the_watch_lock_is_held_once_per_home_and_released_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::at(dir.path().join("home"));
    let lock = board::daemon::lock_watch(&home).unwrap();
    let err = board::daemon::lock_watch(&home).unwrap_err().to_string();
    assert!(
        err.contains(&format!("pid {}", std::process::id())) && err.contains("already running"),
        "{err}"
    );
    drop(lock);
    // A process that another test spawns gets a copy of every descriptor at fork and
    // closes the close-on-exec ones only at exec. The lock lives on the shared open file,
    // so it can outlast the drop for that window (milliseconds under load); wait it out.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match board::daemon::lock_watch(&home) {
            Ok(_) => break,
            Err(err) if Instant::now() >= deadline => panic!("free again once released: {err}"),
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

#[cfg(unix)]
#[test]
fn a_second_watch_on_the_same_home_exits_naming_the_first() {
    use std::process::{Child, Command, Stdio};

    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    stub.on("GET /healthz", json!({"ok": true, "seq": 5}));
    stub.on("GET /v1/boards", Vec::<Board>::new());
    stub.on_stream("GET /v1/events", vec![]);
    let home = home_for(dir.path(), &stub.url);
    let watch = |stderr: Stdio| -> Child {
        Command::new(env!("CARGO_BIN_EXE_board"))
            .arg("watch")
            .env("BOARD_HOME", &home)
            .env("BOARD_TOKEN", TOKEN)
            .env("BOARD_KEYCHAIN", "0")
            .env_remove("T3CODE_THREAD_ID")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("OPENCODE_SESSION_ID")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .env_remove("BOARD_AGENT")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap()
    };
    let wait_connected = |n: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while stub.requests_to("GET /v1/events").len() < n {
            assert!(Instant::now() < deadline, "the daemon never connected");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let stop = |mut child: Child| {
        let killed = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{status:?}");
                return;
            }
            assert!(Instant::now() < deadline, "the daemon ignored SIGTERM");
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    let first = watch(Stdio::null());
    wait_connected(1);
    let second = watch(Stdio::piped()).wait_with_output().unwrap();
    assert!(!second.status.success());
    let message = stderr(&second);
    assert!(
        message.contains("already running") && message.contains(&format!("pid {}", first.id())),
        "{message}"
    );
    assert_eq!(
        stub.requests_to("GET /v1/events").len(),
        1,
        "the second never connected"
    );

    // Gone with the process: the next start (a launchd restart) takes it.
    stop(first);
    let third = watch(Stdio::null());
    wait_connected(2);
    stop(third);
}

#[tokio::test]
async fn a_flapping_stream_backs_off_although_each_connection_carries_an_event() {
    let mut e = following_env(Duration::from_secs(5));
    e.daemon.timing.reconnect_first = Duration::from_millis(50);
    let events = (6..60)
        .map(|seq| {
            let mut ev = event(seq, EventType::TaskUpdated, None, "t3:someone");
            ev.board = None;
            ev
        })
        .collect();
    e.stub.on_flapping_stream("GET /v1/events", events);

    follow_until(&mut e, Duration::from_millis(1200), |_, _| false).await;
    // Backing off: opened at about 0, 50, 150, 350 and 750 ms; one per 50 ms otherwise.
    let opened = e.stub.requests_to("GET /v1/events").len();
    assert!((3..=7).contains(&opened), "opened {opened} times in 1.2 s");
    let handled = e.daemon.state.last_seq.unwrap() - 5;
    assert!(
        handled >= opened as i64 - 1,
        "each connection's event was handled: {handled} of {opened}"
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_watch_state_is_kept_aside_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    stub.on("GET /healthz", json!({"ok": true, "seq": 9}));
    stub.on("GET /v1/boards", Vec::<Board>::new());
    stub.on(
        "GET /v1/events",
        board_proto::EventsPage {
            events: vec![],
            next_since: 9,
        },
    );
    let home = home_for(dir.path(), &stub.url);
    let state = home.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("watch.json"), "{\"last_seq\": 4, \"wakes\": tru").unwrap();

    let out = board(&home, dir.path(), &[], &["watch", "--once"], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let log = stderr(&out);
    assert!(
        log.contains("watch.json: cannot read it") && log.contains("starting fresh"),
        "{log}"
    );
    let copies: Vec<_> = std::fs::read_dir(&state)
        .unwrap()
        .map(|f| f.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("watch.json.unreadable-"))
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(
        std::fs::read_to_string(state.join(&copies[0])).unwrap(),
        "{\"last_seq\": 4, \"wakes\": tru"
    );
    let saved = board::daemon::load_watch_state(&Home::at(home));
    assert_eq!(
        saved.last_seq,
        Some(9),
        "started fresh from the server's seq"
    );
}

#[cfg(unix)]
#[test]
fn a_worker_start_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    stub.on("GET /healthz", json!({"ok": true, "seq": 5}));
    stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(Some(DISPATCH), &[DEVICE]),
    );
    stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    let task = ready_task("factory-1", board_proto::Priority::P2, 0);
    stub.on("GET /v1/tasks/factory-1", task_detail_for(task.clone()));
    stub.on("POST /v1/tasks/factory-1/notes", note(&task.id));
    stub.on("GET /v1/tasks", [task]);
    stub.on(
        "GET /v1/events",
        board_proto::EventsPage {
            events: vec![],
            next_since: 5,
        },
    );
    let t3 = dir.path().join("t3");
    common::script::write_script(
        &t3,
        &format!(
            "#!/bin/sh\ncase \"$1 $2\" in\n\
             'thread start') cat >/dev/null; echo '{{\"threadId\":\"{NEW_THREAD}\"}}' ;;\n\
             'thread show') echo '{{\"thread\":{{\"archived\":false}}}}' ;;\n\
             'thread list') echo '{{\"threads\":[]}}' ;;\n\
             *) cat >/dev/null ;;\nesac\n"
        ),
    );
    let home = home_for(dir.path(), &stub.url);
    let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "{config}t3_bin = {:?}\n[projects]\n{REPO:?} = {:?}\n",
            t3.display().to_string(),
            src("agent-board").display().to_string()
        ),
    )
    .unwrap();

    let out = board(&home, dir.path(), &[], &["watch", "--once"], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let log = stderr(&out);
    assert!(
        log.contains(&format!(
            "started worker thread {NEW_THREAD} for factory-1 on factory"
        )),
        "{log}"
    );
    assert!(
        log.contains(&format!("{DISPATCH} dispatches factory")),
        "{log}"
    );
}

/// Stubs `agent` as an idle T3 thread on this device and queues the answer to its ask.
async fn queue_answer(e: &mut Env, seq: i64, ask: i64, agent: &str, now: Timestamp) {
    e.stub.on(
        &format!("GET {}", agent_path(agent)),
        agent_fixture(agent, DEVICE, AgentState::Idle),
    );
    let ev = answered(e, seq, ask, agent);
    e.daemon.on_event(&ev, now).await.unwrap();
}

fn queued(state: &board::daemon::WatchState) -> Vec<&str> {
    state.wakes.keys().map(|a| a.as_str()).collect()
}

#[tokio::test]
async fn a_stop_that_cuts_a_flush_short_keeps_the_wake_ups_it_was_checking() {
    let mut e = following_env(Duration::from_secs(3600));
    // Only the stop request ends this flush.
    e.daemon.timing.flush_budget = Duration::from_secs(3600);
    e.daemon.timing.stop_budget = Duration::from_millis(300);
    e.stub.on_stream("GET /v1/events", vec![]);
    queue_answer(&mut e, 6, 3, "t3:worker", chrono::Utc::now()).await;
    e.stub.delay(
        &format!("GET {}", agent_path("t3:worker")),
        Duration::from_secs(3600),
    );

    let (_, stopping) = follow_until(&mut e, Duration::from_millis(500), |_, _| false).await;
    assert!(stopping < Duration::from_secs(2), "stopped in {stopping:?}");
    assert_eq!(queued(&saved_state(&e)), ["t3:worker"]);
}

#[tokio::test]
async fn a_last_flush_cut_short_saves_what_it_delivered() {
    let mut e = following_env(Duration::from_secs(5));
    e.daemon.timing.stop_budget = Duration::from_millis(300);
    queue_answer(&mut e, 6, 3, "t3:a", chrono::Utc::now()).await;
    queue_answer(&mut e, 7, 4, "t3:b", chrono::Utc::now()).await;
    e.t3.hang_send.lock().unwrap().push("b".into());
    e.stub.delay("GET /v1/boards", Duration::from_secs(3600));

    // Asked to stop at once: the last flush delivers to a, then hangs sending to b.
    let (_, stopping) = follow_until(&mut e, Duration::ZERO, |_, _| true).await;
    assert!(stopping < Duration::from_secs(2), "stopped in {stopping:?}");
    assert_eq!(threads(&e.t3), ["a", "b"]);
    assert!(
        queued(&saved_state(&e)).is_empty(),
        "a was delivered and b may have been: neither is sent again"
    );
}

#[tokio::test]
async fn a_stop_that_cuts_a_retry_short_keeps_its_events() {
    let mut e = following_env(Duration::from_secs(3600));
    e.stub.on_stream("GET /v1/events", vec![]);
    e.stub
        .on("GET /v1/tasks/factory-1", detail("factory-1", false, &[]));
    e.stub
        .delay("GET /v1/tasks/factory-1", Duration::from_secs(3600));
    let mut failed = event(4, EventType::TaskDone, Some("factory-1"), "t3:x");
    failed.at = chrono::Utc::now();
    e.daemon.state.retry.push(failed);

    follow_until(&mut e, Duration::from_millis(500), |_, _| false).await;
    let saved = saved_state(&e);
    assert_eq!(
        saved.retry.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [4],
        "still to retry"
    );
}

#[tokio::test]
async fn an_event_the_server_keeps_failing_does_not_hold_up_the_others() {
    let mut e = env();
    e.stub.on_error(
        "GET /v1/tasks/factory-1",
        503,
        "unavailable",
        "broken",
        "try again",
    );
    e.stub
        .on("GET /v1/tasks/factory-2", detail("factory-2", false, &[]));
    for (seq, task) in [(6, "factory-1"), (7, "factory-2")] {
        e.daemon
            .state
            .retry
            .push(event(seq, EventType::TaskDone, Some(task), "t3:x"));
    }
    e.daemon.retry_failed(at()).await;
    let left: Vec<i64> = e.daemon.state.retry.iter().map(|e| e.seq).collect();
    assert_eq!(left, [6], "the second was retried and settled");
}

#[tokio::test]
async fn an_unreachable_server_stops_a_retry_round_after_one_request() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let t3 = FakeT3::default();
    let mut daemon = daemon_timed(
        dir.path(),
        &stub,
        projects(),
        &t3,
        Some(Duration::from_millis(200)),
    );
    for (seq, task) in [(6, "factory-1"), (7, "factory-2")] {
        let route = format!("GET /v1/tasks/{task}");
        stub.on(&route, detail(task, false, &[]));
        stub.delay(&route, Duration::from_secs(3600));
        daemon
            .state
            .retry
            .push(event(seq, EventType::TaskDone, Some(task), "t3:x"));
    }
    daemon.retry_failed(at()).await;
    let tried = stub
        .requests()
        .iter()
        .filter(|r| r.path.starts_with("/v1/tasks/"))
        .count();
    assert_eq!(tried, 1, "the rest wait for the next tick");
    assert_eq!(daemon.state.retry.len(), 2);
}

#[tokio::test]
async fn queues_a_flush_did_not_check_in_time_do_not_back_off() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let t3 = FakeT3::default();
    let mut daemon = daemon_timed(
        dir.path(),
        &stub,
        projects(),
        &t3,
        Some(Duration::from_secs(3600)),
    );
    daemon.timing.flush_budget = Duration::from_millis(300);
    let mut e = Env {
        _dir: dir,
        stub,
        t3,
        daemon,
    };
    queue_answer(&mut e, 6, 3, "t3:worker", at()).await;
    queue_answer(&mut e, 7, 4, "t3:slow", at()).await;
    e.stub.delay(
        &format!("GET {}", agent_path("t3:slow")),
        Duration::from_secs(3600),
    );

    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["worker"], "checked side by side");
    let slow = &e.daemon.state.wakes[&"t3:slow".parse().unwrap()];
    assert_eq!(slow.wakes.len(), 1, "kept");
    assert_eq!(slow.backoff, None, "never checked, so nothing failed");
}

fn minute(n: i64) -> Timestamp {
    at() + chrono::Duration::minutes(n)
}

#[tokio::test]
async fn a_second_timeout_before_the_check_keeps_both_messages() {
    let (mut e, agent) = owner_env(AgentState::Idle, None);
    let line = |id: &str| {
        format!(
            "board factory: every blocker of your blocked {id} is done; resume it (task_update {id} status=doing)\nContinue from this and the board delta in this prompt, if there is one."
        )
    };
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;

    // T3 stalls: the check fails, so the new line is not held, and its send times out too.
    e.t3.fail_messages.store(true, Ordering::SeqCst);
    queue_unblocked(&mut e, 11, &["factory-2"], minute(1)).await;
    e.daemon.flush(minute(1)).await;
    assert_eq!(e.t3.sends().len(), 2);
    assert_eq!(unsure_wakes(&e, &agent).unwrap().messages.len(), 2);

    // Neither reached the thread: each is sent again once it is due.
    e.t3.fail_messages.store(false, Ordering::SeqCst);
    let retry = minute(1) + board::daemon::WAKE_RETRY_FIRST;
    e.daemon.flush(retry).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 3, "{sends:?}");
    assert_eq!(sends[2].1, line("factory-1"));
    e.daemon.flush(minute(2)).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 4, "{sends:?}");
    assert_eq!(sends[3].1, line("factory-2"));
    assert_eq!(unsure_wakes(&e, &agent), None);
    e.daemon.flush(minute(30)).await;
    assert_eq!(e.t3.sends().len(), 4);
}

#[tokio::test]
async fn a_flush_stopped_while_sending_leaves_the_message_to_be_looked_for() {
    let (mut e, agent) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;
    e.t3.hang_send.lock().unwrap().push("owner".to_owned());
    let cut = tokio::time::timeout(Duration::from_millis(200), e.daemon.flush(at())).await;
    assert!(cut.is_err());
    assert_eq!(unsure_wakes(&e, &agent).unwrap().messages.len(), 1);
    e.t3.hang_send.lock().unwrap().clear();
    e.daemon.flush(minute(1)).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2, "never got it: sent again");
    assert_eq!(sends[0].1, sends[1].1);
    assert_eq!(unsure_wakes(&e, &agent), None);
}

#[tokio::test]
async fn a_lost_message_to_a_gone_thread_is_not_looked_for_again() {
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    let worker = "t3:worker";
    e.t3.live("worker");
    e.stub.on(
        &format!("GET {}", agent_path(worker)),
        agent_fixture(worker, DEVICE, AgentState::Idle),
    );
    queue_answer(&mut e, 10, 7, worker, at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    assert!(unsure_wakes(&e, worker).is_some());

    // The thread is gone by the check: its lines go with it.
    e.t3.threads.lock().unwrap().remove("worker");
    e.t3.refuse_send.lock().unwrap().push("worker".to_owned());
    e.daemon.flush(minute(1)).await;
    e.daemon.flush(minute(1)).await;
    assert_eq!(unsure_wakes(&e, worker), None);
    assert!(e.daemon.state.wakes.is_empty());
    assert_eq!(
        threads(&e.t3),
        ["worker", "worker"],
        "sent again once, and refused"
    );
}

/// A reading the server would return: `week` percent of the weekly window, read at `read`.
fn usage_reading(week: f64, read: Timestamp) -> serde_json::Value {
    json!({
        "seven_day": { "percent": week, "resets_at": read + chrono::Duration::days(2) },
        "read_at": read,
        "device": DEVICE,
    })
}

/// A reading the server would return: the 5-hour window at `five` percent until `reset`,
/// read at `read`.
fn five_hour_reading(five: f64, reset: Timestamp, read: Timestamp) -> serde_json::Value {
    json!({
        "five_hour": { "percent": five, "resets_at": reset },
        "seven_day": { "percent": 40.0, "resets_at": read + chrono::Duration::days(2) },
        "read_at": read,
        "device": DEVICE,
    })
}

/// The idle Claude agents `t3:owner` and `t3:worker`, with the usage as `reading` says:
/// queues a cleared blocker for the owner and an answer for the worker at `now`.
async fn wakes_under_usage(reading: serde_json::Value, now: Timestamp) -> Env {
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.stub.on("GET /v1/usage", reading);
    e.daemon.reconcile(now).await.unwrap();
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Idle),
    );
    queue_unblocked(&mut e, 10, &["factory-4"], now).await;
    let ev = answered(&e, 30, 3, "t3:worker");
    e.daemon.on_event(&ev, now).await.unwrap();
    e
}

#[tokio::test]
async fn a_full_usage_window_keeps_claude_agents_wake_ups_and_sends_them_once_at_its_reset() {
    // A reset well after the wake-ups' time to live.
    let reset = at() + board::daemon::WAKE_TTL * 2;
    let mut e = wakes_under_usage(five_hour_reading(100.0, reset, at()), at()).await;
    e.daemon.flush(at()).await;
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());
    let kept = |d: &Daemon<FakeT3>| {
        d.state
            .wakes
            .iter()
            .map(|(agent, queue)| (agent.to_string(), queue.wakes.len()))
            .collect::<Vec<_>>()
    };
    let both = [("t3:owner".to_owned(), 1), ("t3:worker".to_owned(), 1)];
    assert_eq!(kept(&e.daemon), both, "both kept");

    // Kept after a restart, also before the daemon has a reading again.
    e.daemon.save().unwrap();
    let mut restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    restarted.flush(at() + chrono::Duration::minutes(1)).await;
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());
    // Kept past the time to live while a fresh reading still shows the window full.
    let before = reset - chrono::Duration::minutes(1);
    e.stub
        .on("GET /v1/usage", five_hour_reading(100.0, reset, before));
    restarted.reconcile(before).await.unwrap();
    restarted.flush(before).await;
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());
    assert_eq!(kept(&restarted), both);

    // At the reset both get what was kept, once.
    restarted.flush(reset).await;
    let sends = e.t3.sends();
    let [(first, cleared), (second, answer)] = sends.as_slice() else {
        panic!("{sends:?}");
    };
    assert_eq!((first.as_str(), second.as_str()), ("owner", "worker"));
    assert!(cleared.contains("your blocked factory-4"), "{cleared}");
    assert!(
        answer.starts_with("the user answered ask #3 about factory-7"),
        "{answer}"
    );
    assert!(restarted.state.wakes.is_empty());
    restarted.flush(reset + chrono::Duration::minutes(1)).await;
    assert_eq!(e.t3.sends().len(), 2, "once");
}

#[tokio::test]
async fn a_full_usage_hold_that_ends_before_its_reset_sends_what_it_kept_and_expires_later_lines() {
    let reset = at() + chrono::Duration::days(2);
    let mut e = wakes_under_usage(five_hour_reading(100.0, reset, at()), at()).await;
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());

    // Two hours later the reading is below 100%: what was kept is still sent.
    let eased = at() + board::daemon::WAKE_TTL * 2;
    e.stub
        .on("GET /v1/usage", five_hour_reading(40.0, reset, eased));
    e.daemon.reconcile(eased).await.unwrap();
    e.daemon.flush(eased).await;
    assert_eq!(e.daemon.state.usage_full_until, Some(eased));
    let sent = threads(&e.t3);
    assert!(sent.contains(&"worker".to_owned()), "{sent:?}");
    assert!(e.daemon.state.wakes.is_empty());

    // A later wake-up that cannot be sent expires an hour after it was queued, not after
    // the reset the hold waited for.
    let later = eased + chrono::Duration::minutes(10);
    e.stub.on(
        &format!("GET {}", agent_path("t3:late")),
        agent_fixture("t3:late", DEVICE, AgentState::Idle),
    );
    e.t3.live("late");
    e.t3.fail_send.store(true, Ordering::SeqCst);
    let ev = answered(&e, 40, 5, "t3:late");
    e.daemon.on_event(&ev, later).await.unwrap();
    e.daemon.flush(later).await;
    assert_eq!(e.daemon.state.wakes.len(), 1, "kept for a retry");
    e.daemon.flush(later + board::daemon::WAKE_TTL).await;
    assert!(e.daemon.state.wakes.is_empty(), "expired");
}

#[tokio::test]
async fn a_usage_window_below_100_percent_keeps_no_wake_up() {
    let reset = at() + chrono::Duration::hours(2);
    let mut e = wakes_under_usage(five_hour_reading(99.0, reset, at()), at()).await;
    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["owner", "worker"]);
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn a_stale_reading_of_a_full_usage_window_keeps_no_wake_up() {
    let reset = at() + chrono::Duration::hours(2);
    let stale = at() + chrono::Duration::minutes(board_proto::USAGE_FRESH_MINUTES + 1);
    let mut e = wakes_under_usage(five_hour_reading(100.0, reset, at()), stale).await;
    e.daemon.flush(stale).await;
    assert_eq!(threads(&e.t3), ["owner", "worker"]);
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn a_full_usage_window_does_not_keep_a_codex_agents_wake_up() {
    let reset = at() + chrono::Duration::hours(2);
    let mut e = wakes_under_usage(five_hour_reading(100.0, reset, at()), at()).await;
    let mut codex = agent_fixture("t3:codex", DEVICE, AgentState::Idle);
    codex.runtime = Runtime::Codex;
    e.stub
        .on(&format!("GET {}", agent_path("t3:codex")), &codex);
    let ev = answered(&e, 31, 4, "t3:codex");
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["codex"], "it spends another limit");
    assert!(e.t3.sends()[0].1.contains("ask #4"));
    assert_eq!(e.daemon.state.wakes.len(), 2, "the Claude agents' are kept");
}

#[tokio::test]
async fn the_daemon_reads_the_usage_endpoint_at_most_every_five_minutes_and_reports_it() {
    use board::daemon::USAGE_POLL_EVERY;
    use board::usage::{Credentials, UsageSource};

    let (mut e, _) = owner_env(AgentState::Idle, None);
    let creds = e._dir.path().join("credentials.json");
    let sign_in = |expires: Timestamp| {
        json!({ "claudeAiOauth": {
            "accessToken": "tok-123", "refreshToken": "never-read",
            "expiresAt": expires.timestamp_millis(),
        }})
        .to_string()
    };
    std::fs::write(&creds, sign_in(at() + chrono::Duration::hours(8))).unwrap();
    e.daemon.usage_source = UsageSource::ClaudeCode {
        url: format!("{}/api/oauth/usage", e.stub.url),
        credentials: Credentials::File(creds.clone()),
        snapshots: Vec::new(),
    };
    e.stub.on(
        "GET /api/oauth/usage",
        json!({
            "five_hour": { "utilization": 12.0, "resets_at": "2026-11-06T14:00:00+00:00" },
            "seven_day": { "utilization": 93.0, "resets_at": null },
            "seven_day_opus": null,
        }),
    );
    e.stub.on("PUT /v1/usage", usage_reading(93.0, at()));
    e.stub.on("GET /v1/usage", usage_reading(93.0, at()));

    e.daemon.reconcile(at()).await.unwrap();
    let reads = e.stub.requests_to("GET /api/oauth/usage");
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].auth.as_deref(), Some("Bearer tok-123"));
    let reports = e.stub.requests_to("PUT /v1/usage");
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0].body,
        json!({
            "five_hour": { "percent": 12.0, "resets_at": "2026-11-06T14:00:00Z" },
            "seven_day": { "percent": 93.0 },
            "read_at": at(),
        })
    );
    assert!(
        !reports[0].body.to_string().contains("tok-123"),
        "the token stays here"
    );
    assert!(e.daemon.usage_holds(&factory(), at()));

    // Between readings, the server's newest reading.
    let soon = at() + chrono::Duration::minutes(1);
    e.daemon.reconcile(soon).await.unwrap();
    assert_eq!(e.stub.requests_to("GET /api/oauth/usage").len(), 1);
    assert_eq!(e.stub.requests_to("GET /v1/usage").len(), 1);

    let next = at() + USAGE_POLL_EVERY;
    e.daemon.reconcile(next).await.unwrap();
    assert_eq!(e.stub.requests_to("GET /api/oauth/usage").len(), 2);

    // An expired token is not refreshed: the endpoint is not read.
    std::fs::write(&creds, sign_in(at())).unwrap();
    e.daemon.reconcile(next + USAGE_POLL_EVERY).await.unwrap();
    assert_eq!(e.stub.requests_to("GET /api/oauth/usage").len(), 2);
    // No sign-in at all: the same.
    std::fs::remove_file(&creds).unwrap();
    e.daemon
        .reconcile(next + USAGE_POLL_EVERY * 2)
        .await
        .unwrap();
    assert_eq!(e.stub.requests_to("GET /api/oauth/usage").len(), 2);
}

#[tokio::test]
async fn an_empty_usage_answer_is_not_reported_and_a_failed_report_keeps_the_local_reading() {
    use board::daemon::USAGE_POLL_EVERY;
    use board::usage::{Credentials, UsageSource};

    let (mut e, _) = owner_env(AgentState::Idle, None);
    let creds = e._dir.path().join("credentials.json");
    std::fs::write(
        &creds,
        json!({ "claudeAiOauth": {
            "accessToken": "tok-123",
            "expiresAt": (at() + chrono::Duration::hours(8)).timestamp_millis(),
        }})
        .to_string(),
    )
    .unwrap();
    e.daemon.usage_source = UsageSource::ClaudeCode {
        url: format!("{}/api/oauth/usage", e.stub.url),
        credentials: Credentials::File(creds),
        snapshots: Vec::new(),
    };
    e.stub
        .on("GET /v1/usage", usage_reading(95.0, at()))
        .on("PUT /v1/usage", usage_reading(95.0, at()));

    // An answer in an unknown shape is no reading: nothing is reported, and the server's
    // reading still holds the board.
    e.stub.on("GET /api/oauth/usage", json!({ "renamed": {} }));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.stub.requests_to("PUT /v1/usage").is_empty());
    assert!(e.daemon.usage_holds(&factory(), at()));

    // The server fails: the daemon acts on its own reading.
    let next = at() + USAGE_POLL_EVERY;
    e.stub
        .on(
            "GET /api/oauth/usage",
            json!({ "seven_day": { "utilization": 97.0 } }),
        )
        .on_error("PUT /v1/usage", 503, "unavailable", "down", "retry")
        .on_error("GET /v1/usage", 503, "unavailable", "down", "retry");
    e.daemon.reconcile(next).await.unwrap();
    assert_eq!(e.stub.requests_to("PUT /v1/usage").len(), 1);
    let usage = e.daemon.usage.as_ref().unwrap();
    assert_eq!(
        (usage.seven_day.unwrap().percent, usage.read_at),
        (97.0, next)
    );
    assert!(e.daemon.usage_holds(&factory(), next));
}

// Daemon dispatch.

const DISPATCH: &str = "dispatch:mac-test";

/// A home board set to daemon dispatch, its lease held by `coordinator`.
fn daemon_board(coordinator: Option<&str>) -> Board {
    let mut board = autopilot(coordinator, &[DEVICE]);
    board.dispatch = board_proto::Dispatch::Daemon;
    board
}

fn ready_task(id: &str, priority: board_proto::Priority, minutes: i64) -> board_proto::Task {
    let mut task = task_fixture(id, &format!("Task {id}"));
    task.priority = priority;
    task.created_at = at() + chrono::Duration::minutes(minutes);
    task
}

/// The task `id` as `GET /v1/tasks/{id}` gives it: `status`, owned by `owner`.
fn task_at(e: &Env, id: &str, status: TaskStatus, owner: Option<&str>) {
    let mut detail = detail(id, status == TaskStatus::Open && owner.is_none(), &[]);
    detail.task.title = format!("Task {id}");
    detail.task.status = status;
    detail.task.owner = owner.map(|o| o.parse().unwrap());
    e.stub.on(&format!("GET /v1/tasks/{id}"), detail);
}

/// A daemon-dispatch board this device runs, with `tasks` ready.
fn dispatch_env(tasks: &[board_proto::Task]) -> Env {
    let e = env();
    e.stub.on("GET /v1/boards", [daemon_board(None)]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        daemon_board(Some(DISPATCH)),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    e.stub.on("GET /v1/tasks", tasks);
    for task in tasks {
        e.stub.on(
            &format!("GET /v1/tasks/{}", task.id),
            task_detail_for(task.clone()),
        );
        e.stub
            .on(&format!("POST /v1/tasks/{}/notes", task.id), note(&task.id));
    }
    e
}

#[test]
fn the_worker_brief_says_how_to_run_a_climb() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        !board::brief::WORKER_PROMPT.contains("## Climbs"),
        "base worker prompt omits conditional Climbs section"
    );
    let brief = words(&board::brief::worker_brief_with_options(
        &factory(),
        &"factory-7".parse().unwrap(),
        board::brief::WORKER_CLOSER,
        board::brief::WorkerBriefOptions {
            include_climbs: true,
            include_revisits: false,
            include_t3_settle: true,
        },
    ));
    for rule in [
        "A climb is a task whose acceptance is a score command",
        "note starting `score:`",
        "use its verdict, do not judge a plateau yourself",
        "report friction in the climb process",
        "applies from the next run: start one and note why",
        "Never change the bar, judges or references within a run",
        "which judges scored it, the gap to the bar and the budget spent",
        "merges only on the user's decision",
    ] {
        assert!(brief.contains(rule), "missing {rule:?}");
    }
}

/// A board from a server that predates the `dispatch` field reads as `thread`, and the
/// daemon dispatches it all the same.
#[tokio::test]
async fn a_board_that_reads_thread_dispatch_is_dispatched() {
    let task = ready_task("factory-4", board_proto::Priority::P1, 0);
    let e = dispatch_env(std::slice::from_ref(&task));
    let mut board = daemon_board(None);
    board.dispatch = board_proto::Dispatch::Thread;
    e.stub.on("GET /v1/boards", [board]);
    let mut e = e;
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);
}

fn note(task: &board_proto::TaskId) -> serde_json::Value {
    json!({"id": 1, "task": task, "agent": DISPATCH, "kind": "note", "text": "noted", "at": at()})
}

fn worker_starts(t3: &FakeT3) -> Vec<(String, String)> {
    t3.calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Start { title, message, .. } => Some((title, message)),
            _ => None,
        })
        .collect()
}

fn dispatch_claims(e: &Env) -> Vec<Recorded> {
    e.stub.requests_to("POST /v1/boards/factory/coordinator")
}

#[tokio::test]
async fn a_daemon_board_gets_a_worker_for_its_highest_priority_oldest_ready_task() {
    use board_proto::Priority::{P1, P2};
    let mut not_auto = ready_task("factory-2", P1, -5);
    not_auto.auto = false;
    let tasks = [
        ready_task("factory-3", P2, -10),
        not_auto,
        ready_task("factory-5", P1, 2),
        ready_task("factory-4", P1, 1),
    ];
    let mut e = dispatch_env(&tasks);

    e.daemon.reconcile(at()).await.unwrap();

    // The lease is held as the device's dispatch agent, registered first.
    let registered = e.stub.requests_to("POST /v1/agents");
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0].body["id"], DISPATCH);
    assert_eq!(registered[0].body["device"], DEVICE);
    let claims = dispatch_claims(&e);
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].body, json!({"device": DEVICE, "agent": DISPATCH}));
    assert_eq!(claims[0].agent.as_deref(), Some(DISPATCH));

    // One worker, for the p1 task created first, with the worker brief.
    let starts = worker_starts(&e.t3);
    assert_eq!(
        starts,
        [(
            "factory-4 Task factory-4".to_owned(),
            board::brief::worker_brief(
                &factory(),
                &"factory-4".parse().unwrap(),
                board::brief::WORKER_CLOSER
            )
        )]
    );
    let worker = &e.daemon.state.workers[&"factory-4".parse().unwrap()];
    assert_eq!(worker.thread.as_deref(), Some(NEW_THREAD));
    assert!(worker.noted);

    // Its link is noted on the task, by the dispatch agent.
    let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].agent.as_deref(), Some(DISPATCH));
    assert_eq!(notes[0].body["agent"], DISPATCH);
    let text = notes[0].body["text"].as_str().unwrap();
    assert!(
        text.contains(&format!("https://t3.example.test/threads/{NEW_THREAD}")),
        "{text}"
    );

    // Recorded in watch.json, so a restarted daemon follows the same worker.
    let restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    assert_eq!(restarted.state.workers, e.daemon.state.workers);

    // While the worker has not claimed the task, its slot is held: no second worker, and
    // the lease is not claimed again before its renewal.
    e.stub.on("GET /v1/boards", [daemon_board(Some(DISPATCH))]);
    task_at(&e, "factory-4", TaskStatus::Open, None);
    let later = at() + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);
    assert_eq!(dispatch_claims(&e).len(), 1);

    // Claimed, it still holds the slot; the lease is renewed after RENEW_EVERY.
    let owner = format!("t3:{NEW_THREAD}");
    task_at(&e, "factory-4", TaskStatus::Doing, Some(&owner));
    let renew = at() + board::daemon::RENEW_EVERY;
    e.daemon.reconcile(renew).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);
    assert_eq!(dispatch_claims(&e).len(), 2);
    assert!(e.daemon.state.workers[&"factory-4".parse().unwrap()].claimed);

    // Parked in review, the slot is free: the next task gets a worker.
    task_at(&e, "factory-4", TaskStatus::Review, Some(&owner));
    e.stub.on("GET /v1/tasks", &tasks[..3]);
    e.daemon.reconcile(renew).await.unwrap();
    let starts = worker_starts(&e.t3);
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].0, "factory-5 Task factory-5");
    assert!(
        !e.daemon
            .state
            .workers
            .contains_key(&"factory-4".parse().unwrap())
    );
}

#[tokio::test]
async fn a_worker_that_leaves_its_task_open_frees_its_slot_and_the_task_waits() {
    use board_proto::Priority::P1;
    let tasks = [
        ready_task("factory-4", P1, 0),
        ready_task("factory-5", P1, 1),
    ];
    let mut e = dispatch_env(&tasks);
    e.daemon.reconcile(at()).await.unwrap();
    let owner = format!("t3:{NEW_THREAD}");
    task_at(&e, "factory-4", TaskStatus::Doing, Some(&owner));
    let later = at() + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();

    // It gives the task back with nothing else in progress: another task gets the slot,
    // and factory-4 waits its retry.
    task_at(&e, "factory-4", TaskStatus::Open, None);
    e.daemon.reconcile(later).await.unwrap();
    let starts = worker_starts(&e.t3);
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].0, "factory-5 Task factory-5");

    // A worker that ends without claiming its task frees the slot too, and factory-4 still
    // waits its retry: nothing more starts.
    let worker_agent = format!("t3:{NEW_THREAD}");
    task_at(&e, "factory-5", TaskStatus::Open, None);
    e.stub.on(
        &format!("GET {}", agent_path(&worker_agent)),
        agent_fixture(&worker_agent, DEVICE, AgentState::Ended),
    );
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 2);
    assert!(e.daemon.state.workers.is_empty());
    assert_eq!(e.daemon.state.worker_retry.len(), 2);

    // After the retry wait, factory-4 gets a new worker.
    let retry = later + board::daemon::WORKER_RETRY;
    e.daemon.reconcile(retry).await.unwrap();
    let starts = worker_starts(&e.t3);
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[2].0, "factory-4 Task factory-4");
}

#[tokio::test]
async fn a_worker_that_never_claims_its_task_frees_the_slot_after_the_claim_wait() {
    let tasks = [ready_task("factory-4", board_proto::Priority::P1, 0)];
    let mut e = dispatch_env(&tasks);
    e.daemon.reconcile(at()).await.unwrap();
    task_at(&e, "factory-4", TaskStatus::Open, None);
    // Its session never registers.
    let wait = at() + board::daemon::WORKER_CLAIM_WAIT;
    e.daemon
        .reconcile(wait - chrono::Duration::seconds(1))
        .await
        .unwrap();
    assert_eq!(e.daemon.state.workers.len(), 1);
    e.daemon.reconcile(wait).await.unwrap();
    assert!(e.daemon.state.workers.is_empty());
    assert_eq!(worker_starts(&e.t3).len(), 1, "the task waits its retry");
}

#[tokio::test]
async fn a_failed_worker_start_is_found_again_or_retried_after_a_delay() {
    let tasks = [ready_task("factory-4", board_proto::Priority::P1, 0)];
    let mut e = dispatch_env(&tasks);
    *e.t3.clock.lock().unwrap() = at();
    e.t3.die_after_create.lock().unwrap().replace(false);
    e.daemon.reconcile(at()).await.unwrap();
    let worker = &e.daemon.state.workers[&"factory-4".parse().unwrap()];
    assert_eq!(worker.thread, None, "recorded before the start");

    // The thread the failed start created is found, briefed and followed: no second start.
    e.t3.die_after_create.lock().unwrap().take();
    task_at(&e, "factory-4", TaskStatus::Open, None);
    let later = at() + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);
    let worker = &e.daemon.state.workers[&"factory-4".parse().unwrap()];
    assert_eq!(worker.thread.as_deref(), Some(ORPHAN));
    assert!(worker.noted);
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, ORPHAN);
    assert!(sends[0].1.contains("factory-4"), "{}", sends[0].1);

    // A start that made no thread is dropped, its task waits WORKER_RETRY, and the next
    // task starts once the board's start backoff ends.
    let tasks = [
        ready_task("factory-4", board_proto::Priority::P1, 0),
        ready_task("factory-5", board_proto::Priority::P1, 1),
    ];
    let mut e = dispatch_env(&tasks);
    e.t3.fail_start.store(true, Ordering::SeqCst);
    e.daemon.reconcile(at()).await.unwrap();
    e.t3.fail_start.store(false, Ordering::SeqCst);
    e.daemon
        .reconcile(at() + chrono::Duration::seconds(30))
        .await
        .unwrap();
    assert!(e.daemon.state.workers.is_empty());
    assert!(
        e.daemon
            .state
            .worker_retry
            .contains_key(&"factory-4".parse().unwrap())
    );
    assert_eq!(worker_starts(&e.t3).len(), 1);
    e.daemon
        .reconcile(at() + board::daemon::START_RETRY_FIRST)
        .await
        .unwrap();
    let starts = worker_starts(&e.t3);
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].0, "factory-5 Task factory-5");
}

#[tokio::test]
async fn a_daemon_board_starts_nothing_while_paused_or_at_its_usage_stop() {
    let tasks = [ready_task("factory-4", board_proto::Priority::P1, 0)];
    let mut e = dispatch_env(&tasks);
    let mut paused = daemon_board(None);
    paused.status = board_proto::BoardStatus::Paused;
    e.stub.on("GET /v1/boards", [&paused]);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(dispatch_claims(&e).is_empty());
    assert!(worker_starts(&e.t3).is_empty());

    e.stub.on("GET /v1/boards", [daemon_board(None)]);
    e.stub.on("GET /v1/usage", usage_reading(95.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(dispatch_claims(&e).len(), 1, "the lease is still held");
    assert!(worker_starts(&e.t3).is_empty());
    assert!(e.t3.sends().is_empty());

    e.stub.on("GET /v1/usage", usage_reading(40.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);
}

/// The stubs a reconcile of a daemon board needs once its dispatcher claims the lease.
#[tokio::test]
async fn a_board_no_longer_run_here_releases_the_dispatch_lease() {
    let mut e = dispatch_env(&[]);
    let mut off = daemon_board(Some(DISPATCH));
    off.autopilot = false;
    e.stub.on("GET /v1/boards", [off]);
    e.stub
        .on("DELETE /v1/boards/factory/coordinator", daemon_board(None));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(releases(&e), 1);
    assert!(dispatch_claims(&e).is_empty());
}

#[tokio::test]
async fn a_judge_registration_asks_for_no_reconcile() {
    let mut e = env();
    let mut judges = event(10, EventType::BoardUpdated, None, DISPATCH);
    judges.payload = json!({"fields": ["judges"], "judge": "t3:j", "kind": "review"});
    assert!(!e.daemon.on_event(&judges, at()).await.unwrap());
    let mut switched = event(11, EventType::BoardUpdated, None, "user");
    switched.payload = json!({"fields": ["dispatch"]});
    assert!(e.daemon.on_event(&switched, at()).await.unwrap());
}

#[tokio::test]
async fn a_task_change_asks_for_a_reconcile_only_on_a_daemon_board_run_here() {
    let mut e = dispatch_env(&[]);
    e.daemon.reconcile(at()).await.unwrap();
    task_at(&e, "factory-4", TaskStatus::Review, Some("t3:w"));
    let moved = event(10, EventType::TaskUpdated, Some("factory-4"), "t3:w");
    assert!(e.daemon.on_event(&moved, at()).await.unwrap());

    // A board whose home is another device does not.
    let mut e = env();
    e.stub.on(
        "GET /v1/boards",
        [autopilot(Some("t3:other"), &["mac-air"])],
    );
    e.daemon.reconcile(at()).await.unwrap();
    task_at(&e, "factory-4", TaskStatus::Review, Some("t3:w"));
    assert!(!e.daemon.on_event(&moved, at()).await.unwrap());
}

#[tokio::test]
async fn a_task_in_doing_that_no_worker_here_follows_takes_the_slot() {
    use board_proto::Priority::P1;
    // A thread this daemon did not start, such as one the user started.
    let mut child = ready_task("factory-9", P1, -60);
    child.status = TaskStatus::Doing;
    child.owner = Some("t3:old-child".parse().unwrap());
    let mut e = dispatch_env(&[child, ready_task("factory-4", P1, 0)]);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(dispatch_claims(&e).len(), 1);
    assert!(worker_starts(&e.t3).is_empty());
}

#[tokio::test]
async fn a_device_without_the_project_folder_leaves_the_dispatch_lease_to_another() {
    let tasks = [ready_task("factory-4", board_proto::Priority::P1, 0)];
    let mut e = dispatch_env(&tasks);
    e.daemon = daemon_at(e._dir.path(), &e.stub, BTreeMap::new(), &e.t3);
    e.stub.on("GET /v1/boards", [daemon_board(Some(DISPATCH))]);
    e.stub
        .on("DELETE /v1/boards/factory/coordinator", daemon_board(None));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(dispatch_claims(&e).is_empty());
    assert_eq!(releases(&e), 1);
    assert!(worker_starts(&e.t3).is_empty());
}

#[tokio::test]
async fn a_worker_entry_this_version_cannot_read_is_skipped_not_the_rest_of_the_state() {
    let tasks = [ready_task("factory-4", board_proto::Priority::P1, 0)];
    let mut e = dispatch_env(&tasks);
    e.daemon.reconcile(at()).await.unwrap();
    let state_dir = home_of(&e).state_dir();
    for suffix in ["daemon.db", "daemon.db-wal", "daemon.db-shm"] {
        let _ = std::fs::remove_file(state_dir.join(suffix));
    }
    let path = state_dir.join("watch.json");
    let mut saved = serde_json::to_value(&e.daemon.state).unwrap();
    saved["workers"]["factory-6"] = json!({ "title": 3 });
    saved["workers"]["not a task"] = saved["workers"]["factory-4"].clone();
    saved["worker_retry"] = json!({ "factory-7": "soon", "factory-8": at() });
    saved["worker_skips"] = json!({ "factory-7": "factory-4", "factory-8": ["factory-4"] });
    saved["owed"] = json!([
        { "step": "judge", "board": "factory" },
        { "step": "release", "board": "factory", "task": "factory-8", "away": "t3:w",
          "since": at(), "noticed": at(), "at": at() },
    ]);
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();

    let restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    assert_eq!(restarted.state.workers, e.daemon.state.workers);
    assert_eq!(restarted.state.workers.len(), 1);
    assert_eq!(
        restarted.state.worker_retry.keys().collect::<Vec<_>>(),
        [&"factory-8".parse::<board_proto::TaskId>().unwrap()]
    );
    assert_eq!(
        restarted.state.worker_skips.keys().collect::<Vec<_>>(),
        [&"factory-8".parse::<board_proto::TaskId>().unwrap()]
    );
    assert_eq!(restarted.state.owed.len(), 1);
}

/// A daemon-dispatch board this device runs with `slots` tasks at once, with `tasks` ready.
fn slots_env(slots: u32, tasks: &[board_proto::Task]) -> Env {
    let e = dispatch_env(tasks);
    let mut board = daemon_board(None);
    board.parallel_tasks = slots;
    let mut held = daemon_board(Some(DISPATCH));
    held.parallel_tasks = slots;
    e.stub.on("GET /v1/boards", [board]);
    e.stub.on("POST /v1/boards/factory/coordinator", held);
    e
}

/// A task in `doing` owned by `owner`, as the list of tasks gives it.
fn doing_task(id: &str, owner: &str, blockers: &[&str]) -> board_proto::Task {
    let mut task = ready_task(id, board_proto::Priority::P1, -60);
    task.status = TaskStatus::Doing;
    task.ready = false;
    task.owner = Some(owner.parse().unwrap());
    task.blockers = blockers.iter().map(|b| b.parse().unwrap()).collect();
    task
}

fn titles(t3: &FakeT3) -> Vec<String> {
    worker_starts(t3)
        .into_iter()
        .map(|(title, _)| title)
        .collect()
}

#[tokio::test]
async fn two_slots_start_two_independent_tasks_and_an_unclaimed_worker_holds_its_slot() {
    use board_proto::Priority::P1;
    let tasks = [
        ready_task("factory-4", P1, 0),
        ready_task("factory-5", P1, 1),
        ready_task("factory-6", P1, 2),
    ];
    let mut e = slots_env(2, &tasks);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        titles(&e.t3),
        ["factory-4 Task factory-4", "factory-5 Task factory-5"]
    );
    assert_eq!(e.daemon.state.workers.len(), 2);

    // Neither has claimed its task yet: both still hold their slots.
    task_at(&e, "factory-4", TaskStatus::Open, None);
    task_at(&e, "factory-5", TaskStatus::Open, None);
    let later = at() + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 2);

    // A task in doing that no worker here follows counts with the unclaimed one.
    let mut e = slots_env(2, &tasks);
    e.stub.on(
        "GET /v1/tasks",
        [
            doing_task("factory-9", "t3:old-child", &[]),
            tasks[0].clone(),
            tasks[1].clone(),
        ],
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);
    task_at(&e, "factory-4", TaskStatus::Open, None);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1, "both slots are taken");

    // Parked, the worker frees its slot for the next task.
    let owner = format!("t3:{NEW_THREAD}");
    task_at(&e, "factory-4", TaskStatus::Review, Some(&owner));
    e.stub.on(
        "GET /v1/tasks",
        [
            doing_task("factory-9", "t3:old-child", &[]),
            tasks[1].clone(),
        ],
    );
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(
        titles(&e.t3),
        ["factory-4 Task factory-4", "factory-5 Task factory-5"]
    );
}

#[tokio::test]
async fn a_task_that_waits_on_a_task_in_progress_or_that_one_waits_on_is_not_started() {
    use board_proto::Priority::{P1, P2};
    // It waits on the task in progress.
    // It waits on the task in progress: the server does not count it ready.
    let mut waits = ready_task("factory-4", P1, 0);
    waits.blockers = vec!["factory-9".parse().unwrap()];
    waits.ready = false;
    let mut e = slots_env(2, &[]);
    e.stub.on(
        "GET /v1/tasks",
        [doing_task("factory-9", "t3:other", &[]), waits.clone()],
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert!(worker_starts(&e.t3).is_empty());

    // An independent task takes the slot instead.
    let f5 = ready_task("factory-5", P2, 1);
    e.stub.on(
        "GET /v1/tasks",
        [doing_task("factory-9", "t3:other", &[]), waits, f5.clone()],
    );
    e.stub.on("GET /v1/tasks/factory-5", task_detail_for(f5));
    e.stub.on(
        "POST /v1/tasks/factory-5/notes",
        note(&"factory-5".parse().unwrap()),
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-5 Task factory-5"]);

    // The task in progress waits on it.
    let mut e = slots_env(2, &[ready_task("factory-4", P1, 0)]);
    e.stub.on(
        "GET /v1/tasks",
        [
            doing_task("factory-9", "t3:other", &["factory-4"]),
            ready_task("factory-4", P1, 0),
        ],
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert!(worker_starts(&e.t3).is_empty());
}

#[tokio::test]
async fn after_a_usage_limit_stop_since_the_tasks_in_progress_started_no_further_task_starts() {
    use board_proto::Priority::P1;
    let mut e = slots_env(2, &[ready_task("factory-4", P1, 0)]);
    e.stub.on(
        "GET /v1/tasks",
        [
            doing_task("factory-9", "t3:other", &[]),
            ready_task("factory-4", P1, 0),
        ],
    );
    // An agent of the board stopped on its usage limit after factory-9 started.
    let member = "t3:member";
    let mut info = agent_fixture(member, DEVICE, AgentState::Paused);
    info.boards = vec![factory()];
    e.stub.on(&format!("GET {}", agent_path(member)), &info);
    let stop = state_changed(
        40,
        member,
        AgentState::Paused,
        Some(at() + chrono::Duration::hours(2)),
    );
    e.daemon.on_event(&stop, at()).await.unwrap();
    assert_eq!(e.daemon.state.usage_stops.len(), 1);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(worker_starts(&e.t3).is_empty());

    // An agent of another board stopping does not count.
    info.boards = vec![];
    e.stub.on(&format!("GET {}", agent_path(member)), &info);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);

    // Nor does a stop from before the tasks in progress started.
    let mut e = slots_env(2, &[ready_task("factory-4", P1, 0)]);
    let mut started = doing_task("factory-9", "t3:other", &[]);
    started.updated_at = at() + chrono::Duration::minutes(5);
    e.stub
        .on("GET /v1/tasks", [started, ready_task("factory-4", P1, 0)]);
    info.boards = vec![factory()];
    e.stub.on(&format!("GET {}", agent_path(member)), &info);
    e.daemon.on_event(&stop, at()).await.unwrap();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(6))
        .await
        .unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);
}

#[tokio::test]
async fn a_usage_limit_stop_ends_at_its_reset_when_the_agent_runs_or_the_account_is_not_full() {
    use board_proto::Priority::P1;
    let member = "t3:member";
    let reset = at() + chrono::Duration::hours(2);
    // A board with a task in progress since before the stop, an agent of it paused on a
    // limit as `info` says, and the usage as `reading` says, if any.
    let env = |info: board_proto::Agent, reading: Option<serde_json::Value>| async move {
        let mut e = slots_env(2, &[ready_task("factory-4", P1, 0)]);
        e.stub.on(
            "GET /v1/tasks",
            [
                doing_task("factory-9", "t3:other", &[]),
                ready_task("factory-4", P1, 0),
            ],
        );
        e.stub.on(&format!("GET {}", agent_path(member)), &info);
        if let Some(reading) = reading {
            e.stub.on("GET /v1/usage", reading);
        }
        let stop = state_changed(40, member, AgentState::Paused, Some(reset));
        e.daemon.on_event(&stop, at()).await.unwrap();
        e
    };
    let mut paused = agent_fixture(member, DEVICE, AgentState::Paused);
    paused.boards = vec![factory()];
    paused.paused_until = Some(reset);

    // Before its reset, with no reading after it, the stop holds.
    let mut e = env(paused.clone(), None).await;
    e.daemon.reconcile(at()).await.unwrap();
    assert!(worker_starts(&e.t3).is_empty());
    // After its reset it no longer does.
    let later = reset + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);

    // An agent whose session ended on the limit holds until the reset too.
    let mut ended = paused.clone();
    ended.state = AgentState::Ended;
    let mut e = env(ended.clone(), None).await;
    e.daemon.reconcile(at()).await.unwrap();
    assert!(worker_starts(&e.t3).is_empty());
    // Ended after its reset, or after it ran again, the server keeps no `paused_until`:
    // the stop is over.
    ended.paused_until = None;
    let mut e = env(ended, None).await;
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);

    // An agent that runs again holds nothing.
    let mut running = paused.clone();
    running.state = AgentState::Running;
    let mut e = env(running, None).await;
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);

    // A reading after the stop with no full window: the limit was not the account's, such
    // as one model's, and the workers start.
    let read = at() + chrono::Duration::minutes(1);
    let mut e = env(paused.clone(), Some(usage_reading(50.0, read))).await;
    e.daemon.reconcile(read).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);

    // A full window in that reading keeps the stop.
    let mut e = env(paused.clone(), Some(usage_reading(100.0, read))).await;
    e.daemon.reconcile(read).await.unwrap();
    assert!(worker_starts(&e.t3).is_empty());

    // So does a reading without windows, which says nothing.
    let empty = json!({ "read_at": read, "device": DEVICE });
    let mut e = env(paused.clone(), Some(empty)).await;
    e.daemon.reconcile(read).await.unwrap();
    assert!(worker_starts(&e.t3).is_empty());

    // So does a reading from before the stop.
    let before = at() - chrono::Duration::minutes(1);
    let mut e = env(paused, Some(usage_reading(50.0, before))).await;
    e.daemon.reconcile(at()).await.unwrap();
    assert!(worker_starts(&e.t3).is_empty());
}

#[tokio::test]
async fn a_task_left_open_on_a_lease_overlap_waits_until_a_task_in_progress_ends() {
    use board_proto::Priority::P1;
    let tasks = [
        ready_task("factory-4", P1, 0),
        ready_task("factory-5", P1, 1),
    ];
    let mut e = slots_env(2, &tasks);
    e.daemon.reconcile(at()).await.unwrap();
    let owner = format!("t3:{NEW_THREAD}");
    task_at(&e, "factory-4", TaskStatus::Doing, Some(&owner));
    task_at(&e, "factory-5", TaskStatus::Doing, Some(&owner));
    let later = at() + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 2);

    // factory-5's lease overlaps factory-4: its worker gives it back and ends. It is not
    // started again while factory-4 is in progress, however long that takes.
    task_at(&e, "factory-5", TaskStatus::Open, None);
    e.daemon.reconcile(later).await.unwrap();
    let f4: board_proto::TaskId = "factory-4".parse().unwrap();
    let f5: board_proto::TaskId = "factory-5".parse().unwrap();
    assert_eq!(
        e.daemon.state.worker_skips[&f5],
        std::collections::BTreeSet::from([f4.clone()])
    );
    assert!(e.daemon.state.worker_retry.is_empty());
    let much_later = later + board::daemon::WORKER_RETRY * 3;
    e.daemon.reconcile(much_later).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 2);

    // Saved, so a restarted daemon keeps the skip.
    let restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    assert_eq!(restarted.state.worker_skips, e.daemon.state.worker_skips);

    // Moving factory-4 to review keeps its leases, so factory-5 stays skipped.
    let f4_review = review_task("factory-4", 0);
    task_at(&e, "factory-4", TaskStatus::Review, Some(&owner));
    e.stub.on("GET /v1/tasks", [f4_review, tasks[1].clone()]);
    e.daemon.reconcile(much_later).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 2);
    assert_eq!(
        e.daemon.state.worker_skips[&f5],
        std::collections::BTreeSet::from([f4.clone()])
    );

    // factory-4 leaves review: factory-5 starts again at once.
    task_at(&e, "factory-4", TaskStatus::Done, Some(&owner));
    e.stub.on("GET /v1/tasks", &tasks[1..]);
    e.daemon.reconcile(much_later).await.unwrap();
    let starts = titles(&e.t3);
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[2], "factory-5 Task factory-5");
    assert!(e.daemon.state.worker_skips.is_empty());
}

#[tokio::test]
async fn a_task_left_open_beside_a_review_task_waits_until_review_ends() {
    use board_proto::Priority::P1;
    let f4_review = review_task("factory-4", 0);
    let f5_ready = ready_task("factory-5", P1, 1);
    let tasks = [f4_review, f5_ready.clone()];
    let mut e = slots_env(1, &tasks);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-5 Task factory-5"]);

    let owner = format!("t3:{NEW_THREAD}");
    task_at(&e, "factory-5", TaskStatus::Doing, Some(&owner));
    let later = at() + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);

    // factory-5 overlaps factory-4 (which is in review, with nothing in doing) and gives
    // it back: it is skipped until factory-4 leaves review, not put on WORKER_RETRY.
    task_at(&e, "factory-5", TaskStatus::Open, None);
    e.daemon.reconcile(later).await.unwrap();
    let f4: board_proto::TaskId = "factory-4".parse().unwrap();
    let f5: board_proto::TaskId = "factory-5".parse().unwrap();
    assert_eq!(
        e.daemon.state.worker_skips[&f5],
        std::collections::BTreeSet::from([f4.clone()])
    );
    assert!(e.daemon.state.worker_retry.is_empty());
    let drops = read_daemon_drops(&e);
    assert!(
        drops.iter().any(|d| {
            d["kind"] == "worker_give_back"
                && d["task"] == "factory-5"
                && d["subject"]["waiting_on"] == json!(["factory-4"])
        }),
        "expected worker_give_back drop waiting on factory-4, got {drops:?}"
    );

    // Saved, so a restarted daemon keeps the skip.
    let restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    assert_eq!(restarted.state.worker_skips, e.daemon.state.worker_skips);

    // Two minutes later, while factory-4 is still in review, factory-5 stays skipped.
    let soon = later + chrono::Duration::minutes(2);
    e.daemon.reconcile(soon).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);

    // As soon as factory-4 leaves review (well before WORKER_RETRY), factory-5 starts on
    // the next reconcile without waiting 30 minutes.
    task_at(&e, "factory-4", TaskStatus::Done, Some(WORKER));
    e.stub.on("GET /v1/tasks", [f5_ready.clone()]);
    e.daemon.reconcile(soon).await.unwrap();
    assert_eq!(
        titles(&e.t3),
        ["factory-5 Task factory-5", "factory-5 Task factory-5"]
    );
    assert!(e.daemon.state.worker_skips.is_empty());

    // Conversely, if a review task stays in review past WORKER_RETRY, the skipped task
    // keeps waiting instead of retrying after 30 minutes.
    let mut e = slots_env(1, &tasks);
    e.daemon.reconcile(at()).await.unwrap();
    task_at(&e, "factory-5", TaskStatus::Doing, Some(&owner));
    e.daemon.reconcile(later).await.unwrap();
    task_at(&e, "factory-5", TaskStatus::Open, None);
    e.daemon.reconcile(later).await.unwrap();
    let much_later = later + board::daemon::WORKER_RETRY * 3;
    e.daemon.reconcile(much_later).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);
    assert_eq!(
        e.daemon.state.worker_skips[&f5],
        std::collections::BTreeSet::from([f4])
    );
}

#[tokio::test]
async fn a_task_left_open_with_no_doing_or_review_task_waits_worker_retry() {
    use board_proto::Priority::P1;
    let tasks = [ready_task("factory-5", P1, 0)];
    let mut e = slots_env(1, &tasks);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-5 Task factory-5"]);

    let owner = format!("t3:{NEW_THREAD}");
    task_at(&e, "factory-5", TaskStatus::Doing, Some(&owner));
    let later = at() + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();

    // No other task is in doing or review: giving the task back triggers WORKER_RETRY.
    task_at(&e, "factory-5", TaskStatus::Open, None);
    e.daemon.reconcile(later).await.unwrap();
    let f5: board_proto::TaskId = "factory-5".parse().unwrap();
    let retry_at = later + board::daemon::WORKER_RETRY;
    assert!(e.daemon.state.worker_skips.is_empty());
    assert_eq!(e.daemon.state.worker_retry.get(&f5), Some(&retry_at));
    let drops = read_daemon_drops(&e);
    assert!(
        drops.iter().any(|d| {
            d["kind"] == "worker_give_back"
                && d["task"] == "factory-5"
                && d["subject"]["retry_at"].is_string()
        }),
        "expected worker_give_back retry drop for factory-5, got {drops:?}"
    );

    // Still held just before WORKER_RETRY expires, then eligible once it passes.
    e.daemon
        .reconcile(retry_at - chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);
    e.daemon.reconcile(retry_at).await.unwrap();
    assert_eq!(
        titles(&e.t3),
        ["factory-5 Task factory-5", "factory-5 Task factory-5"]
    );
}

/// A daemon-dispatch board this device runs, its lease held, with factory-7 `status` and
/// owned by `owner`, ready to be set open.
async fn owed_env(status: TaskStatus, owner: &str) -> Env {
    let mut e = dispatch_env(&[]);
    e.daemon.reconcile(at()).await.unwrap();
    let mut task = detail("factory-7", false, &[]);
    task.task.status = status;
    task.task.owner = Some(owner.parse().unwrap());
    e.stub.on("GET /v1/tasks/factory-7", &task);
    e.stub
        .on("POST /v1/tasks/factory-7/notes", note(&task.task.id));
    let mut open = task.task.clone();
    open.status = TaskStatus::Open;
    open.owner = None;
    e.stub.on("POST /v1/tasks/factory-7/release", open);
    if status == TaskStatus::Doing {
        e.stub
            .on("GET /v1/tasks", [doing_task("factory-7", owner, &[])]);
    }
    e
}

fn set_open(e: &Env) -> Vec<Recorded> {
    e.stub.requests_to("POST /v1/tasks/factory-7/release")
}

#[tokio::test]
async fn an_answer_to_a_gone_asker_is_noted_on_its_task_and_the_task_set_open() {
    // An asker no daemon can wake, ended: the dispatcher does the step on its reconcile.
    let asker = "claude:session";
    let mut e = owed_env(TaskStatus::Blocked, asker).await;
    e.stub.on(
        &format!("GET {}", agent_path(asker)),
        agent_fixture(asker, DEVICE, AgentState::Ended),
    );
    let ev = answered(&e, 30, 3, asker);
    assert!(e.daemon.on_event(&ev, at()).await.unwrap());
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty());
    e.daemon.reconcile(at()).await.unwrap();

    let notes = e.stub.requests_to("POST /v1/tasks/factory-7/notes");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].agent.as_deref(), Some(DISPATCH));
    assert_eq!(notes[0].body["kind"], "decision");
    let text = notes[0].body["text"].as_str().unwrap();
    assert!(
        text.contains("ask #3") && text.contains("sqlite <b>"),
        "{text}"
    );
    let opened = set_open(&e);
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].body, json!({ "agent": DISPATCH }));
    assert!(e.daemon.state.owed.is_empty());

    // A task its asker parked in review goes back to open too.
    let mut e = owed_env(TaskStatus::Review, asker).await;
    e.stub.on(
        &format!("GET {}", agent_path(asker)),
        agent_fixture(asker, DEVICE, AgentState::Ended),
    );
    let ev = answered(&e, 30, 3, asker);
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(set_open(&e).len(), 1);

    // A live asker hears of its answer itself: nothing is done for it while it is there.
    let mut e = owed_env(TaskStatus::Blocked, asker).await;
    e.stub.on(
        &format!("GET {}", agent_path(asker)),
        agent_fixture(asker, DEVICE, AgentState::Idle),
    );
    let ev = answered(&e, 30, 3, asker);
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-7/notes")
            .is_empty()
    );
    assert!(set_open(&e).is_empty());
    assert_eq!(
        e.daemon.state.owed.len(),
        1,
        "kept while the asker is there"
    );

    // It ends without taking the answer up: the step is done.
    e.stub.on(
        &format!("GET {}", agent_path(asker)),
        agent_fixture(asker, DEVICE, AgentState::Ended),
    );
    let later = at() + chrono::Duration::minutes(5);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(set_open(&e).len(), 1);
    assert!(e.daemon.state.owed.is_empty());

    // A T3 asker whose thread is gone: found at the flush, then done by the dispatcher.
    let gone = "t3:gone";
    let mut e = owed_env(TaskStatus::Blocked, gone).await;
    e.stub.on(
        &format!("GET {}", agent_path(gone)),
        agent_fixture(gone, DEVICE, AgentState::Ended),
    );
    let ev = answered(&e, 30, 3, gone);
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    e.daemon.flush(at()).await;
    assert_eq!(e.daemon.state.owed.len(), 1);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-7/notes").len(),
        1
    );
    assert_eq!(set_open(&e).len(), 1);
    assert!(e.t3.sends().is_empty(), "no thread to pass it to");

    // Another home device holds the lease: this one owes nothing it could never do.
    let mut e = owed_env(TaskStatus::Blocked, asker).await;
    e.stub.on(
        "GET /v1/boards",
        [daemon_board(Some("dispatch:other-device"))],
    );
    e.stub.on_error(
        "POST /v1/boards/factory/coordinator",
        409,
        "claim_refused",
        "held by dispatch:other-device",
        "wait",
    );
    e.daemon.reconcile(at()).await.unwrap();
    e.stub.on(
        &format!("GET {}", agent_path(asker)),
        agent_fixture(asker, DEVICE, AgentState::Ended),
    );
    let ev = answered(&e, 30, 3, asker);
    e.daemon.on_event(&ev, at()).await.unwrap();
    assert!(e.daemon.state.owed.is_empty());
}

#[tokio::test]
async fn an_answer_to_a_gone_asker_reopens_an_ownerless_blocked_task() {
    // An ownerless blocked task whose asker cannot be woken cannot use `/release` (the server
    // refuses `POST /v1/tasks/{id}/release` with 403 `nobody owns ...`): the dispatcher must
    // note the answer and move the task to `open` via `PATCH /v1/tasks/{id}`.
    let asker = "claude:session";
    let mut e = dispatch_env(&[]);
    e.daemon.reconcile(at()).await.unwrap();
    let mut blocked = detail("factory-7", false, &[]);
    blocked.task.status = TaskStatus::Blocked;
    blocked.task.owner = None;
    blocked.task.blocked_reason = Some("waiting on ask #3".into());
    blocked.task.version = 4;
    e.stub.on("GET /v1/tasks/factory-7", &blocked);
    e.stub
        .on("POST /v1/tasks/factory-7/notes", note(&blocked.task.id));
    e.stub.on_error(
        "POST /v1/tasks/factory-7/release",
        403,
        "forbidden",
        "nobody owns factory-7, so it cannot be released by you",
        "claim it first with task_claim factory-7",
    );
    let mut opened = blocked.task.clone();
    opened.status = TaskStatus::Open;
    opened.owner = None;
    opened.blocked_reason = None;
    opened.version = 5;
    e.stub.on("PATCH /v1/tasks/factory-7", opened);
    e.stub.on(
        &format!("GET {}", agent_path(asker)),
        agent_fixture(asker, DEVICE, AgentState::Ended),
    );

    let ev = answered(&e, 30, 3, asker);
    assert!(e.daemon.on_event(&ev, at()).await.unwrap());
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty());
    e.daemon.reconcile(at()).await.unwrap();

    let notes = e.stub.requests_to("POST /v1/tasks/factory-7/notes");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].agent.as_deref(), Some(DISPATCH));
    assert_eq!(notes[0].body["kind"], "decision");
    let text = notes[0].body["text"].as_str().unwrap();
    assert!(
        text.contains("ask #3") && text.contains("sqlite <b>"),
        "{text}"
    );
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-7/release")
            .is_empty(),
        "ownerless blocked task must not call /release"
    );
    let patched = e.stub.requests_to("PATCH /v1/tasks/factory-7");
    assert_eq!(patched.len(), 1);
    assert_eq!(patched[0].agent.as_deref(), Some(DISPATCH));
    assert_eq!(patched[0].body["agent"], DISPATCH);
    assert_eq!(patched[0].body["version"], 4);
    assert_eq!(patched[0].body["status"], "open");
    assert!(
        patched[0].body.get("blocked_reason").is_none()
            || patched[0].body["blocked_reason"].is_null(),
        "PATCH to open must not send a blocked_reason: {}",
        patched[0].body
    );
    assert!(e.daemon.state.owed.is_empty());
    assert!(
        !read_daemon_drops(&e)
            .iter()
            .any(|d| d["kind"] == "owed_failed"),
        "reopening an ownerless blocked task must not record an owed_failed drop"
    );
}

/// The server's idle notice for factory-7, away since 40 minutes before `at()`, at `noticed`.
fn idle_notice(seq: i64, away: &str, noticed: Timestamp) -> Event {
    let since = at() - chrono::Duration::minutes(40);
    let mut notice = event(seq, EventType::TaskUpdated, Some("factory-7"), "server");
    notice.at = noticed;
    notice.payload = json!({ "fields": [], "idle_doing": since, "away": away });
    notice
}

#[tokio::test]
async fn an_owner_still_away_at_the_repeat_notice_loses_its_task() {
    let owner = "t3:worker";
    let mut e = owed_env(TaskStatus::Doing, owner).await;
    let mut info = agent_fixture(owner, DEVICE, AgentState::Idle);
    info.branch = Some("task/factory-7".into());
    info.native_ids.insert(
        "t3_link".into(),
        "https://t3.example.test/threads/worker".into(),
    );
    e.stub.on(&format!("GET {}", agent_path(owner)), &info);

    // The first notice wakes the owner as before, and releases nothing.
    e.daemon
        .on_event(&idle_notice(30, owner, at()), at())
        .await
        .unwrap();
    e.daemon.reconcile(at()).await.unwrap();
    assert!(set_open(&e).is_empty());
    assert_eq!(e.daemon.state.wakes.len(), 1);

    // The repeat wakes the owner again; the dispatcher gives it time to come back.
    let repeat = at() + chrono::Duration::minutes(40);
    assert!(
        e.daemon
            .on_event(&idle_notice(31, owner, repeat), repeat)
            .await
            .unwrap()
    );
    e.daemon.reconcile(repeat).await.unwrap();
    assert!(set_open(&e).is_empty());
    assert_eq!(e.daemon.state.owed.len(), 1);

    // Still away after that: the task goes back to open with a note.
    let graced = repeat + RELEASE_GRACE;
    e.daemon.reconcile(graced).await.unwrap();
    let opened = set_open(&e);
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].agent.as_deref(), Some(DISPATCH));
    let notes = e.stub.requests_to("POST /v1/tasks/factory-7/notes");
    assert_eq!(notes.len(), 1);
    let text = notes[0].body["text"].as_str().unwrap();
    assert!(
        text.contains("https://t3.example.test/threads/worker") && text.contains("task/factory-7"),
        "{text}"
    );
    assert!(e.daemon.state.owed.is_empty());
    assert!(e.daemon.state.idle_notices.is_empty());

    // An owner back at work since the repeat keeps its task.
    let mut e = owed_env(TaskStatus::Doing, owner).await;
    let mut back = agent_fixture(owner, DEVICE, AgentState::Running);
    back.last_seen = repeat + chrono::Duration::minutes(1);
    e.stub.on(&format!("GET {}", agent_path(owner)), &back);
    e.daemon
        .on_event(&idle_notice(30, owner, at()), at())
        .await
        .unwrap();
    e.daemon
        .on_event(&idle_notice(31, owner, repeat), repeat)
        .await
        .unwrap();
    e.daemon.reconcile(graced).await.unwrap();
    assert!(set_open(&e).is_empty());
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-7/notes")
            .is_empty()
    );
    assert!(e.daemon.state.owed.is_empty());

    // A notice of a new spell is a first notice again.
    let mut e = owed_env(TaskStatus::Doing, owner).await;
    e.stub.on(&format!("GET {}", agent_path(owner)), &info);
    e.daemon
        .on_event(&idle_notice(30, owner, at()), at())
        .await
        .unwrap();
    let mut later = idle_notice(31, owner, repeat);
    later.payload["idle_doing"] = json!(at());
    e.daemon.on_event(&later, repeat).await.unwrap();
    e.daemon.reconcile(graced).await.unwrap();
    assert!(set_open(&e).is_empty());
}

#[tokio::test]
async fn a_worker_whose_thread_is_gone_with_its_task_in_doing_frees_the_slot() {
    let tasks = [ready_task("factory-4", board_proto::Priority::P1, 0)];
    let mut e = dispatch_env(&tasks);
    e.daemon.reconcile(at()).await.unwrap();
    let worker = format!("t3:{NEW_THREAD}");
    task_at(&e, "factory-4", TaskStatus::Doing, Some(&worker));
    let later = at() + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);

    // Its thread is archived, its agent ended: the task stays in doing, holding the slot,
    // until the server's notice that its owner is away.
    e.t3.threads.lock().unwrap().remove(NEW_THREAD);
    let mut info = agent_fixture(&worker, DEVICE, AgentState::Ended);
    info.branch = Some("task/factory-4".into());
    e.stub.on(&format!("GET {}", agent_path(&worker)), &info);
    let mut doing = detail("factory-4", false, &[]);
    doing.task.status = TaskStatus::Doing;
    doing.task.owner = Some(worker.parse().unwrap());
    let mut open = doing.task.clone();
    open.status = TaskStatus::Open;
    open.owner = None;
    e.stub.on("POST /v1/tasks/factory-4/release", open);
    let noticed = at() + chrono::Duration::minutes(40);
    let mut notice = event(30, EventType::TaskUpdated, Some("factory-4"), "server");
    notice.at = noticed;
    notice.payload = json!({ "fields": [], "idle_doing": later, "away": worker });
    e.daemon.on_event(&notice, noticed).await.unwrap();
    e.daemon.flush(noticed).await;
    e.daemon.flush(noticed).await;
    assert_eq!(
        e.daemon.state.owed.len(),
        1,
        "the wake found the thread gone"
    );

    // The task goes back to open, naming the old thread and branch, and a new worker
    // starts for it at once.
    // On a real server `POST /v1/tasks/factory-4/release` moves the task to open, so the
    // read inside `pay_owed` sees `doing` and the re-check before `start_worker` sees `open`.
    e.stub.once("GET /v1/tasks/factory-4", &doing);
    task_at(&e, "factory-4", TaskStatus::Open, None);
    e.daemon.reconcile(noticed + RELEASE_GRACE).await.unwrap();
    let opened = e.stub.requests_to("POST /v1/tasks/factory-4/release");
    assert_eq!(opened.len(), 1);
    let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
    let texts: Vec<&str> = notes
        .iter()
        .map(|n| n.body["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts.len(), 3, "{texts:?}");
    assert!(
        texts[1].contains(&worker) && texts[1].contains("task/factory-4"),
        "{}",
        texts[1]
    );
    assert_eq!(worker_starts(&e.t3).len(), 2);
}

#[tokio::test]
async fn reconcile_releases_away_owners_when_repeat_notice_never_reached_the_dispatcher() {
    let twice = chrono::Duration::minutes(60);

    // 1. Both idle_doing notices were sent BEFORE this dispatcher started (Gap 1), and the
    //    owner runs on another device (Gap 3): reconcile scans `doing` task history, owes a
    //    Release, and immediately releases once past `RELEASE_GRACE`.
    let owner = "t3:other-device-worker";
    let mut e = owed_env(TaskStatus::Doing, owner).await;
    let since = at() - chrono::Duration::minutes(90);
    let first_at = at() - chrono::Duration::minutes(55);
    let repeat_at = at() - chrono::Duration::minutes(20);
    let mut info = agent_fixture(owner, "other-device", AgentState::Ended);
    info.last_seen = first_at + chrono::Duration::minutes(1); // T3 closed session before repeat
    info.branch = Some("task/factory-7".into());
    e.stub.on(&format!("GET {}", agent_path(owner)), &info);

    let mut claimed = event(10, EventType::TaskClaimed, Some("factory-7"), owner);
    claimed.at = since - chrono::Duration::minutes(10);
    let mut n1 = event(11, EventType::TaskUpdated, Some("factory-7"), "server");
    n1.at = first_at;
    n1.payload = json!({ "fields": [], "idle_doing": since, "away": owner });
    let mut n2 = event(12, EventType::TaskUpdated, Some("factory-7"), "server");
    n2.at = repeat_at;
    n2.payload = json!({ "fields": [], "idle_doing": since, "away": owner });

    let mut task_detail = detail("factory-7", false, &[]);
    task_detail.task.status = TaskStatus::Doing;
    task_detail.task.owner = Some(owner.parse().unwrap());
    task_detail.history = vec![claimed, n1, n2];
    e.stub.on("GET /v1/tasks/factory-7", &task_detail);

    // No SSE events delivered to this daemon; reconcile alone scans and releases.
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(set_open(&e).len(), 1);
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-7/notes").len(),
        1
    );
    assert!(e.daemon.state.owed.is_empty());

    // 2. Late first notice arriving at twice the idle period (Gap 2): the server sends no
    //    separate repeat, so live `on_event` owes a Release directly and releases after grace.
    let late_owner = "t3:late-worker";
    let mut e = owed_env(TaskStatus::Doing, late_owner).await;
    let mut late_info = agent_fixture(late_owner, DEVICE, AgentState::Idle);
    late_info.last_seen = at() - twice;
    e.stub
        .on(&format!("GET {}", agent_path(late_owner)), &late_info);
    let mut late_notice = event(20, EventType::TaskUpdated, Some("factory-7"), "server");
    late_notice.at = at();
    late_notice.payload = json!({
        "fields": [],
        "idle_doing": at() - twice,
        "away": late_owner
    });
    assert!(e.daemon.on_event(&late_notice, at()).await.unwrap());
    assert_eq!(e.daemon.state.owed.len(), 1);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(set_open(&e).is_empty(), "waits out RELEASE_GRACE");
    e.daemon.reconcile(at() + RELEASE_GRACE).await.unwrap();
    assert_eq!(set_open(&e).len(), 1);

    // 3. Reconcile scan also catches a late first notice from task history even without SSE,
    //    and a single missed notice when the local T3 thread is archived/gone.
    let late_hist_owner = "t3:late-history-remote";
    let mut e = owed_env(TaskStatus::Doing, late_hist_owner).await;
    let mut late_hist_info = agent_fixture(late_hist_owner, "other-device", AgentState::Idle);
    late_hist_info.last_seen = at() - chrono::Duration::minutes(75);
    e.stub.on(
        &format!("GET {}", agent_path(late_hist_owner)),
        &late_hist_info,
    );
    let mut late_hist_notice = event(22, EventType::TaskUpdated, Some("factory-7"), "server");
    late_hist_notice.at = at() - chrono::Duration::minutes(10);
    late_hist_notice.payload = json!({
        "fields": [],
        "idle_doing": at() - chrono::Duration::minutes(75),
        "away": late_hist_owner
    });
    let mut late_hist_detail = detail("factory-7", false, &[]);
    late_hist_detail.task.status = TaskStatus::Doing;
    late_hist_detail.task.owner = Some(late_hist_owner.parse().unwrap());
    late_hist_detail.history = vec![late_hist_notice];
    e.stub.on("GET /v1/tasks/factory-7", &late_hist_detail);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        set_open(&e).len(),
        1,
        "late first notice in history released"
    );

    let gone_local = "t3:archived-local";
    let mut e = owed_env(TaskStatus::Doing, gone_local).await;
    let mut gone_info = agent_fixture(gone_local, DEVICE, AgentState::Ended);
    gone_info.last_seen = at() - chrono::Duration::minutes(40);
    e.stub
        .on(&format!("GET {}", agent_path(gone_local)), &gone_info);
    let mut single_notice = event(21, EventType::TaskUpdated, Some("factory-7"), "server");
    single_notice.at = at() - chrono::Duration::minutes(10);
    single_notice.payload = json!({
        "fields": [],
        "idle_doing": at() - chrono::Duration::minutes(45),
        "away": gone_local
    });
    let mut gone_detail = detail("factory-7", false, &[]);
    gone_detail.task.status = TaskStatus::Doing;
    gone_detail.task.owner = Some(gone_local.parse().unwrap());
    gone_detail.history = vec![single_notice.clone()];
    e.stub.on("GET /v1/tasks/factory-7", &gone_detail);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(set_open(&e).len(), 1, "archived local thread released");

    // A single early notice for a remote owner must NOT release before the repeat notice.
    let remote_early = "t3:remote-early";
    let mut e = owed_env(TaskStatus::Doing, remote_early).await;
    let mut remote_info = agent_fixture(remote_early, "other-device", AgentState::Ended);
    remote_info.last_seen = at() - chrono::Duration::minutes(45);
    e.stub
        .on(&format!("GET {}", agent_path(remote_early)), &remote_info);
    let mut early_detail = detail("factory-7", false, &[]);
    early_detail.task.status = TaskStatus::Doing;
    early_detail.task.owner = Some(remote_early.parse().unwrap());
    single_notice.payload["away"] = json!(remote_early);
    early_detail.history = vec![single_notice.clone()];
    e.stub.on("GET /v1/tasks/factory-7", &early_detail);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        set_open(&e).is_empty(),
        "single early notice for remote T3 owner must wait for repeat"
    );

    // A single early notice for an ended non-T3 owner must also wait for the repeat notice
    // (matching docs/DESIGN.md and pull-dispatch housekeeping).
    let non_t3_ended = "claude:cli-ended";
    let mut e = owed_env(TaskStatus::Doing, non_t3_ended).await;
    let mut non_t3_info = agent_fixture(non_t3_ended, DEVICE, AgentState::Ended);
    non_t3_info.last_seen = at() - chrono::Duration::minutes(45);
    e.stub
        .on(&format!("GET {}", agent_path(non_t3_ended)), &non_t3_info);
    let mut non_t3_detail = detail("factory-7", false, &[]);
    non_t3_detail.task.status = TaskStatus::Doing;
    non_t3_detail.task.owner = Some(non_t3_ended.parse().unwrap());
    let mut non_t3_notice = single_notice.clone();
    non_t3_notice.payload["away"] = json!(non_t3_ended);
    non_t3_detail.history = vec![non_t3_notice];
    e.stub.on("GET /v1/tasks/factory-7", &non_t3_detail);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        set_open(&e).is_empty(),
        "single early notice for ended non-T3 owner must wait for repeat"
    );

    // Local T3 owner marked Running and seen after the notice must not thrash owe/drop
    // even if its local thread reports not runnable.
    let running_gone = "t3:running-after-notice";
    let mut e = owed_env(TaskStatus::Doing, running_gone).await;
    let mut running_info = agent_fixture(running_gone, DEVICE, AgentState::Running);
    running_info.last_seen = single_notice.at + chrono::Duration::minutes(2);
    e.stub
        .on(&format!("GET {}", agent_path(running_gone)), &running_info);
    let mut running_detail = detail("factory-7", false, &[]);
    running_detail.task.status = TaskStatus::Doing;
    running_detail.task.owner = Some(running_gone.parse().unwrap());
    let mut running_notice = single_notice;
    running_notice.payload["away"] = json!(running_gone);
    running_detail.history = vec![running_notice];
    e.stub.on("GET /v1/tasks/factory-7", &running_detail);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        set_open(&e).is_empty(),
        "running owner seen after notice must keep its task"
    );
    assert!(
        e.daemon.state.owed.is_empty(),
        "must not owe a release that pay() would immediately drop"
    );

    // 4. Guards: an owner waiting on a usage-limit reset or active after the notice keeps its
    //    task, and a single early notice for a live remote owner does not release.
    let reset_owner = "t3:paused-reset";
    let mut e = owed_env(TaskStatus::Doing, reset_owner).await;
    let mut reset_info = agent_fixture(reset_owner, "other-device", AgentState::Ended);
    reset_info.paused_until = Some(at() + chrono::Duration::hours(2));
    reset_info.last_seen = repeat_at - chrono::Duration::minutes(5);
    e.stub
        .on(&format!("GET {}", agent_path(reset_owner)), &reset_info);
    let mut reset_detail = detail("factory-7", false, &[]);
    reset_detail.task.status = TaskStatus::Doing;
    reset_detail.task.owner = Some(reset_owner.parse().unwrap());
    let mut r1 = event(30, EventType::TaskUpdated, Some("factory-7"), "server");
    r1.at = first_at;
    r1.payload = json!({ "fields": [], "idle_doing": since, "away": reset_owner });
    let mut r2 = event(31, EventType::TaskUpdated, Some("factory-7"), "server");
    r2.at = repeat_at;
    r2.payload = json!({ "fields": [], "idle_doing": since, "away": reset_owner });
    reset_detail.history = vec![r1.clone(), r2.clone()];
    e.stub.on("GET /v1/tasks/factory-7", &reset_detail);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        set_open(&e).is_empty(),
        "owner waiting on usage-limit reset must keep its task"
    );

    // Owner that showed activity after the repeat notice (including resuming and later
    // ending) must not be prematurely released from a stale history spell.
    for state in [AgentState::Idle, AgentState::Ended] {
        let active_owner = "t3:active-again";
        let mut e = owed_env(TaskStatus::Doing, active_owner).await;
        let mut active_info = agent_fixture(active_owner, "other-device", state);
        active_info.last_seen = repeat_at + chrono::Duration::minutes(2);
        e.stub
            .on(&format!("GET {}", agent_path(active_owner)), &active_info);
        let mut active_detail = detail("factory-7", false, &[]);
        active_detail.task.status = TaskStatus::Doing;
        active_detail.task.owner = Some(active_owner.parse().unwrap());
        let mut a1 = r1.clone();
        let mut a2 = r2.clone();
        a1.payload["away"] = json!(active_owner);
        a2.payload["away"] = json!(active_owner);
        active_detail.history = vec![a1, a2];
        e.stub.on("GET /v1/tasks/factory-7", &active_detail);
        e.daemon.reconcile(at()).await.unwrap();
        assert!(
            set_open(&e).is_empty(),
            "owner seen after repeat notice ({state:?}) must keep its task"
        );
    }

    // Live SSE: returning to running during RELEASE_GRACE cancels a pending Owed::Release
    // even if the owner stops back to Idle before reconcile runs.
    let resumed_owner = "t3:resumed-briefly";
    let mut e = owed_env(TaskStatus::Doing, resumed_owner).await;
    let mut resumed_info = agent_fixture(resumed_owner, DEVICE, AgentState::Idle);
    resumed_info.last_seen = at() + chrono::Duration::minutes(2);
    e.stub
        .on(&format!("GET {}", agent_path(resumed_owner)), &resumed_info);
    let mut late_ev = event(40, EventType::TaskUpdated, Some("factory-7"), "server");
    late_ev.at = at();
    late_ev.payload = json!({
        "fields": [],
        "idle_doing": at() - twice,
        "away": resumed_owner
    });
    assert!(e.daemon.on_event(&late_ev, at()).await.unwrap());
    assert_eq!(e.daemon.state.owed.len(), 1);
    let mut back_ev = event(41, EventType::AgentStateChanged, None, resumed_owner);
    back_ev.at = at() + chrono::Duration::minutes(1);
    back_ev.payload = json!({ "from": "idle", "to": "running", "paused_until": null });
    e.daemon
        .on_event(&back_ev, at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert!(e.daemon.state.owed.is_empty());
    e.daemon.reconcile(at() + RELEASE_GRACE).await.unwrap();
    assert!(
        set_open(&e).is_empty(),
        "owner that resumed during grace must not lose its task"
    );
}

/// A reading with a weekly pace whose projection is `projected` against a line at 80.
fn paced_reading(projected: f64, read: Timestamp) -> serde_json::Value {
    let mut v = usage_reading(70.0, read);
    v["pace"] = json!({
        "reserve": 5.5, "day_start": read - chrono::Duration::hours(1), "anchor": 70.0,
        "line": 80.0, "projected": projected,
    });
    v
}

#[tokio::test]
async fn the_daemon_reads_the_usage_every_minute_near_a_pace_line_until_refused() {
    use board::daemon::{USAGE_POLL_EVERY, USAGE_POLL_NEAR, USAGE_REFUSED_BACKOFF};
    use board::usage::{Credentials, UsageSource};

    let (mut e, _) = owner_env(AgentState::Idle, None);
    let creds = e._dir.path().join("credentials.json");
    std::fs::write(
        &creds,
        json!({ "claudeAiOauth": {
            "accessToken": "tok-123",
            "expiresAt": (at() + chrono::Duration::hours(8)).timestamp_millis(),
        }})
        .to_string(),
    )
    .unwrap();
    e.daemon.usage_source = UsageSource::ClaudeCode {
        url: format!("{}/api/oauth/usage", e.stub.url),
        credentials: Credentials::File(creds),
        snapshots: Vec::new(),
    };
    e.stub.on(
        "GET /api/oauth/usage",
        json!({ "seven_day": { "utilization": 70.0, "resets_at": null } }),
    );
    let reads = |e: &Env| e.stub.requests_to("GET /api/oauth/usage").len();

    // Far from the line: every 5 minutes.
    e.stub.on("PUT /v1/usage", paced_reading(72.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.reconcile(at() + USAGE_POLL_NEAR).await.unwrap();
    assert_eq!(reads(&e), 1);

    // Within 2 points of it: every minute.
    let t = at() + USAGE_POLL_EVERY;
    e.stub.on("PUT /v1/usage", paced_reading(78.5, t));
    e.daemon.reconcile(t).await.unwrap();
    assert_eq!(reads(&e), 2);
    e.daemon.reconcile(t + USAGE_POLL_NEAR).await.unwrap();
    assert_eq!(reads(&e), 3);

    // A refusal: every 5 minutes for 30 minutes.
    let refused = t + USAGE_POLL_NEAR * 2;
    e.stub.on_status("GET /api/oauth/usage", 429, json!({}));
    e.stub.on("GET /v1/usage", paced_reading(78.5, refused));
    e.daemon.reconcile(refused).await.unwrap();
    assert_eq!(reads(&e), 4);
    e.daemon.reconcile(refused + USAGE_POLL_NEAR).await.unwrap();
    assert_eq!(reads(&e), 4, "backing off");
    let after = refused + USAGE_POLL_EVERY;
    e.stub.on(
        "GET /api/oauth/usage",
        json!({ "seven_day": { "utilization": 70.0, "resets_at": null } }),
    );
    e.stub.on("PUT /v1/usage", paced_reading(78.5, after));
    e.daemon.reconcile(after).await.unwrap();
    assert_eq!(reads(&e), 5);
    // After the back-off, every minute again.
    let done = refused + USAGE_REFUSED_BACKOFF;
    e.stub.on("PUT /v1/usage", paced_reading(78.5, done));
    e.daemon.reconcile(done).await.unwrap();
    assert_eq!(reads(&e), 6);
    let next = done + USAGE_POLL_NEAR;
    e.stub.on("PUT /v1/usage", paced_reading(78.5, next));
    e.daemon.reconcile(next).await.unwrap();
    assert_eq!(reads(&e), 7);
}

#[tokio::test]
async fn the_weekly_hard_line_holds_every_wake_of_its_boards() {
    let (mut e, agent) = owner_env(AgentState::Idle, None);
    let mut hard = paced_reading(80.0, at());
    hard["pace"]["hard_since"] = json!(at());
    e.stub.on("GET /v1/usage", hard);
    e.daemon.reconcile(at()).await.unwrap();
    let ev = answered(&e, 11, 3, agent.as_str());
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());

    // Below the hard line, an answer goes as before.
    let later = at() + chrono::Duration::minutes(1);
    e.stub.on("GET /v1/usage", paced_reading(70.0, later));
    e.daemon.reconcile(later).await.unwrap();
    let ev = answered(&e, 12, 4, agent.as_str());
    e.daemon.on_event(&ev, later).await.unwrap();
    e.daemon.flush(later).await;
    let text: String = e.t3.sends().iter().map(|(_, m)| m.as_str()).collect();
    assert!(text.contains("ask #4"), "{text}");
}

#[tokio::test]
async fn a_failed_usage_report_keeps_the_last_pace() {
    use board::usage::{Credentials, UsageSource};

    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.stub.on("GET /v1/usage", paced_reading(80.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.daemon.usage_holds(&factory(), at()));

    let creds = e._dir.path().join("credentials.json");
    std::fs::write(
        &creds,
        json!({ "claudeAiOauth": {
            "accessToken": "tok-123",
            "expiresAt": (at() + chrono::Duration::hours(8)).timestamp_millis(),
        }})
        .to_string(),
    )
    .unwrap();
    e.daemon.usage_source = UsageSource::ClaudeCode {
        url: format!("{}/api/oauth/usage", e.stub.url),
        credentials: Credentials::File(creds),
        snapshots: Vec::new(),
    };
    e.stub.on(
        "GET /api/oauth/usage",
        json!({ "seven_day": { "utilization": 70.0, "resets_at": null } }),
    );
    e.stub
        .on_status("PUT /v1/usage", 400, json!({ "code": "invalid" }));
    let later = at() + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(e.stub.requests_to("PUT /v1/usage").len(), 1);
    assert!(e.daemon.usage_holds(&factory(), later), "still held");
    e.daemon.flush(later).await;
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());
}

/// A usage source that reads the stub's endpoint with a valid sign-in, and T3 Code's
/// snapshot at the path returned (not written yet).
fn usage_source_with_snapshot(e: &mut Env, url: String) -> std::path::PathBuf {
    use board::usage::{Credentials, UsageSource};

    let creds = e._dir.path().join("credentials.json");
    std::fs::write(
        &creds,
        json!({ "claudeAiOauth": {
            "accessToken": "tok-123",
            "expiresAt": (at() + chrono::Duration::hours(8)).timestamp_millis(),
        }})
        .to_string(),
    )
    .unwrap();
    let snapshot = e._dir.path().join("claudeAgent.json");
    e.daemon.usage_source = UsageSource::ClaudeCode {
        url,
        credentials: Credentials::File(creds),
        snapshots: vec![snapshot.clone()],
    };
    snapshot
}

/// Writes T3 Code's snapshot: the weekly window at `week` percent, read at `checked`.
fn write_snapshot(path: &std::path::Path, week: f64, resets: Timestamp, checked: Timestamp) {
    let file = json!({
        "driver": "claudeAgent",
        "auth": { "email": "user@example.com", "workspaceId": "ws-1" },
        "usageLimits": { "checkedAt": checked, "windows": [
            { "id": "five_hour", "kind": "session", "usedPercent": 20 },
            { "id": "seven_day", "kind": "weekly", "usedPercent": week, "resetsAt": resets },
        ]},
    });
    std::fs::write(path, file.to_string()).unwrap();
}

#[tokio::test]
async fn a_new_t3_snapshot_is_reported_and_stands_in_for_the_endpoint() {
    use board::daemon::{USAGE_CHECK_EVERY, USAGE_POLL_EVERY};

    let (mut e, _) = owner_env(AgentState::Idle, None);
    let url = format!("{}/api/oauth/usage", e.stub.url);
    let snapshot = usage_source_with_snapshot(&mut e, url);
    let minutes = chrono::Duration::minutes;
    let resets = at() + chrono::Duration::days(2);
    let reads = |e: &Env| e.stub.requests_to("GET /api/oauth/usage").len();
    let reports = |e: &Env| e.stub.requests_to("PUT /v1/usage");
    e.stub.on(
        "GET /api/oauth/usage",
        json!({ "seven_day": { "utilization": 50.0, "resets_at": resets } }),
    );

    // No file yet: the endpoint, as before.
    e.stub.on("PUT /v1/usage", usage_reading(50.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!((reads(&e), reports(&e).len()), (1, 1));

    // T3 Code takes a reading: it is reported at the next reconcile, with no endpoint call.
    let t = at() + minutes(1);
    write_snapshot(&snapshot, 51.0, resets, t);
    e.stub.on("PUT /v1/usage", usage_reading(51.0, t));
    e.daemon.reconcile(t).await.unwrap();
    assert_eq!(reads(&e), 1);
    assert_eq!(
        reports(&e)[1].body,
        json!({
            "five_hour": { "percent": 20.0 },
            "seven_day": { "percent": 51.0, "resets_at": resets },
            "read_at": t,
        })
    );
    assert!(
        !reports(&e)[1].body.to_string().contains("example.com"),
        "only the usage leaves the file"
    );

    // The same reading is reported once, and still stands in when the endpoint is due.
    e.daemon.reconcile(at() + USAGE_POLL_EVERY).await.unwrap();
    assert_eq!((reads(&e), reports(&e).len()), (1, 2));

    // T3 Code stopped: its reading is too old, so the endpoint is read again.
    let stopped = at() + minutes(8);
    e.stub.on("PUT /v1/usage", usage_reading(52.0, stopped));
    e.daemon.reconcile(stopped).await.unwrap();
    assert_eq!((reads(&e), reports(&e).len()), (2, 3));

    // While T3 Code's readings stand in, the endpoint is still read now and then.
    let back = stopped + minutes(5);
    write_snapshot(&snapshot, 53.0, resets, back);
    e.stub.on("PUT /v1/usage", usage_reading(53.0, back));
    e.daemon.reconcile(back).await.unwrap();
    assert_eq!((reads(&e), reports(&e).len()), (2, 4));
    let check = stopped + USAGE_CHECK_EVERY;
    write_snapshot(&snapshot, 53.0, resets, check);
    e.daemon.reconcile(check).await.unwrap();
    assert_eq!(reads(&e), 3);
}

#[tokio::test]
async fn the_newest_reading_of_t3_codes_claude_instances_is_reported() {
    use board::usage::UsageSource;

    let (mut e, _) = owner_env(AgentState::Idle, None);
    let url = format!("{}/api/oauth/usage", e.stub.url);
    let chats = usage_source_with_snapshot(&mut e, url);
    // The daemon's threads run on a second instance, which T3 Code keeps a file for too.
    let agents = e._dir.path().join("claude-agents.json");
    if let UsageSource::ClaudeCode { snapshots, .. } = &mut e.daemon.usage_source {
        snapshots.push(agents.clone());
    }
    let minutes = chrono::Duration::minutes;
    let resets = at() + chrono::Duration::days(2);
    let reads = |e: &Env| e.stub.requests_to("GET /api/oauth/usage").len();
    let reports = |e: &Env| e.stub.requests_to("PUT /v1/usage");
    e.stub.on(
        "GET /api/oauth/usage",
        json!({ "seven_day": { "utilization": 50.0, "resets_at": resets } }),
    );
    e.stub.on("PUT /v1/usage", usage_reading(50.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!((reads(&e), reports(&e).len()), (1, 1));

    // The agents' instance read last: its reading is reported, with no endpoint call.
    let t = at() + minutes(2);
    write_snapshot(&chats, 51.0, resets, at() + minutes(1));
    write_snapshot(&agents, 52.0, resets, t);
    e.stub.on("PUT /v1/usage", usage_reading(52.0, t));
    e.daemon.reconcile(t).await.unwrap();
    assert_eq!((reads(&e), reports(&e).len()), (1, 2));
    assert_eq!(reports(&e)[1].body["read_at"], json!(t));
    assert_eq!(reports(&e)[1].body["seven_day"]["percent"], json!(52.0));

    // Then the chats' instance reads newer: that reading is reported.
    let later = t + minutes(1);
    write_snapshot(&chats, 53.0, resets, later);
    e.stub.on("PUT /v1/usage", usage_reading(53.0, later));
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!((reads(&e), reports(&e).len()), (1, 3));
    assert_eq!(reports(&e)[2].body["read_at"], json!(later));
}

#[tokio::test]
async fn a_t3_snapshot_with_an_old_reading_under_a_new_date_is_not_taken() {
    use board::daemon::USAGE_POLL_EVERY;

    let (mut e, _) = owner_env(AgentState::Idle, None);
    let url = format!("{}/api/oauth/usage", e.stub.url);
    let snapshot = usage_source_with_snapshot(&mut e, url);
    let minutes = chrono::Duration::minutes;
    // The reset time `usage_reading` gives the reading the daemon knows.
    let resets = at() + chrono::Duration::days(2);
    let reads = |e: &Env| e.stub.requests_to("GET /api/oauth/usage").len();
    let reports = |e: &Env| e.stub.requests_to("PUT /v1/usage");
    e.stub.on(
        "GET /api/oauth/usage",
        json!({ "seven_day": { "utilization": 95.0, "resets_at": resets } }),
    );
    e.stub.on("PUT /v1/usage", usage_reading(95.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.daemon.usage_holds(&factory(), at()));

    // Lower in the same window: T3 Code dated an older reading anew. It is not reported,
    // and the endpoint is read when due, as without the file.
    write_snapshot(&snapshot, 80.0, resets, at() + minutes(1));
    e.daemon.reconcile(at() + minutes(1)).await.unwrap();
    assert_eq!((reads(&e), reports(&e).len()), (1, 1));
    assert!(e.daemon.usage_holds(&factory(), at() + minutes(1)));
    write_snapshot(&snapshot, 80.0, resets, at() + USAGE_POLL_EVERY);
    e.daemon.reconcile(at() + USAGE_POLL_EVERY).await.unwrap();
    assert_eq!((reads(&e), reports(&e).len()), (2, 2));

    // Lower in a later window: the limit was reset. The board is free within the minute.
    let reset = at() + USAGE_POLL_EVERY + minutes(1);
    write_snapshot(&snapshot, 3.0, resets + chrono::Duration::days(7), reset);
    e.stub.on("PUT /v1/usage", usage_reading(3.0, reset));
    e.daemon.reconcile(reset).await.unwrap();
    assert_eq!((reads(&e), reports(&e).len()), (2, 3));
    assert_eq!(reports(&e)[2].body["seven_day"]["percent"], 3.0);
    assert!(!e.daemon.usage_holds(&factory(), reset));
}

/// A usage endpoint that answers 429 with `Retry-After` (the stub sends no headers of
/// its own): its URL, the requests it got and the seconds it tells callers to wait.
async fn refusing_usage_endpoint() -> (
    String,
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<std::sync::atomic::AtomicU64>,
) {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api/oauth/usage", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let retry_after = Arc::new(AtomicU64::new(120));
    let (counted, wait) = (hits.clone(), retry_after.clone());
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            counted.fetch_add(1, Ordering::SeqCst);
            let answer = format!(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                wait.load(Ordering::SeqCst)
            );
            socket.write_all(answer.as_bytes()).await.unwrap();
        }
    });
    (url, hits, retry_after)
}

#[tokio::test]
async fn a_refused_usage_reading_is_tried_again_when_the_endpoint_says() {
    use board::daemon::{USAGE_POLL_NEAR, USAGE_RETRY_MAX};
    use std::sync::atomic::Ordering;

    let (url, hits, retry_after) = refusing_usage_endpoint().await;
    let (mut e, _) = owner_env(AgentState::Idle, None);
    let snapshot = usage_source_with_snapshot(&mut e, url);
    e.stub.on("GET /v1/usage", usage_reading(95.0, at()));
    let hits = || hits.load(Ordering::SeqCst);
    let minutes = chrono::Duration::minutes;

    // Refused for 2 minutes: read again then, not 5 minutes later.
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(hits(), 1);
    assert!(e.daemon.usage_holds(&factory(), at()));
    e.daemon.reconcile(at() + USAGE_POLL_NEAR).await.unwrap();
    assert_eq!(hits(), 1);
    retry_after.store(86_400, Ordering::SeqCst);
    let again = at() + minutes(2);
    e.daemon.reconcile(again).await.unwrap();
    assert_eq!(hits(), 2);

    // A longer wait than the daemon accepts: read again after the longest it does.
    e.daemon
        .reconcile(again + USAGE_RETRY_MAX - minutes(1))
        .await
        .unwrap();
    assert_eq!(hits(), 2);

    // While the endpoint refuses, a reset T3 Code read still frees the board.
    let reset = again + minutes(3);
    write_snapshot(&snapshot, 3.0, at() + chrono::Duration::days(9), reset);
    e.stub.on("PUT /v1/usage", usage_reading(3.0, reset));
    e.daemon.reconcile(reset).await.unwrap();
    assert_eq!(hits(), 2);
    assert_eq!(e.stub.requests_to("PUT /v1/usage").len(), 1);
    assert!(!e.daemon.usage_holds(&factory(), reset));

    e.daemon.reconcile(again + USAGE_RETRY_MAX).await.unwrap();
    assert_eq!(hits(), 3);
}

#[tokio::test]
async fn a_t3_snapshot_stands_in_after_a_refusal_and_is_reported_once_when_the_report_fails() {
    use board::daemon::USAGE_CHECK_EVERY;
    use std::sync::atomic::Ordering;

    let (url, hits, _) = refusing_usage_endpoint().await;
    let (mut e, _) = owner_env(AgentState::Idle, None);
    let snapshot = usage_source_with_snapshot(&mut e, url);
    let hits = || hits.load(Ordering::SeqCst);
    let reports = |e: &Env| e.stub.requests_to("PUT /v1/usage").len();
    let minutes = chrono::Duration::minutes;
    let resets = at() + chrono::Duration::days(2);

    // The first endpoint reading is refused for 2 minutes; T3 Code's is reported.
    write_snapshot(&snapshot, 50.0, resets, at());
    e.stub.on("PUT /v1/usage", usage_reading(50.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!((hits(), reports(&e)), (1, 1));

    // The 2 minutes passed, but T3 Code's reading stands in: no endpoint call.
    let t = at() + minutes(2);
    write_snapshot(&snapshot, 51.0, resets, t);
    e.stub.on("PUT /v1/usage", usage_reading(51.0, t));
    e.daemon.reconcile(t).await.unwrap();
    assert_eq!((hits(), reports(&e)), (1, 2));

    // A report that fails is not sent again each reconcile, though the server's older
    // reading comes back.
    let failed = at() + minutes(3);
    write_snapshot(&snapshot, 52.0, resets, failed);
    e.stub
        .on_status("PUT /v1/usage", 500, json!({}))
        .on("GET /v1/usage", usage_reading(51.0, t));
    e.daemon.reconcile(failed).await.unwrap();
    assert_eq!((hits(), reports(&e)), (1, 3));
    e.daemon
        .reconcile(failed + chrono::Duration::seconds(30))
        .await
        .unwrap();
    e.daemon.reconcile(failed + minutes(1)).await.unwrap();
    assert_eq!((hits(), reports(&e)), (1, 3));

    // The check of the endpoint still comes.
    let check = at() + USAGE_CHECK_EVERY;
    write_snapshot(&snapshot, 53.0, resets, check);
    e.daemon.reconcile(check).await.unwrap();
    assert_eq!(hits(), 2);
}

#[tokio::test]
async fn near_a_pace_line_the_endpoint_is_read_every_minute_despite_a_t3_snapshot() {
    use board::daemon::USAGE_POLL_NEAR;

    let (mut e, _) = owner_env(AgentState::Idle, None);
    let url = format!("{}/api/oauth/usage", e.stub.url);
    let snapshot = usage_source_with_snapshot(&mut e, url);
    let reads = |e: &Env| e.stub.requests_to("GET /api/oauth/usage").len();
    e.stub.on(
        "GET /api/oauth/usage",
        json!({ "seven_day": { "utilization": 70.0, "resets_at": null } }),
    );
    e.stub.on("PUT /v1/usage", paced_reading(78.5, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(reads(&e), 1);

    let t = at() + USAGE_POLL_NEAR;
    write_snapshot(&snapshot, 70.0, t + chrono::Duration::days(2), t);
    e.stub.on("PUT /v1/usage", paced_reading(78.5, t));
    e.daemon.reconcile(t).await.unwrap();
    assert_eq!(reads(&e), 2);
}

// Daemon dispatch: judges, merging and finishing.

const PR: &str = "https://github.com/syz51/agent-board/pull/12";
const JUDGE: &str = "t3:thread:project:p1:judge";
const WORKER: &str = "t3:thread:project:p1:worker";
const J1: &str = "thread:project:p1:j1";
const J2: &str = "thread:project:p1:j2";

/// A task its worker parked in `review`, `minutes` after [`at`].
fn review_task(id: &str, minutes: i64) -> board_proto::Task {
    let mut task = ready_task(id, board_proto::Priority::P1, -60);
    task.status = TaskStatus::Review;
    task.ready = false;
    task.owner = Some(WORKER.parse().unwrap());
    task.updated_at = at() + chrono::Duration::minutes(minutes);
    task
}

fn task_note(id: i64, task: &board_proto::Task, agent: &str, kind: &str, text: &str) -> Note {
    serde_json::from_value(json!({
        "id": id, "task": task.id, "agent": agent, "kind": kind, "text": text,
        "at": task.updated_at + chrono::Duration::minutes(id),
    }))
    .unwrap()
}

/// A judge's pass on `task`, with its check and `pr` line.
fn pass_note(task: &board_proto::Task, pr: &str) -> Note {
    let text = format!(
        "decision: review: pass. The limit is now 80 characters.\ncheck: cargo test --workspace => 0\npr: {pr}"
    );
    task_note(2, task, JUDGE, "decision", &text)
}

/// The dispatcher's note that names the review judge it started for `task`.
fn judge_note(task: &board_proto::Task) -> Note {
    let text = format!(
        "note: the dispatcher started a review judge for this task: thread j (agent {JUDGE})"
    );
    task_note(1, task, DISPATCH, "note", &text)
}

/// What `GET /v1/tasks/{id}` gives for `task`, with `notes`.
fn review_detail(e: &Env, task: &board_proto::Task, notes: Vec<Note>) {
    let mut d = detail(task.id.as_str(), false, &[]);
    d.task = task.clone();
    d.notes = notes;
    e.stub.on(&format!("GET /v1/tasks/{}", task.id), d);
}

fn judge(thread: &str, kind: &str, tasks: &[&str], running: bool) -> serde_json::Value {
    json!({
        "board": "factory", "agent": format!("t3:{thread}"), "kind": kind, "tasks": tasks,
        "registered_by": DISPATCH, "created_at": at(),
        "expires_at": at() + chrono::Duration::hours(2), "running": running,
    })
}

fn judges_at_work(e: &Env, judges: &[serde_json::Value]) {
    e.stub.on("GET /v1/boards/factory/judges", judges);
}

/// A daemon-dispatch board this device runs, with `tasks` on it and no judge at work.
fn judges_env(tasks: &[board_proto::Task]) -> Env {
    let e = dispatch_env(tasks);
    judges_at_work(&e, &[]);
    e.stub.on(
        "POST /v1/boards/factory/judges",
        judge(NEW_THREAD, "review", &[], true),
    );
    e
}

/// The judges the daemon registered, as it sent them.
fn registered(e: &Env) -> Vec<serde_json::Value> {
    e.stub
        .requests_to("POST /v1/boards/factory/judges")
        .into_iter()
        .map(|r| {
            assert_eq!(r.agent.as_deref(), Some(DISPATCH));
            r.body
        })
        .collect()
}

fn open_pr(merge_state: MergeState, checks: Checks) -> PullRequest {
    PullRequest {
        url: PR.to_owned(),
        state: PrState::Open,
        draft: false,
        base_ref: "main".to_owned(),
        head_ref: "feat/limit".to_owned(),
        head_oid: "9c1e44d0a7b2c3".to_owned(),
        cross_repository: false,
        merge_commit: None,
        merge_state,
        review_decision: ReviewDecision::None,
        checks,
        passed_checks: Vec::new(),
    }
}

/// A board with factory-4 in `review`, passed by a judge with `pr`, its worker `worker`.
fn passed_env(pr: &str, worker: AgentState) -> Env {
    let task = review_task("factory-4", 0);
    let e = judges_env(std::slice::from_ref(&task));
    review_detail(&e, &task, vec![judge_note(&task), pass_note(&task, pr)]);
    e.stub.on(
        &format!("GET {}", agent_path(WORKER)),
        agent_fixture(WORKER, DEVICE, worker),
    );
    let mut handed = task.clone();
    handed.owner = Some(DISPATCH.parse().unwrap());
    handed.version = 4;
    e.stub.on("PATCH /v1/tasks/factory-4", &handed);
    let mut done = handed.clone();
    done.status = TaskStatus::Done;
    e.stub.on("POST /v1/tasks/factory-4/done", &done);
    let mut open = task;
    open.status = TaskStatus::Open;
    open.owner = None;
    e.stub.on("POST /v1/tasks/factory-4/release", open);
    e
}

fn finishes(e: &Env) -> Vec<Recorded> {
    e.stub.requests_to("POST /v1/tasks/factory-4/done")
}

#[tokio::test]
async fn a_task_moved_to_review_gets_one_review_judge() {
    let task = review_task("factory-4", 0);
    let mut e = judges_env(std::slice::from_ref(&task));
    let parked = task_note(
        1,
        &task,
        WORKER,
        "note",
        &format!("note: parked in review, PR {PR}"),
    );
    review_detail(&e, &task, vec![parked]);
    e.t3.gh.put(open_pr(MergeState::Clean, Checks::Pending));
    e.daemon.reconcile(at()).await.unwrap();

    // One judge thread with the review brief, registered as a judge of the board.
    let id: board_proto::TaskId = "factory-4".parse().unwrap();
    assert_eq!(
        worker_starts(&e.t3),
        [(
            "review factory-4 Task factory-4".to_owned(),
            board::brief::judge_brief(
                board::brief::JudgeKind::Review,
                &factory(),
                std::slice::from_ref(&id)
            )
        )]
    );
    // Its worktree starts on the head of the task's pull request.
    assert_eq!(e.t3.gh.calls(), [format!("view {PR}")]);
    assert_eq!(
        registered(&e),
        [json!({
            "agent": format!("t3:{NEW_THREAD}"), "kind": "review", "tasks": ["factory-4"],
        })]
    );
    let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
    assert_eq!(notes.len(), 1);
    let text = notes[0].body["text"].as_str().unwrap();
    assert!(
        text.contains("review judge")
            && text.contains(&format!("https://t3.example.test/threads/{NEW_THREAD}")),
        "{text}"
    );
    let run = &e.daemon.state.judges[0];
    assert!(run.registered && run.noted);
    // Recorded in watch.json, so a restarted daemon follows the same judge.
    assert_eq!(saved_state(&e).judges, e.daemon.state.judges);

    // While it runs, the task gets no second judge.
    judges_at_work(&e, &[judge(NEW_THREAD, "review", &["factory-4"], true)]);
    let minute = chrono::Duration::minutes(1);
    e.daemon.reconcile(at() + minute).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);

    // It ended without a verdict: the task waits its retry.
    judges_at_work(&e, &[judge(NEW_THREAD, "review", &["factory-4"], false)]);
    e.daemon.reconcile(at() + minute * 2).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);
    assert!(e.daemon.state.judges.is_empty());
    let retry = at() + minute * 2 + board::daemon::JUDGE_RETRY;
    e.daemon.reconcile(retry - minute).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1, "before the retry");

    // A task parked again since is reviewed at once.
    let mut again = task.clone();
    again.updated_at = at() + minute * 3;
    e.stub.on("GET /v1/tasks", [&again]);
    review_detail(&e, &again, vec![]);
    e.t3.ids.lock().unwrap().push_back(J1.to_owned());
    e.daemon.reconcile(at() + minute * 3).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 2);
    assert_eq!(registered(&e)[1]["agent"], format!("t3:{J1}"));
}

#[tokio::test]
async fn a_judge_that_ended_without_its_verdict_is_started_again_after_the_retry() {
    let task = review_task("factory-4", 0);
    let mut e = judges_env(std::slice::from_ref(&task));
    review_detail(&e, &task, vec![]);
    e.daemon.reconcile(at()).await.unwrap();
    judges_at_work(&e, &[]);
    let minute = chrono::Duration::minutes(1);
    e.daemon.reconcile(at() + minute).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);
    let retry = at() + minute + board::daemon::JUDGE_RETRY;
    e.daemon.reconcile(retry).await.unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 2);
}

#[tokio::test]
async fn at_most_two_judges_run_at_once_and_the_longest_wait_goes_first() {
    let tasks = [
        review_task("factory-4", 2),
        review_task("factory-5", 0),
        review_task("factory-6", 1),
    ];
    let mut e = judges_env(&tasks);
    for task in &tasks {
        review_detail(&e, task, vec![]);
    }
    e.t3.ids
        .lock()
        .unwrap()
        .extend([J1.to_owned(), J2.to_owned()]);
    let minute = chrono::Duration::minutes(1);
    e.daemon.reconcile(at() + minute * 5).await.unwrap();
    assert_eq!(
        titles(&e.t3),
        [
            "review factory-5 Task factory-5",
            "review factory-6 Task factory-6"
        ]
    );

    // Both run: the third task waits.
    judges_at_work(
        &e,
        &[
            judge(J1, "review", &["factory-5"], true),
            judge(J2, "review", &["factory-6"], true),
        ],
    );
    e.daemon.reconcile(at() + minute * 6).await.unwrap();
    assert_eq!(titles(&e.t3).len(), 2);

    // One ends: the third gets its judge, and the ended one's task waits its retry.
    judges_at_work(
        &e,
        &[
            judge(J1, "review", &["factory-5"], false),
            judge(J2, "review", &["factory-6"], true),
        ],
    );
    e.daemon.reconcile(at() + minute * 7).await.unwrap();
    let titles = titles(&e.t3);
    assert_eq!(titles.len(), 3);
    assert_eq!(titles[2], "review factory-4 Task factory-4");
}

fn proposal(id: &str, parent: Option<&str>, minutes: i64) -> board_proto::Task {
    let mut task = ready_task(id, board_proto::Priority::P2, minutes);
    task.auto = false;
    task.ready = false;
    task.proposal = true;
    task.body = "Proposal: the list is slow. Evidence: 3 s. Change: an index. Size S.".into();
    task.parent = parent.map(|p| p.parse().unwrap());
    task
}

#[tokio::test]
async fn proposals_get_one_verdict_judge_per_parent_with_at_most_five_each() {
    let mut tasks: Vec<board_proto::Task> = (0..7)
        .map(|n| proposal(&format!("factory-1{n}"), Some("factory-1"), n))
        .collect();
    tasks.push(proposal("factory-20", Some("factory-2"), -1));
    tasks.push(proposal("factory-30", None, 10));
    let mut e = judges_env(&tasks);
    e.t3.ids
        .lock()
        .unwrap()
        .extend([J1.to_owned(), J2.to_owned()]);
    e.daemon.reconcile(at()).await.unwrap();

    // The oldest proposal's group first; five of the seven under factory-1.
    let batch = [
        "factory-10",
        "factory-11",
        "factory-12",
        "factory-13",
        "factory-14",
    ];
    assert_eq!(
        registered(&e),
        [
            json!({"agent": format!("t3:{J1}"), "kind": "verdict", "tasks": ["factory-20"]}),
            json!({"agent": format!("t3:{J2}"), "kind": "verdict", "tasks": batch}),
        ]
    );
    let ids: Vec<board_proto::TaskId> = batch.iter().map(|t| t.parse().unwrap()).collect();
    let starts = worker_starts(&e.t3);
    assert_eq!(starts[0].0, "verdict factory-20 (under factory-2)");
    assert_eq!(
        starts[1],
        (
            "verdict factory-10 (under factory-1)".to_owned(),
            board::brief::judge_brief(board::brief::JudgeKind::Verdict, &factory(), &ids)
        )
    );

    // While the judge of factory-1's proposals runs, the other two wait for it; the
    // proposals without a parent take the slot the other judge left.
    judges_at_work(&e, &[judge(J2, "verdict", &batch, true)]);
    e.stub.on("GET /v1/tasks", &tasks[..]);
    let minute = chrono::Duration::minutes(1);
    e.daemon.reconcile(at() + minute).await.unwrap();
    let judged = registered(&e);
    assert_eq!(judged.len(), 3);
    assert_eq!(judged[2]["tasks"], json!(["factory-30"]));

    // It gave its verdicts and ended: the rest of factory-1's proposals get a judge.
    judges_at_work(&e, &[]);
    e.stub.on("GET /v1/tasks", &tasks[5..7]);
    e.daemon.reconcile(at() + minute * 2).await.unwrap();
    let judged = registered(&e);
    assert_eq!(judged.len(), 4);
    assert_eq!(judged[3]["tasks"], json!(["factory-15", "factory-16"]));
}

/// Whether the stub was answering a request whose `METHOD path?query` contains `a` when
/// one containing `b` arrived, or the other way round: the daemon sent them at once.
fn at_once(stub: &Stub, a: &str, b: &str) -> bool {
    stub.requests().iter().any(|r| {
        let this = format!("{} {}?{}", r.method, r.path, r.query);
        let beside = |other: &str| r.beside.iter().any(|o| o.contains(other));
        (this.contains(a) && beside(b)) || (this.contains(b) && beside(a))
    })
}

#[tokio::test]
async fn the_proposals_and_the_ready_tasks_are_listed_at_once() {
    let tasks = [proposal("factory-10", Some("factory-1"), 0)];
    let mut e = judges_env(&tasks);
    e.stub.delay("GET /v1/tasks", Duration::from_millis(100));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        at_once(&e.stub, "proposal=true", "ready=true&proposal=false"),
        "{:#?}",
        e.stub.requests_to("GET /v1/tasks")
    );
    // The judge the proposals call for is started all the same.
    assert_eq!(
        registered(&e),
        [json!({"agent": format!("t3:{NEW_THREAD}"), "kind": "verdict", "tasks": ["factory-10"]})]
    );
}

#[tokio::test]
async fn the_tasks_of_an_ended_judge_are_read_at_once_and_each_keeps_its_own_date() {
    let batch = ["factory-10", "factory-11", "factory-12"];
    let tasks: Vec<board_proto::Task> = (0..3)
        .map(|n| proposal(batch[n], Some("factory-1"), n as i64))
        .collect();
    let mut e = judges_env(&tasks);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(registered(&e).len(), 1);

    // The judge ended. Each task changed at its own time; the first is the slowest to read.
    let changed = |n: usize| at() + chrono::Duration::seconds(n as i64 + 1);
    for (n, id) in batch.iter().enumerate() {
        let mut read = detail(id, false, &[]);
        read.task.updated_at = changed(n);
        let route = format!("GET /v1/tasks/{id}");
        e.stub.on(&route, read);
        e.stub
            .delay(&route, Duration::from_millis(if n == 0 { 200 } else { 50 }));
    }
    e.stub.clear();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    for other in &batch[1..] {
        assert!(
            at_once(&e.stub, "tasks/factory-10?", &format!("tasks/{other}?")),
            "{other}: {:#?}",
            e.stub.requests()
        );
    }
    for (n, id) in batch.iter().enumerate() {
        let retry = &e.daemon.state.judge_retry[&format!("verdict:{id}")];
        assert_eq!(retry.updated_at, Some(changed(n)), "{id}");
    }
}

/// The tasks that waited on a finished one are read side by side, and the wake-ups keep the
/// order the finished task lists them in, however long each read takes.
#[tokio::test]
async fn the_tasks_a_done_task_blocked_are_read_at_once_and_woken_in_order() {
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    let owner: board_proto::AgentId = "t3:worker".parse().unwrap();
    let waiting = ["factory-12", "factory-13", "factory-14"];
    let mut done = detail("other-3", false, &waiting);
    done.task.status = TaskStatus::Done;
    e.stub.on("GET /v1/tasks/other-3", &done);
    for (n, id) in waiting.iter().enumerate() {
        let mut blocked = detail(id, false, &[]);
        blocked.task.status = TaskStatus::Blocked;
        blocked.task.owner = Some(owner.clone());
        blocked.task.blockers = vec!["other-3".parse().unwrap()];
        let route = format!("GET /v1/tasks/{id}");
        e.stub.on(&route, &blocked);
        // The first is the slowest to read.
        e.stub
            .delay(&route, Duration::from_millis(if n == 0 { 200 } else { 50 }));
    }
    e.stub.clear();

    let mut ev = event(20, EventType::TaskDone, Some("other-3"), "t3:elsewhere");
    ev.board = Some("other".parse().unwrap());
    e.daemon.on_event(&ev, at()).await.unwrap();
    for other in &waiting[1..] {
        assert!(
            at_once(&e.stub, "tasks/factory-12?", &format!("tasks/{other}?")),
            "{other}: {:#?}",
            e.stub.requests()
        );
    }
    let woken: Vec<String> = e.daemon.state.wakes[&owner]
        .wakes
        .iter()
        .map(|wake| match &wake.about {
            board::daemon::WakeAbout::Unblocked { task, .. } => task.to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(woken, waiting);
    // The finished task is read once, and each blocker of a waiting task once.
    assert_eq!(e.stub.requests_to("GET /v1/tasks/other-3").len(), 4);
}

#[tokio::test]
async fn a_goal_review_starts_once_a_task_was_finished_and_the_board_rests() {
    let goal_board = |coordinator| {
        let mut board = daemon_board(coordinator);
        board.goal = "Lists answer in under a second.".into();
        board
    };
    let mut e = judges_env(&[]);
    e.stub.on("GET /v1/boards", [goal_board(None)]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        goal_board(Some(DISPATCH)),
    );
    // Nothing was finished since the daemon started: no goal review.
    e.daemon.reconcile(at()).await.unwrap();
    assert!(worker_starts(&e.t3).is_empty());

    let mut done = detail("factory-3", false, &[]);
    done.task.status = TaskStatus::Done;
    e.stub.on("GET /v1/tasks/factory-3", done);
    e.daemon
        .on_event(
            &event(40, EventType::TaskDone, Some("factory-3"), WORKER),
            at(),
        )
        .await
        .unwrap();
    let minute = chrono::Duration::minutes(1);
    e.daemon.reconcile(at() + minute).await.unwrap();
    let done_id: board_proto::TaskId = "factory-3".parse().unwrap();
    assert_eq!(
        worker_starts(&e.t3),
        [(
            "goal review factory".to_owned(),
            board::brief::judge_brief(
                board::brief::JudgeKind::Goal,
                &factory(),
                std::slice::from_ref(&done_id),
            )
        )]
    );
    assert_eq!(
        registered(&e),
        [json!({"agent": format!("t3:{NEW_THREAD}"), "kind": "goal", "tasks": ["factory-3"]})]
    );

    // One review per finished task: the next waits for another task to finish.
    judges_at_work(&e, &[judge(NEW_THREAD, "goal", &["factory-3"], false)]);
    e.daemon
        .reconcile(at() + minute + board::daemon::JUDGE_RETRY)
        .await
        .unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);

    // A board with work left gets a worker, and no goal review.
    let mut e = judges_env(&[ready_task("factory-4", board_proto::Priority::P1, 0)]);
    e.stub.on("GET /v1/boards", [goal_board(None)]);
    e.daemon
        .state
        .goal_due
        .insert(factory(), Some(done_id.clone()));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);
    assert_eq!(
        e.daemon.state.goal_due.get(&factory()),
        Some(&Some(done_id)),
        "still due with the finishing task preserved"
    );
}

#[tokio::test]
async fn a_goal_review_with_last_done_task_backs_off_under_goal_board_key() {
    let goal_board = |coordinator| {
        let mut board = daemon_board(coordinator);
        board.goal = "Lists answer in under a second.".into();
        board
    };
    let mut e = judges_env(&[]);
    e.stub.on("GET /v1/boards", [goal_board(None)]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        goal_board(Some(DISPATCH)),
    );
    let done_id: board_proto::TaskId = "factory-3".parse().unwrap();
    e.daemon
        .state
        .goal_due
        .insert(factory(), Some(done_id.clone()));

    // First start fails before spawning a thread: the JudgeRun stays pending with
    // `tasks = ["factory-3"]` and `goal_due` still holds `factory-3`.
    e.t3.fail_start.store(true, Ordering::SeqCst);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(registered(&e).is_empty());
    assert_eq!(e.daemon.state.judges.len(), 1);
    assert_eq!(
        e.daemon.state.judges[0].tasks,
        std::slice::from_ref(&done_id)
    );

    // On reconcile, `settle_judge_starts` finds no thread and calls `end_run` with
    // `JUDGE_START_RETRY`. Even though `run.tasks` carries `factory-3`, `end_run` must
    // record backoff under `goal:factory` (not `goal:factory-3`), must not fetch
    // `GET /v1/tasks/factory-3`, and must hold off respawning until `JUDGE_START_RETRY`.
    e.t3.fail_start.store(false, Ordering::SeqCst);
    let minute = chrono::Duration::minutes(1);
    e.daemon.reconcile(at() + minute).await.unwrap();
    assert!(e.daemon.state.judges.is_empty());
    assert!(registered(&e).is_empty(), "must back off after start fail");
    assert!(
        e.daemon.state.judge_retry.contains_key("goal:factory"),
        "expected board-level goal retry key: {:?}",
        e.daemon.state.judge_retry
    );
    assert!(
        !e.daemon.state.judge_retry.contains_key("goal:factory-3"),
        "must not write unread per-task goal retry key: {:?}",
        e.daemon.state.judge_retry
    );
    assert_eq!(
        e.stub.requests_to("GET /v1/tasks/factory-3").len(),
        0,
        "end_run must not fetch the last finished task for a goal review"
    );

    // Still inside the backoff window: no goal review starts.
    e.daemon
        .reconcile(at() + minute + board::daemon::JUDGE_START_RETRY - chrono::Duration::seconds(1))
        .await
        .unwrap();
    assert!(registered(&e).is_empty());
    assert_eq!(
        worker_starts(&e.t3).len(),
        1,
        "only the initial failed attempt should exist during backoff"
    );

    // Once `JUDGE_START_RETRY` passes, the goal review starts with `Last finished task: factory-3`.
    e.daemon
        .reconcile(at() + minute + board::daemon::JUDGE_START_RETRY)
        .await
        .unwrap();
    assert_eq!(
        registered(&e),
        [json!({"agent": format!("t3:{NEW_THREAD}"), "kind": "goal", "tasks": ["factory-3"]})]
    );
    let expected_start = (
        "goal review factory".to_owned(),
        board::brief::judge_brief(
            board::brief::JudgeKind::Goal,
            &factory(),
            std::slice::from_ref(&done_id),
        ),
    );
    assert_eq!(
        worker_starts(&e.t3),
        [expected_start.clone(), expected_start]
    );
}

#[tokio::test]
async fn a_ready_item_on_the_feedback_board_gets_a_triage_judge_and_no_worker() {
    let mut item = feedback_fixture("factory-9", None);
    item.board = factory();
    let mut e = judges_env(std::slice::from_ref(&item));
    let home = Home::at(e._dir.path().join("home"));
    std::fs::create_dir_all(home.config_path().parent().unwrap()).unwrap();
    let config = format!(
        "feedback_board = \"factory\"\n[projects]\n\"{REPO}\" = '{}'\n",
        src("agent-board").display()
    );
    std::fs::write(home.config_path(), config).unwrap();
    e.daemon.reconcile(at()).await.unwrap();
    let id: board_proto::TaskId = "factory-9".parse().unwrap();
    assert_eq!(
        worker_starts(&e.t3),
        [(
            "triage factory-9".to_owned(),
            board::brief::judge_brief(
                board::brief::JudgeKind::Triage,
                &factory(),
                std::slice::from_ref(&id)
            )
        )]
    );
    assert_eq!(
        registered(&e),
        [json!({"agent": format!("t3:{NEW_THREAD}"), "kind": "triage", "tasks": ["factory-9"]})]
    );
}

#[tokio::test]
async fn a_passed_task_is_merged_once_its_checks_pass_and_finished_with_the_judges_summary() {
    let mut e = passed_env(PR, AgentState::Idle);
    e.t3.gh.put(open_pr(MergeState::Clean, Checks::Pending));
    let every = board::daemon::PR_READ_EVERY;

    // Its checks still run: read, not merged.
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(e.t3.gh.calls(), [format!("view {PR}")]);

    // The pull request is read at most every two minutes.
    e.daemon
        .reconcile(at() + every - chrono::Duration::seconds(1))
        .await
        .unwrap();
    assert_eq!(e.t3.gh.ran("view"), 1);

    // Behind its base: its branch is updated, and it waits for the new checks.
    e.t3.gh.put(open_pr(MergeState::Behind, Checks::Passed));
    e.daemon.reconcile(at() + every).await.unwrap();
    assert_eq!(e.t3.gh.ran("view"), 2);
    assert_eq!(e.t3.gh.ran("update-branch"), 1);
    e.daemon.reconcile(at() + every * 2).await.unwrap();
    assert_eq!(e.t3.gh.ran("view"), 3);
    assert_eq!(e.t3.gh.ran("update-branch"), 1);
    assert_eq!(e.t3.gh.ran("merge"), 0);
    assert!(finishes(&e).is_empty());

    // Every check passed on an up-to-date branch: merged, and the task finished by the
    // dispatcher with the pull request, the merge commit and the judge's checks.
    e.t3.gh.put(open_pr(MergeState::Clean, Checks::Passed));
    e.daemon.reconcile(at() + every * 3).await.unwrap();
    assert_eq!(e.t3.gh.ran("merge"), 1);
    let handed = e.stub.requests_to("PATCH /v1/tasks/factory-4");
    assert_eq!(handed.len(), 1);
    assert_eq!(handed[0].body["owner"], DISPATCH);
    assert_eq!(handed[0].body["version"], 3);
    let done = finishes(&e);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].agent.as_deref(), Some(DISPATCH));
    let body = &done[0].body;
    assert_eq!(body["version"], 4);
    assert_eq!(body["summary"], "The limit is now 80 characters.");
    let evidence: Vec<(&str, &str, Option<i64>)> = body["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["kind"].as_str().unwrap(),
                e["value"].as_str().unwrap(),
                e["exit_code"].as_i64(),
            )
        })
        .collect();
    assert_eq!(
        evidence,
        [
            ("pr", PR, None),
            ("commit", MERGE_COMMIT, None),
            ("check", "cargo test --workspace", Some(0)),
        ]
    );
    assert!(e.daemon.state.merges.is_empty());
    // A passed task gets no second review, and no verdict was changed.
    assert!(worker_starts(&e.t3).is_empty());
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-4/notes")
            .is_empty()
    );
}

#[tokio::test]
async fn a_passed_draft_pr_is_marked_ready_then_merged_and_finished() {
    let mut e = passed_env(PR, AgentState::Idle);
    let mut draft = open_pr(MergeState::Waits, Checks::Passed);
    draft.draft = true;
    e.t3.gh.put(draft);
    let every = board::daemon::PR_READ_EVERY;

    // First reconcile sees the draft PR, marks it ready, and waits for GitHub to update
    // its merge state and any ready_for_review checks before merging.
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        e.t3.gh.calls(),
        [format!("view {PR}"), format!("ready {PR}")]
    );
    assert_eq!(e.t3.gh.ran("merge"), 0);
    assert!(finishes(&e).is_empty());

    // Next reconcile sees the ready, clean, passing PR -> merges and finishes the task.
    e.daemon.reconcile(at() + every).await.unwrap();
    assert_eq!(e.t3.gh.ran("ready"), 1);
    assert_eq!(e.t3.gh.ran("merge"), 1);
    let done = finishes(&e);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].agent.as_deref(), Some(DISPATCH));
}

#[tokio::test]
async fn a_passed_conflicting_draft_pr_is_reopened_without_marking_ready() {
    let mut e = passed_env(PR, AgentState::Idle);
    let mut draft = open_pr(MergeState::Conflict, Checks::Passed);
    draft.draft = true;
    e.t3.gh.put(draft);

    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(e.t3.gh.ran("ready"), 0);
    assert_eq!(e.t3.gh.ran("merge"), 0);
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-4/release").len(),
        1
    );
    assert!(finishes(&e).is_empty());
}

#[tokio::test]
async fn a_review_task_whose_pr_merged_without_a_verdict_is_reviewed_on_base_and_finished() {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "--quiet", "--initial-branch=trunk"]);
    git(
        &origin,
        &["commit", "--quiet", "--allow-empty", "-m", "first"],
    );
    git(&origin, &["branch", "fork"]);
    git(&origin, &["branch", "feat/merged"]);
    git(dir.path(), &["clone", "--quiet", "origin", "repo"]);
    let repo = dir.path().join("repo");

    let task = review_task("factory-4", 0);
    let mut e = env_with_projects([(REPO.to_owned(), repo)].into());
    e.stub.on("GET /v1/boards", [daemon_board(None)]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        daemon_board(Some(DISPATCH)),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    e.stub.on("GET /v1/tasks", std::slice::from_ref(&task));
    e.stub.on("POST /v1/tasks/factory-4/notes", note(&task.id));
    judges_at_work(&e, &[]);
    e.stub.on(
        "POST /v1/boards/factory/judges",
        judge(NEW_THREAD, "review", &["factory-4"], true),
    );
    let parked = task_note(
        1,
        &task,
        WORKER,
        "finding",
        &format!("PR {PR} opened; squash-merged as {MERGE_COMMIT} at user request"),
    );
    review_detail(&e, &task, vec![parked.clone()]);
    e.stub.on(
        &format!("GET {}", agent_path(WORKER)),
        agent_fixture(WORKER, DEVICE, AgentState::Ended),
    );
    let mut handed = task.clone();
    handed.owner = Some(DISPATCH.parse().unwrap());
    handed.version = 4;
    e.stub.on("PATCH /v1/tasks/factory-4", &handed);
    let mut done = handed.clone();
    done.status = TaskStatus::Done;
    e.stub.on("POST /v1/tasks/factory-4/done", &done);

    // Even when the merged PR came from a fork and its head branch still exists on origin,
    // the review judge starts on the PR's base branch (`origin/fork`), which holds the merge.
    let mut merged = open_pr(MergeState::Waits, Checks::Passed);
    merged.state = PrState::Merged;
    merged.base_ref = "fork".to_owned();
    merged.head_ref = "feat/merged".to_owned();
    merged.cross_repository = true;
    merged.merge_commit = Some(MERGE_COMMIT.to_owned());
    e.t3.gh.put(merged);

    // Reconcile 1: no pass verdict yet -> daemon routes it to a review judge on `origin/fork`.
    e.daemon.reconcile(at()).await.unwrap();
    let starts: Vec<Option<NewWorktree>> =
        e.t3.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Start { worktree, .. } => Some(worktree),
                _ => None,
            })
            .collect();
    assert_eq!(
        starts,
        [Some(NewWorktree {
            base: "origin/fork".into(),
            branch: "review/factory-4".into(),
        })]
    );
    assert_eq!(registered(&e).len(), 1);
    assert!(finishes(&e).is_empty());

    // Reconcile 2: the review judge passes the already-merged PR and ends -> daemon closes
    // the task directly with the PR, merge commit, checks and summary, with no merge call.
    judges_at_work(&e, &[judge(NEW_THREAD, "review", &["factory-4"], false)]);
    let started = task_note(
        2,
        &task,
        DISPATCH,
        "note",
        &format!(
            "note: the dispatcher started a review judge for this task: thread j (agent t3:{NEW_THREAD})"
        ),
    );
    let mut pass = pass_note(&task, PR);
    pass.id = 3;
    pass.agent = format!("t3:{NEW_THREAD}").parse().unwrap();
    review_detail(&e, &task, vec![parked, started, pass]);

    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(e.t3.gh.ran("merge"), 0);
    assert_eq!(e.t3.gh.ran("update-branch"), 0);
    let done = finishes(&e);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].agent.as_deref(), Some(DISPATCH));
    assert_eq!(done[0].body["summary"], "The limit is now 80 characters.");
    let evidence: Vec<(&str, &str, Option<i64>)> = done[0].body["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|ev| {
            (
                ev["kind"].as_str().unwrap(),
                ev["value"].as_str().unwrap(),
                ev["exit_code"].as_i64(),
            )
        })
        .collect();
    assert_eq!(
        evidence,
        [
            ("pr", PR, None),
            ("commit", MERGE_COMMIT, None),
            ("check", "cargo test --workspace", Some(0)),
        ]
    );
}

#[tokio::test]
async fn a_passed_task_whose_pull_request_conflicts_is_set_open_with_a_note() {
    for behind in [false, true] {
        let mut e = passed_env(PR, AgentState::Idle);
        if behind {
            // `gh pr update-branch` finds the conflict.
            e.t3.gh.put(open_pr(MergeState::Behind, Checks::Passed));
            e.t3.gh.conflicts.store(true, Ordering::SeqCst);
        } else {
            e.t3.gh.put(open_pr(MergeState::Conflict, Checks::Passed));
        }
        e.daemon.reconcile(at()).await.unwrap();
        let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].body["kind"], "decision");
        let text = notes[0].body["text"].as_str().unwrap();
        assert!(
            text.starts_with("decision: review: void") && text.contains(PR),
            "{text}"
        );
        let opened = e.stub.requests_to("POST /v1/tasks/factory-4/release");
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].body, json!({ "agent": DISPATCH }));
        assert_eq!(e.t3.gh.ran("merge"), 0);
        assert!(finishes(&e).is_empty());
    }
}

#[tokio::test]
async fn failed_checks_are_noted_once_and_nothing_is_merged() {
    let mut e = passed_env(PR, AgentState::Idle);
    e.t3.gh.put(open_pr(
        MergeState::Clean,
        Checks::Failed(vec!["rust".into()]),
    ));
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon
        .reconcile(at() + board::daemon::PR_READ_EVERY)
        .await
        .unwrap();
    assert_eq!(e.t3.gh.ran("view"), 2);
    let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
    assert_eq!(notes.len(), 1, "once per head commit");
    assert_eq!(notes[0].body["kind"], "finding");
    assert!(notes[0].body["text"].as_str().unwrap().contains("rust"));
    assert_eq!(e.t3.gh.ran("merge"), 0);
    assert!(finishes(&e).is_empty());
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-4/release")
            .is_empty()
    );
}

#[tokio::test]
async fn failed_checks_holding_past_limit_void_pass_and_reopen_with_check_names() {
    let mut e = passed_env(PR, AgentState::Idle);
    e.t3.gh.put(open_pr(
        MergeState::Clean,
        Checks::Failed(vec!["rust".into(), "web".into()]),
    ));
    let hold = board::daemon::PR_HOLD_WAIT;
    let every = board::daemon::PR_READ_EVERY;

    // Initial read notes the failure once and starts the hold timer.
    e.daemon.reconcile(at()).await.unwrap();
    // Just before the hold expires: still waiting, no second note and no reopen.
    e.daemon.reconcile(at() + hold - every).await.unwrap();
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-4/notes").len(),
        1
    );
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-4/release")
            .is_empty()
    );

    // At the hold limit on the same head commit: void the pass with failed check names and reopen.
    e.daemon.reconcile(at() + hold).await.unwrap();
    let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[1].body["kind"], "decision");
    let text = notes[1].body["text"].as_str().unwrap();
    assert!(
        text.starts_with("decision: review: void")
            && text.contains("rust, web")
            && text.contains(PR)
            && text.contains("feat/limit"),
        "{text}"
    );
    let opened = e.stub.requests_to("POST /v1/tasks/factory-4/release");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].body, json!({ "agent": DISPATCH }));
    assert_eq!(e.t3.gh.ran("merge"), 0);
    assert!(e.daemon.state.merges.is_empty());
}

#[tokio::test]
async fn failed_checks_hold_resets_on_new_head_or_rerun() {
    let mut e = passed_env(PR, AgentState::Idle);
    let hold = board::daemon::PR_HOLD_WAIT;
    let t0 = at();
    let mut pr = open_pr(MergeState::Clean, Checks::Failed(vec!["rust".into()]));
    e.t3.gh.put(pr.clone());

    e.daemon.reconcile(t0).await.unwrap();
    // New head pushed after 20 minutes resets the hold timer and notes the new commit.
    pr.head_oid = "newhead123456".to_owned();
    e.t3.gh.put(pr.clone());
    e.daemon
        .reconcile(t0 + chrono::Duration::minutes(20))
        .await
        .unwrap();
    // 30 minutes after t0 is only 10 minutes on the new head -> must NOT reopen yet.
    e.daemon.reconcile(t0 + hold).await.unwrap();
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-4/release")
            .is_empty()
    );

    // A rerun goes Pending at t0+40m (clearing the hold), then fails again at t0+42m.
    pr.checks = Checks::Pending;
    e.t3.gh.put(pr.clone());
    e.daemon
        .reconcile(t0 + chrono::Duration::minutes(40))
        .await
        .unwrap();
    pr.checks = Checks::Failed(vec!["rust".into()]);
    e.t3.gh.put(pr);
    let t_fail2 = t0 + chrono::Duration::minutes(42);
    e.daemon.reconcile(t_fail2).await.unwrap();
    // 50 minutes after t0 (30m after new head, but only 8m after rerun failure) -> still waits.
    e.daemon
        .reconcile(t0 + chrono::Duration::minutes(50))
        .await
        .unwrap();
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-4/release")
            .is_empty()
    );

    // 30 minutes after the rerun failure -> reopens!
    e.daemon.reconcile(t_fail2 + hold).await.unwrap();
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-4/release").len(),
        1
    );
}

#[tokio::test]
async fn a_draft_pr_that_stays_draft_past_limit_is_reopened() {
    let mut e = passed_env(PR, AgentState::Idle);
    let mut draft = open_pr(MergeState::Waits, Checks::Passed);
    draft.draft = true;
    e.t3.gh.put(draft);
    e.t3.gh.ready_fails.store(true, Ordering::SeqCst);
    let hold = board::daemon::PR_HOLD_WAIT;

    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(e.t3.gh.ran("ready"), 1);
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-4/release")
            .is_empty()
    );

    // After PR_HOLD_WAIT on the same head commit, voids pass and reopens without retrying ready.
    e.daemon.reconcile(at() + hold).await.unwrap();
    assert_eq!(e.t3.gh.ran("ready"), 1);
    let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
    assert_eq!(notes.len(), 1);
    let text = notes[0].body["text"].as_str().unwrap();
    assert!(
        text.starts_with("decision: review: void") && text.contains("draft") && text.contains(PR),
        "{text}"
    );
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-4/release").len(),
        1
    );
}

#[tokio::test]
async fn merge_waits_reopens_after_limit_and_wakes_user_when_human_review_is_required() {
    let hold = board::daemon::PR_HOLD_WAIT;

    // 1. Generic MergeState::Waits (no required human review) reopens after PR_HOLD_WAIT.
    {
        let mut e = passed_env(PR, AgentState::Idle);
        e.t3.gh.put(open_pr(MergeState::Waits, Checks::Passed));
        e.daemon.reconcile(at()).await.unwrap();
        assert!(
            e.stub
                .requests_to("POST /v1/tasks/factory-4/release")
                .is_empty()
        );
        e.daemon.reconcile(at() + hold).await.unwrap();
        let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].body["text"]
                .as_str()
                .unwrap()
                .starts_with("decision: review: void")
        );
        assert_eq!(
            e.stub.requests_to("POST /v1/tasks/factory-4/release").len(),
            1
        );
    }

    // 2. ChangesRequested reopens after PR_HOLD_WAIT naming changes requested.
    {
        let mut e = passed_env(PR, AgentState::Idle);
        let mut pr = open_pr(MergeState::Waits, Checks::Passed);
        pr.review_decision = ReviewDecision::ChangesRequested;
        e.t3.gh.put(pr);
        e.daemon.reconcile(at()).await.unwrap();
        e.daemon.reconcile(at() + hold).await.unwrap();
        let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
        let text = notes[0].body["text"].as_str().unwrap();
        assert!(
            text.starts_with("decision: review: void") && text.contains("changes were requested"),
            "{text}"
        );
        assert_eq!(
            e.stub.requests_to("POST /v1/tasks/factory-4/release").len(),
            1
        );
    }

    // 3. ReviewRequired creates an Action ask and moves the task to blocked instead of reopening.
    {
        let mut e = passed_env(PR, AgentState::Idle);
        let mut pr = open_pr(MergeState::Waits, Checks::Passed);
        pr.review_decision = ReviewDecision::ReviewRequired;
        e.t3.gh.put(pr);
        e.stub.on("GET /v1/asks", json!([]));
        e.stub.on(
            "POST /v1/asks",
            json!({
                "id": 77,
                "board": "factory",
                "task": "factory-4",
                "agent": DISPATCH,
                "kind": "action",
                "question": "review required",
                "options": ["Approved — resume merge", "Reopen for a worker"],
                "status": "open",
                "answer": null,
                "answered_via": null,
                "created_at": at(),
                "answered_at": null,
            }),
        );
        let mut blocked = review_task("factory-4", 0);
        blocked.status = TaskStatus::Blocked;
        blocked.version = 4;
        e.stub.on("PATCH /v1/tasks/factory-4", &blocked);

        e.daemon.reconcile(at()).await.unwrap();
        assert!(e.stub.requests_to("POST /v1/asks").is_empty());

        e.daemon.reconcile(at() + hold).await.unwrap();
        let asks = e.stub.requests_to("POST /v1/asks");
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].body["kind"], "action");
        assert_eq!(asks[0].body["task"], "factory-4");
        assert!(
            asks[0].body["question"]
                .as_str()
                .unwrap()
                .contains("Reason: only-me")
        );
        let patches = e.stub.requests_to("PATCH /v1/tasks/factory-4");
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].body["status"], "blocked");
        assert!(
            patches[0].body["blocked_reason"]
                .as_str()
                .unwrap()
                .contains("ask #77")
        );
        assert!(
            e.stub
                .requests_to("POST /v1/tasks/factory-4/release")
                .is_empty()
        );
        assert!(e.daemon.state.merges.is_empty());
    }
}

#[tokio::test]
async fn legacy_dispatch_answers_human_review_ask_and_ignores_moved_tasks() {
    let ask_event = |seq: i64, id: i64| {
        let mut ev = event(seq, EventType::AskAnswered, Some("factory-4"), "user");
        ev.payload = json!({ "id": id, "agent": DISPATCH, "via": "telegram" });
        ev
    };
    let ask_fixture = |id: i64, answer: &str| {
        json!({
            "id": id,
            "board": "factory",
            "task": "factory-4",
            "agent": DISPATCH,
            "kind": "action",
            "question": "PR requires human review",
            "options": ["Approved — resume merge", "Reopen for a worker"],
            "status": "answered",
            "answer": answer,
            "answered_via": "telegram",
            "created_at": at(),
            "answered_at": at(),
        })
    };

    // 1. Approved answer moves blocked task back to review via PATCH (not release).
    {
        let mut e = judges_env(&[]);
        let mut blocked = review_task("factory-4", 0);
        blocked.status = TaskStatus::Blocked;
        blocked.blocked_reason = Some(format!("waiting on ask #77: required human review on {PR}"));
        let mut d = detail("factory-4", false, &[]);
        d.task = blocked.clone();
        e.stub.on("GET /v1/tasks/factory-4", &d);
        e.stub
            .on("POST /v1/tasks/factory-4/notes", note(&blocked.id));
        e.stub.on(
            "GET /v1/asks/77",
            ask_fixture(77, "Approved — resume merge"),
        );
        e.stub.on("GET /v1/asks", json!([]));
        let mut back = blocked.clone();
        back.status = TaskStatus::Review;
        back.blocked_reason = None;
        back.version += 1;
        e.stub.on("PATCH /v1/tasks/factory-4", &back);

        e.daemon.reconcile(at()).await.unwrap();
        e.daemon.on_event(&ask_event(90, 77), at()).await.unwrap();
        e.daemon.reconcile(at()).await.unwrap();
        let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].body["text"]
                .as_str()
                .unwrap()
                .contains("Approved — resume merge")
        );
        let patches = e.stub.requests_to("PATCH /v1/tasks/factory-4");
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].body["status"], "review");
        assert!(
            e.stub
                .requests_to("POST /v1/tasks/factory-4/release")
                .is_empty()
        );
    }

    // 2. Reopen answer voids pass and sets task open via PATCH (works even if owner is None).
    {
        let mut e = judges_env(&[]);
        let mut blocked = review_task("factory-4", 0);
        blocked.status = TaskStatus::Blocked;
        blocked.owner = None;
        blocked.blocked_reason = Some(format!("waiting on ask #78: required human review on {PR}"));
        let mut d = detail("factory-4", false, &[]);
        d.task = blocked.clone();
        e.stub.on("GET /v1/tasks/factory-4", &d);
        e.stub
            .on("POST /v1/tasks/factory-4/notes", note(&blocked.id));
        e.stub
            .on("GET /v1/asks/78", ask_fixture(78, "Reopen for a worker"));
        e.stub.on("GET /v1/asks", json!([]));
        let mut opened = blocked.clone();
        opened.status = TaskStatus::Open;
        opened.blocked_reason = None;
        opened.version += 1;
        e.stub.on("PATCH /v1/tasks/factory-4", &opened);

        e.daemon.reconcile(at()).await.unwrap();
        e.daemon.on_event(&ask_event(91, 78), at()).await.unwrap();
        e.daemon.reconcile(at()).await.unwrap();
        let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].body["text"]
                .as_str()
                .unwrap()
                .starts_with("decision: review: void")
        );
        let patches = e.stub.requests_to("PATCH /v1/tasks/factory-4");
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].body["status"], "open");
        assert!(
            e.stub
                .requests_to("POST /v1/tasks/factory-4/release")
                .is_empty()
        );
    }

    // 3. Task already moved out of blocked: noop with no spurious note and no status patch.
    {
        let mut e = judges_env(&[]);
        let task = review_task("factory-4", 0);
        let mut d = detail("factory-4", false, &[]);
        d.task = task;
        e.stub.on("GET /v1/tasks/factory-4", &d);
        e.stub.on(
            "GET /v1/asks/79",
            ask_fixture(79, "Approved — resume merge"),
        );

        e.daemon.reconcile(at()).await.unwrap();
        e.daemon.on_event(&ask_event(92, 79), at()).await.unwrap();
        e.daemon.reconcile(at()).await.unwrap();
        assert!(
            e.stub
                .requests_to("POST /v1/tasks/factory-4/notes")
                .is_empty()
        );
        assert!(e.stub.requests_to("PATCH /v1/tasks/factory-4").is_empty());
        assert!(
            e.stub
                .requests_to("POST /v1/tasks/factory-4/release")
                .is_empty()
        );
    }
}

#[tokio::test]
async fn a_passed_task_without_a_pull_request_is_finished_once_its_worker_stops() {
    // Its worker still runs: it may still write its report.
    let mut e = passed_env("none", AgentState::Running);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(finishes(&e).is_empty());

    e.stub.on(
        &format!("GET {}", agent_path(WORKER)),
        agent_fixture(WORKER, DEVICE, AgentState::Ended),
    );
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    let done = finishes(&e);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].body["summary"], "The limit is now 80 characters.");
    let evidence = done[0].body["evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0]["kind"], "check");
    assert!(e.t3.gh.calls().is_empty(), "no pull request to read");
}

#[tokio::test]
async fn a_pass_the_worker_wrote_or_one_before_the_task_was_parked_again_is_no_pass() {
    // The task's own worker cannot pass it.
    let task = review_task("factory-4", 0);
    let mut e = judges_env(std::slice::from_ref(&task));
    let mut own = pass_note(&task, "none");
    own.agent = WORKER.parse().unwrap();
    review_detail(&e, &task, vec![own]);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["review factory-4 Task factory-4"]);
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-4/done")
            .is_empty()
    );

    // A pass from before the task went back to its worker is void: a new review.
    let mut e = judges_env(std::slice::from_ref(&task));
    let pass = pass_note(&task, "none");
    let mut d = detail("factory-4", false, &[]);
    d.task = task.clone();
    let mut parked = event(50, EventType::TaskUpdated, Some("factory-4"), WORKER);
    parked.at = pass.at + chrono::Duration::minutes(5);
    parked.payload = json!({"from": "doing", "to": "review"});
    d.notes = vec![judge_note(&task), pass];
    d.history = vec![parked];
    e.stub.on("GET /v1/tasks/factory-4", d);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["review factory-4 Task factory-4"]);
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-4/done")
            .is_empty()
    );
}

#[tokio::test]
async fn a_pass_by_an_agent_the_dispatcher_did_not_start_as_judge_closes_nothing() {
    let task = review_task("factory-4", 0);
    let mut e = judges_env(std::slice::from_ref(&task));
    // No note of the dispatcher names the author as this task's judge.
    review_detail(&e, &task, vec![pass_note(&task, PR)]);
    e.t3.gh.put(open_pr(MergeState::Clean, Checks::Passed));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(e.t3.gh.ran("merge"), 0);
    assert!(finishes(&e).is_empty());
    assert_eq!(titles(&e.t3), ["review factory-4 Task factory-4"]);
}

#[tokio::test]
async fn a_passed_pull_request_is_merged_at_the_head_read_and_not_while_its_worker_runs() {
    let mut e = passed_env(PR, AgentState::Running);
    e.t3.gh.put(open_pr(MergeState::Clean, Checks::Passed));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.gh.calls().is_empty(), "its worker may still push");

    e.stub.on(
        &format!("GET {}", agent_path(WORKER)),
        agent_fixture(WORKER, DEVICE, AgentState::Idle),
    );
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert!(
        e.t3.gh
            .calls()
            .contains(&format!("merge {PR} at 9c1e44d0a7b2c3")),
        "{:?}",
        e.t3.gh.calls()
    );
    assert_eq!(finishes(&e).len(), 1);
}

#[tokio::test]
async fn a_passed_task_with_after_merge_checks_runs_after_merge_judge_before_finishing() {
    let task = review_task("factory-4", 0);
    let mut e = judges_env(std::slice::from_ref(&task));
    let pre_pass = task_note(
        2,
        &task,
        JUDGE,
        "decision",
        &format!(
            "decision: review: pass. Fixed the limit.\ncheck: cargo test --workspace => 0\nafter merge: deploy/upgrade.sh --yes and check /healthz\npr: {PR}"
        ),
    );
    review_detail(&e, &task, vec![judge_note(&task), pre_pass.clone()]);
    e.stub.on(
        &format!("GET {}", agent_path(WORKER)),
        agent_fixture(WORKER, DEVICE, AgentState::Idle),
    );
    // Even if the pre-merge review judge retry cooldown is active, the after-merge judge
    // uses its own retry key and starts immediately once the PR merges.
    e.daemon.state.judge_retry.insert(
        "review:factory-4".to_owned(),
        board::daemon::judges::JudgeRetry {
            until: at() + chrono::Duration::minutes(30),
            updated_at: Some(task.updated_at),
        },
    );
    e.t3.gh.put(open_pr(MergeState::Clean, Checks::Passed));

    // Tick 1: merges PR, does NOT call task_done, writes AFTER_MERGE_READY_NOTE, and
    // launches the after-merge review judge with the AfterMerge brief.
    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        e.t3.gh
            .calls()
            .contains(&format!("merge {PR} at 9c1e44d0a7b2c3")),
        "{:?}",
        e.t3.gh.calls()
    );
    assert!(
        finishes(&e).is_empty(),
        "must not finish task while after-merge checks remain"
    );
    let posted_notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
    assert!(
        posted_notes.iter().any(|r| {
            r.body["text"]
                .as_str()
                .unwrap_or_default()
                .starts_with("note: merged; waiting for after-merge checks")
        }),
        "expected Trail Rule after-merge handoff note: {posted_notes:?}"
    );
    let starts = worker_starts(&e.t3);
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0].0, "after-merge factory-4 Task factory-4");
    assert!(
        starts[0].1.contains("## After-merge review") && starts[0].1.contains("deploy/upgrade.sh"),
        "{}",
        starts[0].1
    );
    assert!(e.daemon.state.judges.iter().any(|j| j.after_merge));

    // Tick 2: the after-merge judge finished and wrote a clean `review: pass` (no
    // `after merge:` lines). Dispatcher finishes the task with BOTH pre-merge and
    // post-merge check evidence plus PR and merge commit.
    let ready_text = posted_notes
        .iter()
        .find_map(|r| {
            let t = r.body["text"].as_str()?;
            t.starts_with("note: merged; waiting for after-merge checks")
                .then(|| t.to_owned())
        })
        .unwrap();
    let ready_note = task_note(3, &task, DISPATCH, "note", &ready_text);
    let after_judge_agent = format!("t3:{NEW_THREAD}");
    let after_judge_note = task_note(
        4,
        &task,
        DISPATCH,
        "note",
        &format!(
            "note: the dispatcher started a review judge for this task: thread {NEW_THREAD} (after-merge) (agent {after_judge_agent})"
        ),
    );
    let post_pass = task_note(
        5,
        &task,
        &after_judge_agent,
        "decision",
        &format!(
            "decision: review: pass. Deployed and verified.\ncheck: deploy/upgrade.sh --yes deadbeef => 0\npr: {PR}"
        ),
    );
    review_detail(
        &e,
        &task,
        vec![
            judge_note(&task),
            pre_pass,
            ready_note,
            after_judge_note,
            post_pass,
        ],
    );
    let mut handed = task.clone();
    handed.owner = Some(DISPATCH.parse().unwrap());
    handed.version = 4;
    e.stub.on("PATCH /v1/tasks/factory-4", &handed);
    let mut done = handed;
    done.status = TaskStatus::Done;
    e.stub.on("POST /v1/tasks/factory-4/done", &done);

    e.daemon
        .reconcile(at() + chrono::Duration::minutes(3))
        .await
        .unwrap();
    let done_reqs = finishes(&e);
    assert_eq!(done_reqs.len(), 1, "{done_reqs:?}");
    let evidence = done_reqs[0].body["evidence"].as_array().unwrap();
    assert!(
        evidence
            .iter()
            .any(|ev| ev["kind"] == "pr" && ev["value"] == PR),
        "{evidence:?}"
    );
    assert!(
        evidence
            .iter()
            .any(|ev| ev["kind"] == "check" && ev["value"] == "cargo test --workspace"),
        "pre-merge check evidence must be preserved: {evidence:?}"
    );
    assert!(
        evidence.iter().any(|ev| {
            ev["kind"] == "check" && ev["value"] == "deploy/upgrade.sh --yes deadbeef"
        }),
        "post-merge check evidence must be attached: {evidence:?}"
    );
}

#[tokio::test]
async fn an_after_merge_pass_that_still_leaves_after_merge_checks_reopens_without_looping() {
    let task = review_task("factory-4", 0);
    let mut e = judges_env(std::slice::from_ref(&task));
    let pre_pass = task_note(
        2,
        &task,
        JUDGE,
        "decision",
        &format!(
            "decision: review: pass. Code ok.\ncheck: cargo test => 0\nafter merge: deploy/upgrade.sh --yes\npr: {PR}"
        ),
    );
    let ready_note = task_note(
        3,
        &task,
        DISPATCH,
        "note",
        &format!(
            "note: merged; waiting for after-merge checks (pass #2):\npr: {PR}\nbase: main\nleft: deploy/upgrade.sh --yes"
        ),
    );
    let after_judge_agent = format!("t3:{NEW_THREAD}");
    let after_judge_note = task_note(
        4,
        &task,
        DISPATCH,
        "note",
        &format!(
            "note: the dispatcher started a review judge for this task: thread {NEW_THREAD} (after-merge) (agent {after_judge_agent})"
        ),
    );
    let bad_post_pass = task_note(
        5,
        &task,
        &after_judge_agent,
        "decision",
        &format!(
            "decision: review: pass. Still not deployed.\nafter merge: deploy/upgrade.sh --yes\npr: {PR}"
        ),
    );
    review_detail(
        &e,
        &task,
        vec![
            judge_note(&task),
            pre_pass,
            ready_note,
            after_judge_note,
            bad_post_pass,
        ],
    );
    e.stub.on(
        &format!("GET {}", agent_path(WORKER)),
        agent_fixture(WORKER, DEVICE, AgentState::Idle),
    );
    let mut open = task;
    open.status = TaskStatus::Open;
    open.owner = None;
    e.stub.on("POST /v1/tasks/factory-4/release", open);
    let mut merged = open_pr(MergeState::Clean, Checks::Passed);
    merged.state = PrState::Merged;
    merged.merge_commit = Some("deadbeef1234".to_owned());
    e.t3.gh.put(merged);

    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        finishes(&e).is_empty(),
        "must never finish with unrun checks"
    );
    assert!(
        worker_starts(&e.t3).is_empty(),
        "must not spawn another after-merge judge in a loop"
    );
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-4/release").len(),
        1
    );
    let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
    assert!(
        notes.iter().any(|r| r.body["kind"] == "finding"
            && r.body["text"]
                .as_str()
                .unwrap_or_default()
                .contains("left post-merge checks unrun")),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|r| r.body["kind"] == "decision"
            && r.body["text"]
                .as_str()
                .unwrap_or_default()
                .starts_with("decision: review: void")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_pull_request_outside_the_boards_repositories_or_named_without_its_url_is_left() {
    for pr in ["https://github.com/o/other/pull/3", "#12"] {
        let mut e = passed_env(pr, AgentState::Idle);
        e.daemon.reconcile(at()).await.unwrap();
        assert!(e.t3.gh.calls().is_empty(), "{pr}: {:?}", e.t3.gh.calls());
        assert!(finishes(&e).is_empty(), "{pr}: not finished as without one");
        assert!(worker_starts(&e.t3).is_empty());
    }
}

#[tokio::test]
async fn a_passed_task_whose_pull_request_was_closed_is_set_open_with_a_note() {
    let mut e = passed_env(PR, AgentState::Idle);
    let mut closed = open_pr(MergeState::Waits, Checks::Passed);
    closed.state = PrState::Closed;
    e.t3.gh.put(closed);
    e.daemon.reconcile(at()).await.unwrap();
    let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
    assert_eq!(notes.len(), 1);
    let text = notes[0].body["text"].as_str().unwrap();
    assert!(
        text.starts_with("decision: review: void") && text.contains("closed"),
        "{text}"
    );
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-4/release").len(),
        1
    );
    assert!(finishes(&e).is_empty());
}

#[tokio::test]
async fn a_board_at_its_usage_stop_gets_no_new_judge() {
    let task = review_task("factory-4", 0);
    let mut e = judges_env(std::slice::from_ref(&task));
    review_detail(&e, &task, vec![]);
    e.stub.on("GET /v1/usage", usage_reading(95.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(worker_starts(&e.t3).is_empty());

    e.stub.on("GET /v1/usage", usage_reading(40.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["review factory-4 Task factory-4"]);
}

#[tokio::test]
async fn review_judge_skips_soft_pace_hold_while_other_judges_and_hard_limits_still_hold() {
    let review = review_task("factory-4", 0);
    let prop = proposal("factory-5", None, 1);

    // Helper producing a Usage reading with a specific weekly percent and pace state.
    let paced_usage = |week_pct: f64, line: f64, hard: bool| {
        let day_start = at() - chrono::Duration::hours(2);
        let resets = day_start + chrono::Duration::days(6);
        json!({
            "five_hour": { "percent": 5.0, "resets_at": at() + chrono::Duration::hours(3) },
            "seven_day": { "percent": week_pct, "resets_at": resets },
            "read_at": at(),
            "device": DEVICE,
            "pace": {
                "reserve": 5.0,
                "day_start": day_start,
                "anchor": 0.0,
                "line": line,
                "projected": week_pct,
                "hard_since": hard.then_some(at()),
            }
        })
    };

    // 1. Soft daily pace hold (25% >= line 15.8%, below usage_stop_at=90 and hard=95):
    //    review judge starts, verdict judge waits!
    let mut e = judges_env(&[review.clone(), prop]);
    review_detail(&e, &review, vec![]);
    e.stub.on("GET /v1/usage", paced_usage(25.0, 15.8, false));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        titles(&e.t3),
        ["review factory-4 Task factory-4"],
        "review judge must skip soft daily pace hold while verdict waits"
    );
    // Escalating the same board from soft pace hold to hard usage_stop_at must write a
    // distinct `judge_hard` drop record (never deduplicated away by the soft hold key).
    let review2 = review_task("factory-6", 2);
    e.stub.on("GET /v1/tasks", [review2.clone()]);
    review_detail(&e, &review2, vec![]);
    e.stub.on("GET /v1/usage", paced_usage(92.0, 80.0, false));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        titles(&e.t3),
        ["review factory-4 Task factory-4"],
        "escalated usage_stop_at must hold the new review judge"
    );
    let drops = read_daemon_drops(&e);
    assert!(
        drops
            .iter()
            .any(|d| d["kind"] == "usage_hold" && d["subject"]["kind"] == "judge"),
        "expected soft-pace judge hold drop record: {drops:?}"
    );
    assert!(
        drops
            .iter()
            .any(|d| d["kind"] == "usage_hold" && d["subject"]["kind"] == "judge_hard"),
        "expected distinct hard-limit judge_hard drop record after escalation: {drops:?}"
    );

    // 2. Hard reserve line (96% >= 95%): review judge is held even when usage_stop_at=99!
    let mut e_hard = judges_env(std::slice::from_ref(&review));
    let mut high_stop = autopilot(Some(DISPATCH), &[DEVICE]);
    high_stop.usage_stop_at = 99;
    e_hard.stub.on("GET /v1/boards", [high_stop]);
    review_detail(&e_hard, &review, vec![]);
    e_hard
        .stub
        .on("GET /v1/usage", paced_usage(96.0, 80.0, true));
    e_hard.daemon.reconcile(at()).await.unwrap();
    assert!(
        worker_starts(&e_hard.t3).is_empty(),
        "hard reserve line must hold review judge"
    );

    // 3. Board usage_stop_at (92% >= 90%, below hard reserve 95%): review judge is held!
    let mut e_stop = judges_env(std::slice::from_ref(&review));
    review_detail(&e_stop, &review, vec![]);
    e_stop
        .stub
        .on("GET /v1/usage", paced_usage(92.0, 80.0, false));
    e_stop.daemon.reconcile(at()).await.unwrap();
    assert!(
        worker_starts(&e_stop.t3).is_empty(),
        "usage_stop_at must hold review judge"
    );
}

#[tokio::test]
async fn a_judge_start_that_made_no_thread_is_tried_again_soon() {
    let task = review_task("factory-4", 0);
    let mut e = judges_env(std::slice::from_ref(&task));
    review_detail(&e, &task, vec![]);
    e.t3.fail_start.store(true, Ordering::SeqCst);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(registered(&e).is_empty());
    assert_eq!(
        e.daemon.state.judges.len(),
        1,
        "kept until its thread is looked for"
    );

    // No thread came of it: the start is tried again after the short wait, not the
    // retry of a judge that ran.
    e.t3.fail_start.store(false, Ordering::SeqCst);
    let minute = chrono::Duration::minutes(1);
    e.daemon.reconcile(at() + minute).await.unwrap();
    assert!(e.daemon.state.judges.is_empty());
    assert!(registered(&e).is_empty());
    e.daemon
        .reconcile(at() + minute + board::daemon::JUDGE_START_RETRY)
        .await
        .unwrap();
    assert_eq!(registered(&e).len(), 1);
}

#[tokio::test]
async fn a_judge_start_the_daemon_did_not_finish_is_registered_with_the_thread_it_made() {
    let task = review_task("factory-4", 0);
    let mut e = judges_env(std::slice::from_ref(&task));
    review_detail(&e, &task, vec![]);
    // T3 made the thread, and the start then timed out.
    *e.t3.clock.lock().unwrap() = at();
    *e.t3.die_after_create.lock().unwrap() = Some(true);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(registered(&e).is_empty());

    *e.t3.die_after_create.lock().unwrap() = None;
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(
        registered(&e),
        [json!({"agent": format!("t3:{ORPHAN}"), "kind": "review", "tasks": ["factory-4"]})]
    );
    assert_eq!(worker_starts(&e.t3).len(), 1, "no second thread");
    // The judge's agent id ends the dispatcher's note: a pass counts only from it.
    let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
    let text = notes[0].body["text"].as_str().unwrap();
    assert!(text.ends_with(&format!("(agent t3:{ORPHAN})")), "{text}");
}

/// A wake-up asks for no snapshot call: its lines name the change, and the woken agent's
/// prompt hook adds the board delta to the prompt the message arrives as.
#[tokio::test]
async fn a_wake_up_prompt_carries_the_board_change_without_a_snapshot_call() {
    const SESSION: &str = "00893aaf-19fa-41d2-8238-13269b9b3ca0";
    let (mut e, _) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-4"], at()).await;
    e.daemon.flush(at()).await;
    let (_, message) = e.t3.sends().pop().expect("a wake-up");
    assert!(message.starts_with("board factory: every blocker of your blocked factory-4 is done; resume it (task_update factory-4 status=doing)\n"));
    assert!(!message.contains("board_snapshot"), "{message}");

    // The thread gets the message as its next prompt, where the hook runs.
    let dir = e._dir.path().join("hook");
    std::fs::create_dir_all(&dir).unwrap();
    let home = home_for(&dir, &e.stub.url);
    let repo = git_repo(&dir, "git@github.com:syz51/agent-board.git");
    let delta = "<board>\nalerts: factory-4 is ready\ncounts: ready 1\n</board>";
    e.stub.on(
        &format!(
            "POST /v1/agents/{}/state",
            enc(&format!("claude:{SESSION}"))
        ),
        agent_fixture(&format!("claude:{SESSION}"), DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(board_proto::SnapshotKind::Delta, delta, 11),
    );
    let payload = json!({
        "session_id": SESSION,
        "cwd": repo,
        "hook_event_name": "UserPromptSubmit",
        "prompt": message,
    });
    let out = tokio::task::spawn_blocking(move || {
        board(
            &home,
            &repo,
            &[],
            &["hook", "user-prompt-submit", "--runtime", "claude"],
            &payload.to_string(),
        )
    })
    .await
    .unwrap();
    let reply: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("JSON output");
    assert_eq!(
        reply["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap(),
        delta
    );
    let asked = e.stub.requests_to("GET /v1/snapshot");
    assert_eq!(asked.last().unwrap().query_pairs()["kind"], "delta");
}

/// A second daemon-dispatch board this device runs, with one judge of it at work.
fn other_board_with_a_judge(e: &mut Env, slots: u32) {
    let mut board = daemon_board(None);
    board.parallel_tasks = slots;
    let mut held = daemon_board(Some(DISPATCH));
    held.parallel_tasks = slots;
    let mut other = held.clone();
    other.slug = "other".parse().unwrap();
    e.stub.on("GET /v1/boards", [board, other.clone()]);
    e.stub.on("POST /v1/boards/factory/coordinator", held);
    e.stub.on("POST /v1/boards/other/coordinator", other);
    e.daemon.state.judges.push(board::daemon::judges::JudgeRun {
        board: "other".parse().unwrap(),
        kind: board_proto::JudgeKind::Goal,
        tasks: Vec::new(),
        thread: Some(J2.to_owned()),
        runner: None,
        agent_id: None,
        worktree_id: None,
        worktree_path: None,
        run_id: None,
        title: "goal review other".to_owned(),
        started_at: at(),
        registered: false,
        noted: false,
        worktree_note: None,
        after_merge: false,
    });
}

#[tokio::test]
async fn the_device_limit_counts_the_runs_of_every_board_and_keeps_a_slot_for_judges() {
    use board_proto::Priority::P1;
    let tasks = [
        ready_task("factory-4", P1, 0),
        ready_task("factory-5", P1, 1),
        ready_task("factory-6", P1, 2),
    ];
    // Four runs at once (default reserve 1 -> daemon limit 3): two may be workers, and the
    // other board's judge is one.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(4);
    other_board_with_a_judge(&mut e, 3);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3).len(), 2);

    // Three runs at once (daemon limit 2): the judge has one, and a worker takes the other.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(3);
    other_board_with_a_judge(&mut e, 3);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);

    // A paused board's judge still runs, so it keeps its slot.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(3);
    other_board_with_a_judge(&mut e, 3);
    let mut paused = daemon_board(Some(DISPATCH));
    paused.slug = "other".parse().unwrap();
    paused.status = board_proto::BoardStatus::Paused;
    let mut board = daemon_board(None);
    board.parallel_tasks = 3;
    e.stub.on("GET /v1/boards", [board, paused]);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);

    // Without a limit of the device, the board's own three slots fill.
    let mut e = slots_env(3, &tasks);
    other_board_with_a_judge(&mut e, 3);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3).len(), 3);
}

#[tokio::test]
async fn runs_at_once_in_the_configuration_replaces_the_default_device_limit() {
    use board_proto::Priority::P1;
    let tasks = [
        ready_task("factory-4", P1, 0),
        ready_task("factory-5", P1, 1),
        ready_task("factory-6", P1, 2),
    ];
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(2);
    let config = Config {
        server: Some(e.stub.url.clone()),
        device: Some(DEVICE.parse().unwrap()),
        projects: projects(),
        runs_at_once: std::num::NonZeroU32::new(4),
        ..Config::default()
    };
    let path = e._dir.path().join("home").join("config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        titles(&e.t3).len(),
        2,
        "four runs with default reserve 1 leave daemon limit 3 (two workers, one judge)"
    );
}

fn running_lists(t3: &FakeT3) -> usize {
    let calls = t3.calls().into_iter();
    calls.filter(|c| matches!(c, Call::Running(_))).count()
}

/// The holds by the device limit in the device's trail file.
fn holds(home: &Home) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(home.state_dir().join("device-holds.jsonl"));
    let text = text.unwrap_or_default();
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn the_turns_of_other_threads_take_the_slots_of_the_device() {
    use board_proto::Priority::P1;
    let tasks = [
        ready_task("factory-4", P1, 0),
        ready_task("factory-5", P1, 1),
        ready_task("factory-6", P1, 2),
    ];
    // Two threads the dispatcher did not start run on the device: on a four-run device
    // (default reserve 1 -> daemon limit 3), one slot is left so one worker starts and the
    // next task is held, with a note that says why.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(4);
    *e.t3.running.lock().unwrap() = vec!["user-1".to_owned(), "agent-child-2".to_owned()];
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);
    // T3 is asked once in the reconcile, for the one project of this device.
    assert_eq!(running_lists(&e.t3), 1);
    let notes = e.stub.requests_to("POST /v1/tasks/factory-5/notes");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].agent.as_deref(), Some(DISPATCH));
    let text = notes[0].body["text"].as_str().unwrap();
    for part in [
        "The device limit holds this task",
        "has 3 of 4 runs (daemon limit 3, 1 reserved): 1 of the dispatcher, 2 turns of other threads",
        "agent-child-2, user-1",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    assert!(text.chars().count() <= 800, "{text}");
    // The device's trail file has the hold too: the time, the task, the load, the threads.
    let held = holds(&home_of(&e));
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["kind"], "worker");
    assert_eq!(held[0]["subject"], "factory-5");
    assert_eq!(held[0]["board"], "factory");
    assert_eq!(held[0]["others"], json!(["agent-child-2", "user-1"]));
    assert!(held[0]["at"].is_string(), "{held:?}");
    let cause = held[0]["cause"].as_str().unwrap();
    assert!(
        cause.contains("has 3 of 4 runs (daemon limit 3, 1 reserved)"),
        "{cause}"
    );

    // The dispatcher's own run is among the running threads and counts once.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(3);
    other_board_with_a_judge(&mut e, 3);
    *e.t3.running.lock().unwrap() = vec![J2.to_owned()];
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);

    // A full device starts nothing, and a task held in two reconciles gets one note.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(2);
    *e.t3.running.lock().unwrap() = vec!["user-1".to_owned(), "user-2".to_owned()];
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert!(titles(&e.t3).is_empty());
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-4/notes").len(),
        1
    );
    assert_eq!(holds(&home_of(&e)).len(), 1);

    // A device without a limit asks T3 for nothing.
    let mut e = slots_env(3, &tasks);
    *e.t3.running.lock().unwrap() = vec!["user-1".to_owned()];
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3).len(), 3);
    assert_eq!(running_lists(&e.t3), 0);
}

/// The limit protects the device's memory, so a T3 that cannot list the running threads
/// leaves the load unknown: the dispatcher starts nothing and records the hold.
#[tokio::test]
async fn a_t3_that_cannot_list_threads_holds_every_start_with_a_record() {
    use board_proto::Priority::P1;
    let tasks = [
        ready_task("factory-4", P1, 0),
        ready_task("factory-5", P1, 1),
    ];
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(4);
    e.t3.fail_list.store(true, Ordering::SeqCst);
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert!(titles(&e.t3).is_empty());
    // One line in the trail file and one note on the task, for both passes.
    let held = holds(&home_of(&e));
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["kind"], "worker");
    assert_eq!(held[0]["subject"], "factory-4");
    let cause = held[0]["cause"].as_str().unwrap();
    assert!(
        cause.contains("T3 could not list the running threads of")
            && cause.contains("server not running")
            && cause.contains("nothing starts"),
        "{cause}"
    );
    assert_eq!(held[0]["unread"].as_array().unwrap().len(), 1, "{held:?}");
    let notes = e.stub.requests_to("POST /v1/tasks/factory-4/notes");
    assert_eq!(notes.len(), 1);
    let text = notes[0].body["text"].as_str().unwrap();
    assert!(
        text.contains("T3 could not list the running threads")
            && text.contains("device-holds.jsonl"),
        "{text}"
    );

    // T3 answers again: the task starts.
    e.t3.fail_list.store(false, Ordering::SeqCst);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(2))
        .await
        .unwrap();
    assert_eq!(titles(&e.t3)[0], "factory-4 Task factory-4");
}

/// A hold whose cause changes gets a line for each cause, and a hold older than
/// [`board::daemon::HOLD_KEPT`] is written again.
#[tokio::test]
async fn a_hold_gets_a_line_per_cause_and_a_new_one_after_a_week() {
    use board_proto::Priority::P1;
    let tasks = [ready_task("factory-4", P1, 0)];
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(2);
    *e.t3.running.lock().unwrap() = vec!["user-1".to_owned(), "user-2".to_owned()];
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(holds(&home_of(&e)).len(), 1);

    // T3 stops answering: the same task is held for another cause.
    e.t3.fail_list.store(true, Ordering::SeqCst);
    let later = at() + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();
    e.daemon
        .reconcile(later + chrono::Duration::minutes(1))
        .await
        .unwrap();
    let held = holds(&home_of(&e));
    assert_eq!(held.len(), 2, "{held:?}");
    assert_eq!(held[0]["unread"], json!([]));
    assert_eq!(held[1]["unread"].as_array().unwrap().len(), 1);
    let notes = || e.stub.requests_to("POST /v1/tasks/factory-4/notes").len();
    assert_eq!(notes(), 1);

    // Still held after the daemon forgot the records: both are written again.
    let week = later + board::daemon::HOLD_KEPT + chrono::Duration::minutes(1);
    e.daemon.reconcile(week).await.unwrap();
    assert_eq!(holds(&home_of(&e)).len(), 3);
    assert_eq!(notes(), 2);
}

/// The daemon keeps the holds it recorded, so a restart writes none of them again.
#[tokio::test]
async fn a_restart_writes_no_second_record_of_a_hold() {
    use board_proto::Priority::P1;
    let tasks = [ready_task("factory-4", P1, 0)];
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(2);
    *e.t3.running.lock().unwrap() = vec!["user-1".to_owned(), "user-2".to_owned()];
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(holds(&home_of(&e)).len(), 1);
    let notes = || e.stub.requests_to("POST /v1/tasks/factory-4/notes").len();
    assert_eq!(notes(), 1);

    let mut restarted =
        daemon_at(e._dir.path(), &e.stub, projects(), &e.t3).with_default_run_limit(2);
    restarted
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert!(titles(&e.t3).is_empty());
    assert_eq!(holds(&home_of(&e)).len(), 1);
    assert_eq!(notes(), 1);

    // The task starts: the daemon forgets the hold, so a later one is recorded anew.
    *e.t3.running.lock().unwrap() = Vec::new();
    restarted
        .reconcile(at() + chrono::Duration::minutes(2))
        .await
        .unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);
    assert!(
        restarted.state.holds.is_empty(),
        "{:?}",
        restarted.state.holds
    );
    assert!(restarted.state.hold_notes.is_empty());
}

/// A judge is held like a worker while T3 cannot list the running threads.
#[tokio::test]
async fn a_judge_waits_while_t3_cannot_list_threads() {
    let tasks = [review_task("factory-4", 0)];
    let mut e = judges_env(&tasks);
    review_detail(&e, &tasks[0], vec![]);
    e.daemon = e.daemon.with_default_run_limit(4);
    e.t3.fail_list.store(true, Ordering::SeqCst);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(5))
        .await
        .unwrap();
    assert!(titles(&e.t3).is_empty());
    let held = holds(&home_of(&e));
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["kind"], "judge");
    let cause = held[0]["cause"].as_str().unwrap();
    assert!(
        cause.contains("T3 could not list the running threads"),
        "{cause}"
    );
}

#[tokio::test]
async fn a_judge_waits_while_other_threads_fill_the_device() {
    let tasks = [review_task("factory-4", 0)];
    let mut e = judges_env(&tasks);
    review_detail(&e, &tasks[0], vec![]);
    e.daemon = e.daemon.with_default_run_limit(3);
    *e.t3.running.lock().unwrap() = vec!["user-1".to_owned(), "user-2".to_owned()];
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(5))
        .await
        .unwrap();
    assert!(titles(&e.t3).is_empty());
    // A held judge gets no note: the device's trail file is its record, once per hold.
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(5) + chrono::Duration::seconds(30))
        .await
        .unwrap();
    let held = holds(&home_of(&e));
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["kind"], "judge");
    assert_eq!(held[0]["subject"], "review factory-4 Task factory-4");
    assert_eq!(held[0]["others"], json!(["user-1", "user-2"]));
    assert!(
        e.stub
            .requests_to("POST /v1/tasks/factory-4/notes")
            .is_empty()
    );

    // One of them ends: the judge starts (leaving 1 reserved slot on the 3-run device).
    *e.t3.running.lock().unwrap() = vec!["user-1".to_owned()];
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(6))
        .await
        .unwrap();
    assert_eq!(titles(&e.t3), ["review factory-4 Task factory-4"]);
}

#[tokio::test]
async fn a_judge_waits_while_the_device_is_at_its_limit() {
    let tasks = [review_task("factory-4", 0), review_task("factory-5", 1)];
    let mut e = judges_env(&tasks);
    for task in &tasks {
        review_detail(&e, task, vec![]);
    }
    e.t3.ids
        .lock()
        .unwrap()
        .extend([J1.to_owned(), J2.to_owned()]);
    e.daemon = e.daemon.with_default_run_limit(3);
    other_board_with_a_judge(&mut e, 1);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(5))
        .await
        .unwrap();
    assert_eq!(titles(&e.t3), ["review factory-4 Task factory-4"]);
}

#[tokio::test]
async fn the_daemon_holds_workers_and_judges_for_the_reserved_turn_and_records_each_hold() {
    use board_proto::Priority::P1;
    // On a 4-run device with default reserve 1 (daemon limit 3), two workers and one judge
    // can run, but a third worker or a second judge is held for the reserved turn.
    let tasks = [
        ready_task("factory-4", P1, 0),
        ready_task("factory-5", P1, 1),
        ready_task("factory-6", P1, 2),
    ];
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(4);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        titles(&e.t3),
        ["factory-4 Task factory-4", "factory-5 Task factory-5"]
    );
    let held = holds(&home_of(&e));
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["kind"], "worker");
    assert_eq!(held[0]["subject"], "factory-6");
    assert_eq!(held[0]["limit"], 2);
    assert!(
        held[0]["cause"]
            .as_str()
            .unwrap()
            .contains("has 2 of 4 runs (daemon limit 3, 1 reserved)"),
        "{held:?}"
    );

    // Judges also yield to the reserved turn: on a 2-run device (daemon limit 1) with one
    // judge already running, a second judge is held with a trail record.
    let review_tasks = [review_task("factory-4", 0)];
    let mut je = judges_env(&review_tasks);
    review_detail(&je, &review_tasks[0], vec![]);
    je.daemon = je.daemon.with_default_run_limit(2);
    other_board_with_a_judge(&mut je, 1);
    je.daemon
        .reconcile(at() + chrono::Duration::minutes(5))
        .await
        .unwrap();
    assert!(titles(&je.t3).is_empty());
    let jheld = holds(&home_of(&je));
    assert_eq!(jheld.len(), 1, "{jheld:?}");
    assert_eq!(jheld[0]["kind"], "judge");
    assert_eq!(jheld[0]["limit"], 1);
    assert!(
        jheld[0]["cause"]
            .as_str()
            .unwrap()
            .contains("has 1 of 2 runs (daemon limit 1, 1 reserved)"),
        "{jheld:?}"
    );

    // Hot-reloading config.toml with reserve_runs = 0 lifts the reserve without a restart.
    let config = Config {
        server: Some(je.stub.url.clone()),
        device: Some(DEVICE.parse().unwrap()),
        projects: projects(),
        runs_at_once: std::num::NonZeroU32::new(2),
        reserve_runs: Some(0),
        ..Config::default()
    };
    let path = je._dir.path().join("home").join("config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    je.daemon
        .reconcile(at() + chrono::Duration::minutes(6))
        .await
        .unwrap();
    assert_eq!(titles(&je.t3), ["review factory-4 Task factory-4"]);
}

#[tokio::test]
async fn a_ranked_task_starts_before_the_older_tasks_of_its_priority_level() {
    use board_proto::Priority::{P1, P2};
    let mut second = ready_task("factory-6", P2, 3);
    second.rank = Some(2);
    let mut first = ready_task("factory-7", P2, 4);
    first.rank = Some(1);
    let mut low = ready_task("factory-8", P2, -30);
    low.rank = None;
    let tasks = [low, second, first];
    let mut e = dispatch_env(&tasks);
    e.daemon.reconcile(at()).await.unwrap();
    // The rank leads, not the age: the newest task holds rank 1.
    let starts = worker_starts(&e.t3);
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].0, "factory-7 Task factory-7");

    // A rank orders one level only: a higher priority still starts first.
    let mut ranked = ready_task("factory-6", P2, 3);
    ranked.rank = Some(1);
    let tasks = [ranked, ready_task("factory-9", P1, 5)];
    let mut e = dispatch_env(&tasks);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(worker_starts(&e.t3)[0].0, "factory-9 Task factory-9");
}

/// The active board `slug` with this device as a home, not on autopilot.
fn off_autopilot(slug: &str) -> Board {
    let mut board = daemon_board(None);
    board.slug = slug.parse().unwrap();
    board.autopilot = false;
    board
}

/// A device with `boards`, each holding `tasks` and no judge at work.
fn off_autopilot_env(boards: &[Board], tasks: &[board_proto::Task]) -> Env {
    let e = env();
    e.stub.on("GET /v1/boards", boards);
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    e.stub.on("GET /v1/tasks", tasks);
    for board in boards {
        let judges = format!("/v1/boards/{}/judges", board.slug);
        e.stub.on(&format!("GET {judges}"), json!([]));
        e.stub.on(
            &format!("POST {judges}"),
            judge(NEW_THREAD, "verdict", &[], true),
        );
    }
    e
}

/// The user's choice in ask #171 (agent-board-491): proposals get their verdict on every
/// home board, and a board that is not on autopilot gets nothing else.
#[tokio::test]
async fn a_home_board_off_autopilot_gets_verdict_judges_and_no_other_run() {
    // Work that an autopilot board would start a worker, a review judge and, with the
    // goal due, a goal judge for.
    let tasks = [
        ready_task("factory-4", board_proto::Priority::P1, 0),
        review_task("factory-5", 0),
        proposal("factory-10", Some("factory-1"), 0),
        proposal("factory-11", Some("factory-1"), 1),
    ];
    let mut board = off_autopilot("factory");
    board.goal = "Ship the factory".into();
    let mut e = off_autopilot_env(&[board], &tasks);
    review_detail(&e, &tasks[1], vec![]);
    e.daemon.reconcile(at()).await.unwrap();

    assert_eq!(titles(&e.t3), ["verdict factory-10 (under factory-1)"]);
    assert_eq!(
        registered(&e),
        [json!({
            "agent": format!("t3:{NEW_THREAD}"), "kind": "verdict",
            "tasks": ["factory-10", "factory-11"],
        })]
    );
    // No lease, and nothing read of the board but its proposals.
    assert!(dispatch_claims(&e).is_empty());
    let lists = e.stub.requests_to("GET /v1/tasks");
    assert!(
        lists.iter().all(|r| r.query.contains("proposal=true")),
        "{lists:#?}"
    );
    assert!(e.stub.requests_to("GET /v1/tasks/factory-5").is_empty());
    assert!(e.stub.requests_to("PATCH /v1/tasks/factory-4").is_empty());

    // While that judge runs, nothing more starts.
    e.stub.on(
        "GET /v1/boards/factory/judges",
        [judge(
            NEW_THREAD,
            "verdict",
            &["factory-10", "factory-11"],
            true,
        )],
    );
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(titles(&e.t3).len(), 1);
}

#[tokio::test]
async fn a_board_off_autopilot_gets_no_judge_where_this_device_is_no_home_or_it_is_paused() {
    let tasks = [proposal("factory-10", None, 0)];
    let mut elsewhere = off_autopilot("factory");
    elsewhere.home_devices = vec!["mac-pro".parse().unwrap()];
    let mut homeless = off_autopilot("other");
    homeless.home_devices = Vec::new();
    let mut paused = off_autopilot("third");
    paused.status = board_proto::BoardStatus::Paused;
    let mut e = off_autopilot_env(&[elsewhere, homeless, paused], &tasks);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(titles(&e.t3).is_empty());
    assert!(e.stub.requests_to("GET /v1/tasks").is_empty());
    assert!(e.stub.requests_to("POST /v1/agents").is_empty());
}

#[tokio::test]
async fn a_board_off_autopilot_at_its_usage_stop_gets_no_verdict_judge() {
    let tasks = [proposal("factory-10", None, 0)];
    let mut e = off_autopilot_env(&[off_autopilot("factory")], &tasks);
    e.stub.on("GET /v1/usage", usage_reading(95.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(titles(&e.t3).is_empty());

    e.stub.on("GET /v1/usage", usage_reading(40.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["verdict factory-10 (no parent)"]);
}

#[tokio::test]
async fn the_boards_off_autopilot_share_two_verdict_judges_and_a_held_one_is_recorded() {
    let tasks = [proposal("factory-10", None, 0)];
    let boards = ["factory", "other", "third"].map(off_autopilot);
    let mut e = off_autopilot_env(&boards, &tasks);
    e.t3.ids
        .lock()
        .unwrap()
        .extend([J1.to_owned(), J2.to_owned()]);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3).len(), 2);
    for (slug, started) in [("factory", 1), ("other", 1), ("third", 0)] {
        let path = format!("POST /v1/boards/{slug}/judges");
        assert_eq!(e.stub.requests_to(&path).len(), started, "{slug}");
    }
    let held = holds(&home_of(&e));
    assert_eq!(held.len(), 1, "{held:#?}");
    assert_eq!(
        (&held[0]["kind"], &held[0]["board"], &held[0]["limit"]),
        (&json!("judge"), &json!("third"), &json!(2))
    );
    assert_eq!(held[0]["subject"], "verdict factory-10 (no parent)");
    assert_eq!(
        held[0]["cause"],
        "2 verdict judges run on the boards that are not on autopilot"
    );

    // The same hold is not written again, and the board starts once a slot is free.
    let minute = chrono::Duration::minutes(1);
    for (slug, thread) in [("factory", J1), ("other", J2)] {
        e.stub.on(
            &format!("GET /v1/boards/{slug}/judges"),
            [judge(thread, "verdict", &["factory-10"], true)],
        );
    }
    e.daemon.reconcile(at() + minute).await.unwrap();
    assert_eq!(titles(&e.t3).len(), 2);
    assert_eq!(holds(&home_of(&e)).len(), 1);
    // A paused board's judge is followed no longer, and takes none of the two.
    let mut boards = boards;
    boards[0].status = board_proto::BoardStatus::Paused;
    e.stub.on("GET /v1/boards", &boards[..]);
    e.daemon.reconcile(at() + minute * 2).await.unwrap();
    assert_eq!(e.stub.requests_to("POST /v1/boards/third/judges").len(), 1);
}

/// No lease gives a board off autopilot to one device, so the device whose judge is at
/// work has its proposals.
#[tokio::test]
async fn a_board_off_autopilot_that_another_home_device_judges_gets_no_judge_here() {
    let tasks = [
        proposal("factory-10", None, 0),
        proposal("factory-11", None, 1),
    ];
    let mut e = off_autopilot_env(&[off_autopilot("factory")], &tasks);
    let mut theirs = judge(J1, "verdict", &["factory-10"], true);
    theirs["registered_by"] = json!("dispatch:another-device");
    e.stub.on("GET /v1/boards/factory/judges", [theirs]);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(titles(&e.t3).is_empty());

    e.stub.on("GET /v1/boards/factory/judges", json!([]));
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(titles(&e.t3).len(), 1);
}

/// The trail rule: what keeps a board's proposals from their verdicts is written to the
/// device's trail, once.
#[tokio::test]
async fn a_failure_of_the_verdict_judges_of_a_board_off_autopilot_is_recorded_once() {
    let tasks = [proposal("factory-10", None, 0)];
    // The server does not answer for the board's judges.
    let mut e = off_autopilot_env(&[], &tasks);
    e.stub.on("GET /v1/boards", [off_autopilot("factory")]);
    let minute = chrono::Duration::minutes(1);
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.reconcile(at() + minute).await.unwrap();
    assert!(titles(&e.t3).is_empty());
    let held = holds(&home_of(&e));
    assert_eq!(held.len(), 1, "{held:#?}");
    assert_eq!(
        (&held[0]["kind"], &held[0]["board"]),
        (&json!("verdicts"), &json!("factory"))
    );
    assert_eq!(held[0]["subject"], "the verdict judges of factory");
    let cause = held[0]["cause"].as_str().unwrap();
    assert!(cause.contains("listing the board's judges"), "{cause}");
    // The dispatch agent is registered again after a failure: the server may have lost it.
    assert_eq!(e.stub.requests_to("POST /v1/agents").len(), 2);

    e.stub.on("GET /v1/boards/factory/judges", json!([]));
    e.stub.on(
        "POST /v1/boards/factory/judges",
        judge(NEW_THREAD, "verdict", &[], true),
    );
    e.daemon.reconcile(at() + minute * 2).await.unwrap();
    assert_eq!(titles(&e.t3), ["verdict factory-10 (no parent)"]);
}

/// A board whose autopilot is switched off while a review judge runs: that judge's
/// rights ended with the lease, so its run is dropped and keeps no judge slot.
#[tokio::test]
async fn a_review_judge_of_a_board_switched_off_autopilot_keeps_no_slot() {
    let task = review_task("factory-4", 0);
    let mut e = judges_env(std::slice::from_ref(&task));
    let parked = task_note(
        1,
        &task,
        WORKER,
        "note",
        &format!("note: parked in review, PR {PR}"),
    );
    review_detail(&e, &task, vec![parked]);
    e.t3.gh.put(open_pr(MergeState::Clean, Checks::Pending));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3).len(), 1);

    e.stub.on("GET /v1/boards", [off_autopilot("factory")]);
    e.stub.on(
        "GET /v1/tasks",
        [
            proposal("factory-10", Some("factory-1"), 0),
            proposal("factory-11", Some("factory-2"), 1),
        ],
    );
    e.t3.ids
        .lock()
        .unwrap()
        .extend([J1.to_owned(), J2.to_owned()]);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    // Both of the board's judge slots go to its proposals.
    assert_eq!(
        titles(&e.t3)[1..],
        [
            "verdict factory-10 (under factory-1)",
            "verdict factory-11 (under factory-2)"
        ]
    );
}

#[tokio::test]
async fn a_home_board_off_autopilot_with_verdict_judges_off_gets_no_judge() {
    let tasks = [proposal("factory-10", None, 0)];
    let mut board = off_autopilot("factory");
    board.verdict_judges = false;
    let mut e = off_autopilot_env(&[board.clone()], &tasks);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(titles(&e.t3).is_empty());
    assert!(
        e.stub
            .requests_to("GET /v1/boards/factory/judges")
            .is_empty()
    );
    assert!(e.stub.requests_to("GET /v1/tasks").is_empty());
    assert!(e.stub.requests_to("POST /v1/agents").is_empty());
    assert!(
        e.stub
            .requests_to("POST /v1/boards/factory/judges")
            .is_empty()
    );

    // Turning verdict judges back on dispatches the pending proposal.
    board.verdict_judges = true;
    e.stub.on("GET /v1/boards", [&board]);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(titles(&e.t3), ["verdict factory-10 (no parent)"]);

    // Disabling verdict judges while a verdict judge is tracked drops the tracked run, so
    // re-enabling after it ended starts a new judge immediately instead of waiting JUDGE_RETRY.
    board.verdict_judges = false;
    e.stub.on("GET /v1/boards", [&board]);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(2))
        .await
        .unwrap();

    board.verdict_judges = true;
    e.stub.on("GET /v1/boards", [&board]);
    e.stub.on("GET /v1/boards/factory/judges", json!([]));
    e.t3.ids.lock().unwrap().push_back(J2.to_owned());
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(3))
        .await
        .unwrap();
    assert_eq!(
        titles(&e.t3),
        [
            "verdict factory-10 (no parent)",
            "verdict factory-10 (no parent)"
        ]
    );
}

fn started_models(t3: &FakeT3) -> Vec<(String, Option<ThreadModel>)> {
    t3.calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Start { title, model, .. } => Some((title, model)),
            _ => None,
        })
        .collect()
}

fn env_with_models(
    tasks: &[board_proto::Task],
    thread_model: Option<ThreadModel>,
    judge_model: Option<ThreadModel>,
    judge_models: JudgeModels,
) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let t3 = FakeT3::default();
    let config = Config {
        server: Some(stub.url.clone()),
        device: Some(DEVICE.parse().unwrap()),
        projects: projects(),
        thread_model,
        judge_model,
        judge_models,
        ..Config::default()
    };
    let home = Home::at(dir.path().join("home"));
    config.save(&home).unwrap();
    let daemon = daemon_configured(dir.path(), &stub, &t3, None, config);
    let board = autopilot(Some(DISPATCH), &[DEVICE]);
    stub.on("GET /v1/boards", [&board]);
    stub.on("POST /v1/boards/factory/coordinator", &board);
    stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    stub.on("GET /v1/tasks", tasks);
    stub.on("GET /v1/boards/factory/judges", json!([]));
    stub.on(
        "POST /v1/boards/factory/judges",
        judge(NEW_THREAD, "verdict", &[], true),
    );
    for task in tasks {
        stub.on(
            &format!("GET /v1/tasks/{}", task.id),
            task_detail_for(task.clone()),
        );
        stub.on(&format!("POST /v1/tasks/{}/notes", task.id), note(&task.id));
    }
    Env {
        stub,
        t3,
        daemon,
        _dir: dir,
    }
}

#[tokio::test]
async fn workers_use_thread_model_while_judges_prefer_judge_model_and_fall_back() {
    let worker_model = ThreadModel {
        provider: "antigravity".into(),
        model: "gemini-4-neon".into(),
        options: [("thought_level".into(), "high".into())].into(),
    };
    let judge_model = ThreadModel {
        provider: "antigravity".into(),
        model: "gemini-4-neon".into(),
        options: [("thought_level".into(), "medium".into())].into(),
    };
    let tasks = [
        ready_task("factory-1", board_proto::Priority::P1, 0),
        proposal("factory-2", None, 1),
    ];

    // With both set, judges get [judge_model] and workers get [thread_model].
    let mut e = env_with_models(
        &tasks,
        Some(worker_model.clone()),
        Some(judge_model.clone()),
        JudgeModels::default(),
    );
    e.t3.ids.lock().unwrap().push_back(J2.to_owned());
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        started_models(&e.t3),
        [
            (
                "verdict factory-2 (no parent)".into(),
                Some(judge_model.clone())
            ),
            (
                "factory-1 Task factory-1".into(),
                Some(worker_model.clone())
            ),
        ]
    );

    // Without [judge_model], judges fall back to [thread_model].
    let mut e = env_with_models(
        &tasks,
        Some(worker_model.clone()),
        None,
        JudgeModels::default(),
    );
    e.t3.ids.lock().unwrap().push_back(J2.to_owned());
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        started_models(&e.t3),
        [
            (
                "verdict factory-2 (no parent)".into(),
                Some(worker_model.clone())
            ),
            ("factory-1 Task factory-1".into(), Some(worker_model)),
        ]
    );

    // Without either, both judges and workers leave project defaults (None).
    let mut e = env_with_models(&tasks, None, None, JudgeModels::default());
    e.t3.ids.lock().unwrap().push_back(J2.to_owned());
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        started_models(&e.t3),
        [
            ("verdict factory-2 (no parent)".into(), None),
            ("factory-1 Task factory-1".into(), None),
        ]
    );
}

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);
struct BufWriter(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = BufWriter;
    fn make_writer(&'a self) -> Self::Writer {
        BufWriter(self.0.clone())
    }
}

#[tokio::test]
async fn reload_config_warns_to_restart_when_judge_model_changes_and_keeps_active_model() {
    let original_judge = ThreadModel {
        provider: "antigravity".into(),
        model: "gemini-4-neon".into(),
        options: [("thought_level".into(), "medium".into())].into(),
    };
    let updated_judge = ThreadModel {
        provider: "antigravity".into(),
        model: "gemini-4-neon".into(),
        options: [("thought_level".into(), "low".into())].into(),
    };

    let tasks = [proposal("factory-10", None, 0)];
    let mut e = env_with_models(
        &tasks,
        None,
        Some(original_judge.clone()),
        JudgeModels::default(),
    );
    let home = Home::at(e._dir.path().join("home"));

    // Rewrite config.toml with an updated [judge_model] before reconcile reloads it.
    Config {
        server: Some(e.stub.url.clone()),
        device: Some(DEVICE.parse().unwrap()),
        projects: projects(),
        judge_model: Some(updated_judge),
        ..Config::default()
    }
    .save(&home)
    .unwrap();

    let buf = LogBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(buf.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    e.daemon.reconcile(at()).await.unwrap();
    drop(_guard);

    let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("judge_model") && logs.contains("restart board watch"),
        "expected restart warning for judge_model change, got: {logs:?}"
    );
    assert_eq!(
        started_models(&e.t3),
        [(
            "verdict factory-10 (no parent)".into(),
            Some(original_judge)
        )]
    );
}

/// Sets up a real git clone whose `origin` has `trunk` as its default branch plus
/// `extra_branches`, and a daemon-dispatch board with `tasks` in `review`.
fn git_review_env(
    extra_branches: &[&str],
    tasks: &[board_proto::Task],
) -> (tempfile::TempDir, Env) {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "--quiet", "--initial-branch=trunk"]);
    git(
        &origin,
        &["commit", "--quiet", "--allow-empty", "-m", "first"],
    );
    for branch in extra_branches {
        git(&origin, &["branch", branch]);
    }
    git(dir.path(), &["clone", "--quiet", "origin", "repo"]);
    let repo = dir.path().join("repo");

    let e = env_with_projects([(REPO.to_owned(), repo)].into());
    e.stub.on("GET /v1/boards", [daemon_board(None)]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        daemon_board(Some(DISPATCH)),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    e.stub.on("GET /v1/tasks", tasks);
    for task in tasks {
        e.stub.on(
            &format!("GET /v1/tasks/{}", task.id),
            task_detail_for(task.clone()),
        );
        e.stub
            .on(&format!("POST /v1/tasks/{}/notes", task.id), note(&task.id));
    }
    judges_at_work(&e, &[]);
    e.stub.on(
        "POST /v1/boards/factory/judges",
        judge(NEW_THREAD, "review", &[], true),
    );
    (dir, e)
}

fn started_reviews(t3: &FakeT3) -> Vec<(Option<NewWorktree>, String)> {
    t3.calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Start {
                worktree, message, ..
            } => Some((worktree, message)),
            _ => None,
        })
        .collect()
}

fn capture_warns() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    let buf = LogBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(buf.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (buf.0, guard)
}

fn flat_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[tokio::test]
async fn a_review_without_pr_starts_on_the_workers_suffixed_branch_fetched_from_origin() {
    let task = review_task("factory-4", 0);
    let (_dir, mut e) = git_review_env(
        &["task/factory-4", "task/factory-4-20261108-132005"],
        std::slice::from_ref(&task),
    );
    let started = task_note(
        1,
        &task,
        DISPATCH,
        "note",
        "note: the dispatcher started a worker for this task on branch task/factory-4-20261108-132005: https://t3.example.test/threads/w1",
    );
    review_detail(&e, &task, vec![started]);

    e.daemon.reconcile(at()).await.unwrap();
    let starts = started_reviews(&e.t3);
    assert_eq!(starts.len(), 1);
    assert_eq!(
        starts[0].0,
        Some(NewWorktree {
            base: "origin/task/factory-4-20261108-132005".into(),
            branch: "review/factory-4".into(),
        })
    );
    let brief = flat_text(&starts[0].1);
    assert!(
        brief.contains("origin/task/factory-4-20261108-132005"),
        "brief should name the worker branch: {brief}"
    );
    assert!(
        !brief.contains("on the head of its pull request"),
        "brief must not claim PR head when started from worker branch: {brief}"
    );

    // Without a dispatcher note, falls back to `task/<id>` on origin.
    let task_plain = review_task("factory-5", 0);
    let (_dir2, mut e2) = git_review_env(&["task/factory-5"], std::slice::from_ref(&task_plain));
    review_detail(&e2, &task_plain, vec![]);
    e2.daemon.reconcile(at()).await.unwrap();
    let starts2 = started_reviews(&e2.t3);
    assert_eq!(starts2.len(), 1);
    assert_eq!(
        starts2[0].0,
        Some(NewWorktree {
            base: "origin/task/factory-5".into(),
            branch: "review/factory-5".into(),
        })
    );
}

#[tokio::test]
async fn a_review_without_pr_or_pushed_branch_waits_then_uses_the_recorded_pr() {
    let task = review_task("factory-4", 0);
    let (_dir, mut e) = git_review_env(&["feat/custom"], std::slice::from_ref(&task));
    let started = task_note(
        1,
        &task,
        DISPATCH,
        "note",
        "note: the dispatcher started a worker for this task on branch task/factory-4: https://t3.example.test/threads/w1",
    );
    review_detail(&e, &task, vec![started.clone()]);

    // Reconcile right when parked: no PR recorded yet and `task/factory-4` is not on origin.
    // Must wait rather than silently starting on `origin/trunk`, and record the hold once
    // in `state/device-holds.jsonl` (trail rule).
    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        started_reviews(&e.t3).is_empty(),
        "must wait a tick instead of falling back to default base immediately"
    );
    let home = Home::at(e._dir.path().join("home"));
    let recorded = holds(&home);
    assert_eq!(
        recorded.len(),
        1,
        "expected one review_wait hold: {recorded:?}"
    );
    assert_eq!(recorded[0]["kind"], "review_wait");
    assert_eq!(recorded[0]["board"], "factory");
    assert_eq!(recorded[0]["subject"], "review factory-4 Task factory-4");
    assert!(
        recorded[0]["cause"]
            .as_str()
            .unwrap()
            .contains("task/factory-4"),
        "{:?}",
        recorded[0]
    );

    // Repeating while still waiting writes no duplicate hold line.
    e.daemon
        .reconcile(at() + chrono::Duration::seconds(30))
        .await
        .unwrap();
    assert_eq!(holds(&home).len(), 1, "once per cause");

    // One minute later, the worker's parking note names the PR on `feat/custom`.
    let parked = task_note(
        2,
        &task,
        WORKER,
        "note",
        &format!("note: parked in review, PR {PR}"),
    );
    review_detail(&e, &task, vec![started, parked]);
    let mut pr = open_pr(MergeState::Clean, Checks::Pending);
    pr.head_ref = "feat/custom".into();
    e.t3.gh.put(pr);

    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    let starts = started_reviews(&e.t3);
    assert_eq!(starts.len(), 1);
    assert_eq!(
        starts[0].0,
        Some(NewWorktree {
            base: "origin/feat/custom".into(),
            branch: "review/factory-4".into(),
        })
    );
    assert!(
        flat_text(&starts[0].1).contains("on the head of its pull request"),
        "{}",
        starts[0].1
    );
}

#[tokio::test]
async fn a_review_with_a_recorded_same_repo_pr_starts_on_the_pr_head() {
    let task = review_task("factory-4", 0);
    let (_dir, mut e) = git_review_env(
        &["task/factory-4", "feat/limit"],
        std::slice::from_ref(&task),
    );
    let parked = task_note(
        1,
        &task,
        WORKER,
        "note",
        &format!("note: parked in review, PR {PR}"),
    );
    review_detail(&e, &task, vec![parked]);
    e.t3.gh.put(open_pr(MergeState::Clean, Checks::Pending));

    e.daemon.reconcile(at()).await.unwrap();
    let starts = started_reviews(&e.t3);
    assert_eq!(starts.len(), 1);
    assert_eq!(
        starts[0].0,
        Some(NewWorktree {
            base: "origin/feat/limit".into(),
            branch: "review/factory-4".into(),
        })
    );
    assert!(
        flat_text(&starts[0].1).contains("on the head of its pull request"),
        "{}",
        starts[0].1
    );
}

#[tokio::test]
async fn a_no_code_review_starts_without_starving_and_names_the_base_in_its_brief() {
    // Subcase A: explicit `pr: none` starts right away on the default base without waiting,
    // even when an earlier note cites a PR URL.
    let task_none = review_task("factory-4", 0);
    let (_dir, mut e) = git_review_env(&[], std::slice::from_ref(&task_none));
    let worker_note = task_note(
        1,
        &task_none,
        WORKER,
        "note",
        &format!("Superseded by {PR}; no code changes here.\npr: none"),
    );
    review_detail(&e, &task_none, vec![worker_note]);

    let (buf, guard) = capture_warns();
    e.daemon.reconcile(at()).await.unwrap();
    drop(guard);
    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("factory-4") && logs.contains("origin/trunk"),
        "expected warning naming task and base, got: {logs:?}"
    );
    let starts = started_reviews(&e.t3);
    assert_eq!(starts.len(), 1);
    assert_eq!(
        starts[0].0,
        Some(NewWorktree {
            base: "origin/trunk".into(),
            branch: "review/factory-4".into(),
        })
    );
    let brief = flat_text(&starts[0].1);
    assert!(
        brief.contains("origin/trunk") && !brief.contains("on the head of its pull request"),
        "brief must name base instead of PR head: {brief}"
    );

    // Subcase B: no `pr: none`, no PR URL, no worker branch on origin -> waits up to the
    // bounded wait, then starts so the task is never starved.
    let task_plain = review_task("factory-5", 0);
    let (_dir2, mut e2) = git_review_env(&[], std::slice::from_ref(&task_plain));
    review_detail(&e2, &task_plain, vec![]);

    e2.daemon.reconcile(at()).await.unwrap();
    e2.daemon
        .reconcile(at() + board::daemon::REVIEW_PR_WAIT - chrono::Duration::seconds(1))
        .await
        .unwrap();
    assert!(
        started_reviews(&e2.t3).is_empty(),
        "still within bounded wait"
    );

    let (buf2, guard2) = capture_warns();
    e2.daemon
        .reconcile(at() + board::daemon::REVIEW_PR_WAIT)
        .await
        .unwrap();
    drop(guard2);
    let logs2 = String::from_utf8(buf2.lock().unwrap().clone()).unwrap();
    assert!(
        logs2.contains("factory-5") && logs2.contains("origin/trunk"),
        "expected warning after bounded wait, got: {logs2:?}"
    );
    let starts2 = started_reviews(&e2.t3);
    assert_eq!(starts2.len(), 1);
    assert_eq!(
        starts2[0].0,
        Some(NewWorktree {
            base: "origin/trunk".into(),
            branch: "review/factory-5".into(),
        })
    );
    let brief2 = flat_text(&starts2[0].1);
    assert!(
        brief2.contains("origin/trunk") && !brief2.contains("on the head of its pull request"),
        "brief must name base instead of PR head: {brief2}"
    );
}

#[tokio::test]
async fn an_unresolvable_pr_warns_and_names_the_default_base_in_the_judges_brief() {
    for (case, setup_pr) in [
        ("gh_view_error", None),
        (
            "fork_pr",
            Some({
                let mut pr = open_pr(MergeState::Clean, Checks::Passed);
                pr.cross_repository = true;
                pr
            }),
        ),
        (
            "missing_remote_head",
            Some({
                let mut pr = open_pr(MergeState::Clean, Checks::Passed);
                pr.head_ref = "feat/deleted-on-origin".into();
                pr
            }),
        ),
    ] {
        let task = review_task("factory-4", 0);
        let (_dir, mut e) = git_review_env(&[], std::slice::from_ref(&task));
        let parked = task_note(
            1,
            &task,
            WORKER,
            "note",
            &format!("note: parked in review, PR {PR}"),
        );
        review_detail(&e, &task, vec![parked]);
        if let Some(pr) = setup_pr {
            e.t3.gh.put(pr);
        }

        let (buf, guard) = capture_warns();
        e.daemon.reconcile(at()).await.unwrap();
        drop(guard);
        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains("factory-4") && logs.contains("origin/trunk"),
            "{case}: expected warning naming task and base, got: {logs:?}"
        );
        let starts = started_reviews(&e.t3);
        assert_eq!(starts.len(), 1, "{case}");
        assert_eq!(
            starts[0].0,
            Some(NewWorktree {
                base: "origin/trunk".into(),
                branch: "review/factory-4".into(),
            }),
            "{case}"
        );
        let brief = flat_text(&starts[0].1);
        assert!(
            brief.contains("origin/trunk") && !brief.contains("on the head of its pull request"),
            "{case}: brief must name default base instead of PR head: {brief}"
        );
    }
}

#[tokio::test]
async fn a_recovered_review_judge_start_keeps_its_worktree_note_after_reload() {
    let task = review_task("factory-4", 0);
    let (_dir, mut e) = git_review_env(&["task/factory-4"], std::slice::from_ref(&task));
    let started = task_note(
        1,
        &task,
        DISPATCH,
        "note",
        "note: the dispatcher started a worker for this task on branch task/factory-4: https://t3.example.test/threads/w1",
    );
    review_detail(&e, &task, vec![started]);

    // T3 created the thread without delivering the brief (`ran == false`), then start failed.
    *e.t3.clock.lock().unwrap() = at();
    *e.t3.die_after_create.lock().unwrap() = Some(false);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(registered(&e).is_empty());

    // Confirm `worktree_note` survived watch.json persistence.
    let reloaded = saved_state(&e);
    assert_eq!(reloaded.judges.len(), 1);
    let saved_note = reloaded.judges[0]
        .worktree_note
        .as_deref()
        .expect("worktree_note persisted in watch.json");
    assert!(saved_note.contains("origin/task/factory-4"), "{saved_note}");

    // Reload state into the daemon and recover the unbriefed thread.
    e.daemon.state = reloaded;
    *e.t3.die_after_create.lock().unwrap() = None;
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, ORPHAN);
    let resent = flat_text(&sends[0].1);
    assert!(
        resent.contains("origin/task/factory-4")
            && !resent.contains("on the head of its pull request"),
        "recovered brief must keep worktree_note: {resent}"
    );
}

#[tokio::test]
async fn per_kind_judge_models_select_each_kind_and_warn_on_reload() {
    let worker_model = ThreadModel {
        provider: "antigravity".into(),
        model: "gemini-4-neon".into(),
        options: [("thought_level".into(), "high".into())].into(),
    };
    let fallback_judge = ThreadModel {
        provider: "antigravity".into(),
        model: "gemini-4-neon".into(),
        options: [("thought_level".into(), "medium".into())].into(),
    };
    let verdict_model = ThreadModel {
        provider: "antigravity".into(),
        model: "gemini-4-neon".into(),
        options: [("thought_level".into(), "low".into())].into(),
    };
    let goal_model = ThreadModel {
        provider: "antigravity".into(),
        model: "gemini-4-neon".into(),
        options: [("thought_level".into(), "high".into())].into(),
    };

    // Phase 1: pending proposal starts a verdict judge using [judge_models.verdict], and
    // rewriting config.toml warns to restart while keeping the original selection active.
    let tasks = [proposal("factory-12", None, 0)];
    let mut e = env_with_models(
        &tasks,
        Some(worker_model.clone()),
        Some(fallback_judge.clone()),
        JudgeModels {
            verdict: Some(verdict_model.clone()),
            goal: Some(goal_model.clone()),
            ..JudgeModels::default()
        },
    );
    let mut board = autopilot(Some(DISPATCH), &[DEVICE]);
    board.goal = "Keep latency low.".into();
    e.stub.on("GET /v1/boards", [&board]);
    e.stub.on("POST /v1/boards/factory/coordinator", &board);

    let home = Home::at(e._dir.path().join("home"));
    Config {
        server: Some(e.stub.url.clone()),
        device: Some(DEVICE.parse().unwrap()),
        projects: projects(),
        thread_model: Some(worker_model),
        judge_model: Some(fallback_judge.clone()),
        judge_models: JudgeModels {
            verdict: Some(goal_model.clone()),
            goal: Some(goal_model.clone()),
            ..JudgeModels::default()
        },
        ..Config::default()
    }
    .save(&home)
    .unwrap();

    let buf = LogBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(buf.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    e.daemon.reconcile(at()).await.unwrap();
    drop(_guard);

    let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("judge_models") && logs.contains("restart board watch"),
        "expected restart warning for judge_models change, got: {logs:?}"
    );

    // Phase 2: once the board rests with a done task, goal review uses [judge_models.goal].
    e.stub.on("GET /v1/tasks", json!([]));
    e.stub.on("GET /v1/boards/factory/judges", json!([]));
    e.t3.threads.lock().unwrap().clear();
    e.t3.ids.lock().unwrap().push_back(J2.to_owned());
    let mut done = detail("factory-11", false, &[]);
    done.task.status = TaskStatus::Done;
    e.stub.on("GET /v1/tasks/factory-11", done);
    e.daemon
        .on_event(
            &event(40, EventType::TaskDone, Some("factory-11"), WORKER),
            at() + chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(2))
        .await
        .unwrap();

    assert_eq!(
        started_models(&e.t3),
        [
            ("verdict factory-12 (no parent)".into(), Some(verdict_model)),
            ("goal review factory".into(), Some(goal_model.clone())),
        ]
    );

    // Phase 3: an unset kind in [judge_models] falls back to [judge_model].
    let mut e2 = env_with_models(
        &tasks,
        None,
        Some(fallback_judge.clone()),
        JudgeModels {
            goal: Some(goal_model),
            ..JudgeModels::default()
        },
    );
    e2.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        started_models(&e2.t3),
        [(
            "verdict factory-12 (no parent)".into(),
            Some(fallback_judge)
        )]
    );
}

#[tokio::test]
async fn daemon_with_non_t3_runner_owns_git_worktree_lifecycle_and_safe_cleanup() {
    use board::runner::{FakeRunner, RunEventKind, RunnerState};
    use board::worktree::WorktreeStatus;

    let git_dir = tempfile::tempdir().unwrap();
    let origin = git_dir.path().join("origin");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &origin,
        &["commit", "--quiet", "--allow-empty", "-m", "init"],
    );
    git(git_dir.path(), &["clone", "--quiet", "origin", "repo"]);
    let repo = git_dir.path().join("repo");

    let mut e = env_with_projects([(REPO.to_owned(), repo.clone())].into());
    let home = Home::at(e._dir.path().join("home"));
    let fake = FakeRunner::new("acp")
        .with_home(home.clone())
        .with_clock(std::sync::Arc::new(at));
    e.daemon = e.daemon.with_runner(fake.clone());
    assert_eq!(e.daemon.runner_kind(), "acp");

    let t1 = ready_task("factory-1", board_proto::Priority::P1, 0);
    let t2 = ready_task("factory-2", board_proto::Priority::P2, 1);
    e.stub.on("GET /v1/boards", [daemon_board(None)]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        daemon_board(Some(DISPATCH)),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    e.stub.on("GET /v1/tasks", [t1.clone(), t2.clone()]);
    e.stub
        .on("GET /v1/tasks/factory-1", task_detail_for(t1.clone()));
    e.stub
        .on("GET /v1/tasks/factory-2", task_detail_for(t2.clone()));
    e.stub.on("POST /v1/tasks/factory-1/notes", note(&t1.id));
    e.stub.on("POST /v1/tasks/factory-2/notes", note(&t2.id));
    judges_at_work(&e, &[]);

    // Reconcile 1: starts worker for `factory-1` in a daemon-owned worktree with zero T3 calls.
    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        e.t3.calls().is_empty(),
        "non-T3 runner must not invoke T3 CLI: {:?}",
        e.t3.calls()
    );
    let spawns = fake.spawns();
    assert_eq!(spawns.len(), 1);
    assert_eq!(
        spawns[0].spec.worktree, None,
        "daemon created worktree directly instead of delegating --worktree"
    );
    assert!(
        spawns[0].workdir.starts_with(home.root().join("worktrees")),
        "workdir must be under $BOARD_HOME/worktrees: {}",
        spawns[0].workdir.display()
    );
    assert!(spawns[0].workdir.exists());
    let wt1_path = spawns[0].workdir.clone();
    let wt1_id = e.daemon.state.workers[&t1.id]
        .worktree_id
        .clone()
        .expect("worktree_id tracked on worker");
    assert_eq!(
        e.daemon.worktrees().records().unwrap().len(),
        1,
        "WorktreeManager tracked the active worktree"
    );

    // Poll events and cancel via Daemon's Runner boundary.
    let events = e
        .daemon
        .poll_run_events(&spawns[0].handle, 0)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, RunEventKind::Phase);

    e.daemon
        .cancel_run(&spawns[0].handle, "preempted for test")
        .await
        .unwrap();
    let cancel_events = e
        .daemon
        .poll_run_events(&spawns[0].handle, 1)
        .await
        .unwrap();
    assert_eq!(cancel_events.len(), 1);
    assert_eq!(cancel_events[0].kind, RunEventKind::Cancelled);
    assert_eq!(fake.cancels()[0].reason, "preempted for test");
    assert_eq!(
        board::runner::Runner::status(&fake, &spawns[0].handle)
            .await
            .unwrap()
            .state,
        RunnerState::Cancelled
    );
    assert!(home.state_dir().join("runner-cancels.jsonl").exists());

    // Mark `factory-1` done so the worker frees its slot and `Daemon` cleans up the clean worktree.
    let mut done_t1 = detail("factory-1", false, &[]);
    done_t1.task.status = TaskStatus::Done;
    e.stub.on("GET /v1/tasks/factory-1", done_t1);
    e.stub.on("GET /v1/tasks", [t2.clone()]);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert!(
        !wt1_path.exists(),
        "clean worktree must be removed when worker ends"
    );
    assert_eq!(
        e.daemon.worktrees().store().get(&wt1_id).unwrap(),
        None,
        "removed worktree record cleared from store"
    );

    // Second worker (`factory-2`) started; dirty its worktree before it ends -> MUST NOT delete.
    let spawns = fake.spawns();
    assert_eq!(spawns.len(), 2);
    let wt2_path = spawns[1].workdir.clone();
    let wt2_id = e.daemon.state.workers[&t2.id]
        .worktree_id
        .clone()
        .expect("second worktree_id tracked");
    std::fs::write(wt2_path.join("uncommitted.txt"), "keep me").unwrap();

    let mut done_t2 = detail("factory-2", false, &[]);
    done_t2.task.status = TaskStatus::Done;
    e.stub.on("GET /v1/tasks/factory-2", done_t2);
    e.stub.on("GET /v1/tasks", json!([]));
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(2))
        .await
        .unwrap();
    assert!(
        wt2_path.join("uncommitted.txt").exists(),
        "dirty worktree must never be deleted silently"
    );
    let rec2 = e
        .daemon
        .worktrees()
        .store()
        .get(&wt2_id)
        .unwrap()
        .expect("dirty skipped record kept");
    assert_eq!(rec2.status, WorktreeStatus::DirtySkipped);
    let trail = std::fs::read_to_string(home.state_dir().join("worktree-events.jsonl")).unwrap();
    assert!(trail.contains("cleanup_skipped_dirty"), "{trail}");
    assert!(trail.contains("uncommitted.txt"), "{trail}");
}

#[tokio::test]
async fn daemon_with_non_t3_runner_creates_and_cleans_up_review_judge_worktree() {
    use board::runner::FakeRunner;

    let task = review_task("factory-4", 0);
    let (_dir, mut e) = git_review_env(&["feat/limit"], std::slice::from_ref(&task));
    let home = Home::at(e._dir.path().join("home"));
    let fake = FakeRunner::new("acp")
        .with_home(home.clone())
        .with_clock(std::sync::Arc::new(at));
    e.daemon = e.daemon.with_runner(fake.clone());

    let parked = task_note(
        1,
        &task,
        WORKER,
        "note",
        &format!("note: parked in review, PR {PR}"),
    );
    review_detail(&e, &task, vec![parked]);
    e.t3.gh.put(open_pr(MergeState::Clean, Checks::Pending));
    e.stub.on(
        "POST /v1/boards/factory/judges",
        json!({
            "board": "factory",
            "agent": "antigravity:acp-run-1",
            "kind": "review",
            "tasks": ["factory-4"],
            "registered_by": DISPATCH,
            "created_at": at(),
            "expires_at": at() + chrono::Duration::hours(2),
            "running": true,
        }),
    );

    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());
    let spawns = fake.spawns();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].spec.worktree, None);
    let wt_path = spawns[0].workdir.clone();
    assert!(wt_path.starts_with(home.root().join("worktrees")));
    assert!(wt_path.exists());

    // Judge finishes: server no longer lists it as running -> daemon cleans up clean review worktree.
    judges_at_work(&e, &[]);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert!(
        !wt_path.exists(),
        "clean review judge worktree must be removed when judge ends"
    );
    assert!(e.daemon.worktrees().records().unwrap().is_empty());
}

#[tokio::test]
async fn non_t3_runner_wakes_its_own_agents_verifies_unsure_wakes_and_ignores_t3_agents() {
    use board::runner::{FakeRunner, RunnerState, RunnerStatus};

    let mut e = env();
    let home = Home::at(e._dir.path().join("home"));
    let fake = FakeRunner::new("acp")
        .with_home(home)
        .with_clock(std::sync::Arc::new(at));
    e.daemon = e.daemon.with_runner(fake.clone());

    let acp_agent = "antigravity:acp-run-1";
    let t3_agent = "t3:interactive-1";
    fake.set_status(
        "acp-run-1",
        RunnerStatus {
            state: RunnerState::Idle,
            runnable: true,
            busy: false,
            runtime: Some(Runtime::Antigravity),
            workdir: None,
            runs: Some(1),
        },
    );

    e.stub.on("GET /v1/boards", json!([]));
    e.stub.on(
        &format!("GET {}", agent_path(acp_agent)),
        agent_fixture(acp_agent, DEVICE, AgentState::Idle),
    );
    e.stub.on(
        &format!("GET {}", agent_path(t3_agent)),
        agent_fixture(t3_agent, DEVICE, AgentState::Idle),
    );
    e.daemon.reconcile(at()).await.unwrap();

    // Queue an answered ask for the acp runner's agent and an interactive T3 agent.
    let ev_acp = answered(&e, 1, 10, acp_agent);
    let ev_t3 = answered(&e, 2, 11, t3_agent);
    e.daemon.on_event(&ev_acp, at()).await.unwrap();
    e.daemon.on_event(&ev_t3, at()).await.unwrap();

    // Only the acp agent is queued; the interactive T3 agent must NOT be routed to FakeRunner.
    assert!(
        e.daemon
            .state
            .wakes
            .contains_key(&acp_agent.parse().unwrap())
    );
    assert!(
        !e.daemon
            .state
            .wakes
            .contains_key(&t3_agent.parse().unwrap())
    );

    // First send times out after delivering: recorded in unsure_wakes and verified via
    // FakeRunner::poll_events + status (ZERO calls to T3!).
    fake.set_time_out_send(Some(true));
    e.daemon.flush(at()).await;
    fake.set_time_out_send(None);
    assert_eq!(fake.sends().len(), 1);
    assert!(unsure_wakes(&e, acp_agent).is_some());
    assert!(e.t3.calls().is_empty());

    // After SEND_CHECK_AFTER, flush checks via FakeRunner::poll_events, finds the delivered
    // wake marker, settles unsure_wakes, and does NOT resend or call T3.
    e.daemon.flush(minute(1)).await;
    assert_eq!(fake.sends().len(), 1, "must not resend delivered wake");
    assert_eq!(unsure_wakes(&e, acp_agent), None);
    assert!(e.t3.calls().is_empty());
    assert_eq!(e.t3.message_reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn legacy_watch_state_workers_without_runner_field_still_resolve_as_t3() {
    use board::runner::FakeRunner;

    let mut e = dispatch_env(&[]);
    let home = Home::at(e._dir.path().join("home"));
    let fake = FakeRunner::new("acp")
        .with_home(home)
        .with_clock(std::sync::Arc::new(at));
    e.daemon = e.daemon.with_runner(fake);

    // Simulate an in-flight legacy watch.json worker (runner: None) while switching to `acp`.
    let task_id: board_proto::TaskId = "factory-9".parse().unwrap();
    e.daemon.state.workers.insert(
        task_id.clone(),
        board::daemon::Worker {
            thread: Some("legacy-thread-1".to_owned()),
            title: "factory-9 Legacy".to_owned(),
            branch: Some("task/factory-9".to_owned()),
            runner: None,
            agent_id: None,
            worktree_id: None,
            worktree_path: None,
            run_id: None,
            cost_units: None,
            started_at: at(),
            claimed: false,
            noted: true,
        },
    );
    let mut doing = detail("factory-9", false, &[]);
    doing.task.status = TaskStatus::Doing;
    doing.task.owner = Some("t3:legacy-thread-1".parse().unwrap());
    e.stub.on("GET /v1/tasks/factory-9", doing);
    judges_at_work(&e, &[]);

    e.daemon.reconcile(at()).await.unwrap();
    let worker = e
        .daemon
        .state
        .workers
        .get(&task_id)
        .expect("legacy t3 worker still holds its slot");
    assert!(
        worker.claimed,
        "legacy runner: None worker must resolve to t3:legacy-thread-1 and mark claimed"
    );
}

fn read_daemon_drops(e: &Env) -> Vec<serde_json::Value> {
    let path = home_of(e).state_dir().join("daemon-drops.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn watch_json_and_worktrees_json_migrate_once_into_daemon_db_preserving_state() {
    use board::daemon::{Wake, WakeAbout, WakeQueue, WatchState};
    use board::worktree::{WorktreeRecord, WorktreeStatus};

    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let t3 = FakeT3::default();
    let home = Home::at(dir.path().join("home"));
    std::fs::create_dir_all(home.state_dir()).unwrap();

    let agent: board_proto::AgentId = "t3:worker-mig".parse().unwrap();
    let mut legacy = WatchState {
        last_seq: Some(77),
        ..WatchState::default()
    };
    legacy.wakes.insert(
        agent.clone(),
        WakeQueue {
            wakes: vec![Wake {
                board: factory(),
                about: WakeAbout::Answered {
                    ask: 12,
                    asker: agent.clone(),
                },
                queued_at: at(),
                forwarded: false,
                resent: false,
            }],
            backoff: None,
        },
    );
    board::state::save_named(&home, "watch", &legacy).unwrap();
    board::state::save_named(
        &home,
        "worktrees",
        &json!({
            "records": {
                "org-repo/run-mig": WorktreeRecord {
                    id: "org-repo/run-mig".into(),
                    repo_slug: "org-repo".into(),
                    repo_root: src("agent-board"),
                    path: dir.path().join("wt-mig"),
                    branch: "task/factory-77".into(),
                    base: "origin/main".into(),
                    status: WorktreeStatus::Active,
                    created_at: at(),
                    last_used_at: at(),
                    cleanup_note: None,
                }
            }
        }),
    )
    .unwrap();

    // First startup migrates both legacy JSON files into daemon.db and renames them.
    let mut daemon = daemon_at(dir.path(), &stub, projects(), &t3);
    assert!(home.state_dir().join("daemon.db").exists());
    assert!(!home.state_dir().join("watch.json").exists());
    assert!(home.state_dir().join("watch.json.migrated").exists());
    assert!(!home.state_dir().join("worktrees.json").exists());
    assert!(home.state_dir().join("worktrees.json.migrated").exists());
    assert_eq!(daemon.state.last_seq, Some(77));
    assert_eq!(daemon.state.wakes[&agent].wakes.len(), 1);
    let wts = daemon.worktrees().records().unwrap();
    assert_eq!(wts.len(), 1);
    assert_eq!(wts[0].id, "org-repo/run-mig");

    // Advance state in daemon.db; restarting does not re-import watch.json.migrated.
    daemon.state.last_seq = Some(88);
    daemon.save().unwrap();
    let restarted = daemon_at(dir.path(), &stub, projects(), &t3);
    assert_eq!(restarted.state.last_seq, Some(88));
}

#[tokio::test]
async fn crash_mid_wake_send_persists_unsure_wake_before_send_and_never_sends_twice() {
    let (mut e, agent) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;

    // Make `t3.send` hang so we can drop the flush future mid-send (simulating SIGKILL).
    // Post-flush `save()` NEVER executes!
    let native_thread = agent.strip_prefix("t3:").unwrap().to_owned();
    e.t3.hang_send.lock().unwrap().push(native_thread.clone());
    {
        let flush_fut = e.daemon.flush(at());
        let timed = tokio::time::timeout(Duration::from_millis(80), flush_fut).await;
        assert!(timed.is_err(), "flush must hang inside t3.send");
    }
    assert_eq!(
        e.t3.sends().len(),
        1,
        "send started before simulated hard kill"
    );

    // Without ever calling save() after the aborted flush, daemon.db ALREADY contains the
    // unsure digest and removed the wake from sticky_wakes!
    let persisted = saved_state(&e);
    let agent_id: board_proto::AgentId = agent.parse().unwrap();
    assert!(
        persisted.wakes.is_empty(),
        "wake removed from sticky_wakes before send"
    );
    let unsure = persisted
        .unsure_wakes
        .get(&agent_id)
        .expect("unsure wake committed to daemon.db BEFORE send_wake");
    assert_eq!(unsure.messages.len(), 1);
    let digest = unsure.messages[0].digest.clone();

    // Simulate that T3 actually delivered the message before our process died.
    e.t3.hang_send.lock().unwrap().clear();
    e.t3.received.lock().unwrap().insert(
        native_thread,
        vec![format!(
            "delivered before kill\n{}",
            board::daemon::wake_marker(&digest)
        )],
    );

    // Restart a fresh daemon on the same home: it loads unsure_wakes from daemon.db,
    // sees the delivered marker after SEND_CHECK_AFTER, settles it, and NEVER sends twice.
    let mut restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    restarted.reconcile(minute(1)).await.unwrap();
    restarted.flush(minute(1)).await;
    assert_eq!(
        e.t3.sends().len(),
        1,
        "restarted daemon must NOT send wake-up a second time"
    );
    assert!(restarted.state.unsure_wakes.is_empty());
}

#[tokio::test]
async fn crash_between_worktree_creation_and_spawn_leaves_durable_local_run_naming_branch() {
    use board::local_db::LocalRunState;

    let git_dir = tempfile::tempdir().unwrap();
    let origin = git_dir.path().join("origin");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &origin,
        &["commit", "--quiet", "--allow-empty", "-m", "init"],
    );
    git(git_dir.path(), &["clone", "--quiet", "origin", "repo"]);
    let repo = git_dir.path().join("repo");

    let mut e = env_with_projects([(REPO.to_owned(), repo.clone())].into());
    let t1 = ready_task("factory-4", board_proto::Priority::P1, 0);
    e.stub.on("GET /v1/boards", [daemon_board(None)]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        daemon_board(Some(DISPATCH)),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    e.stub
        .on("GET /v1/tasks/factory-4", task_detail_for(t1.clone()));
    e.stub.on("GET /v1/tasks", [t1]);
    judges_at_work(&e, &[]);

    // Hang inside `t3.start_thread` (after worker_worktree resolved branch `task/factory-4`)
    // and drop the reconcile future (simulating SIGKILL mid-spawn).
    e.t3.hang_start_in.lock().unwrap().push(repo.clone());
    let waited = tokio::select! {
        _ = e.daemon.reconcile(at()) => false,
        () = e.t3.start_hung.notified() => true,
    };
    assert!(waited, "reconcile reached runner.spawn and hung");

    // Reopen daemon.db from a fresh daemon instance (no post-reconcile save() ran!).
    let restarted = daemon_at(
        e._dir.path(),
        &e.stub,
        [(REPO.to_owned(), repo)].into(),
        &e.t3,
    );
    let task_id: board_proto::TaskId = "factory-4".parse().unwrap();
    let worker = restarted
        .state
        .workers
        .get(&task_id)
        .expect("worker record persisted BEFORE spawn");
    assert_eq!(worker.branch.as_deref(), Some("task/factory-4"));
    assert_eq!(worker.thread, None);

    let runs = restarted.db().list_active_local_runs().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].task_id.as_ref(), Some(&task_id));
    assert_eq!(runs[0].branch.as_deref(), Some("task/factory-4"));
    assert_eq!(runs[0].state, LocalRunState::Starting);
    assert_eq!(runs[0].session_id, None);
}

#[tokio::test]
async fn crash_mid_owed_step_persists_noted_before_release_without_duplicate_note() {
    let mut e = dispatch_env(&[]);
    let ask_id = 55;
    let task_id: board_proto::TaskId = "factory-7".parse().unwrap();
    e.stub
        .on("GET /v1/asks/55", answered_ask(ask_id, "claude:gone"));
    let mut blocked = detail("factory-7", false, &[]);
    blocked.task.status = TaskStatus::Blocked;
    blocked.task.owner = Some("claude:gone".parse().unwrap());
    e.stub.on("GET /v1/tasks/factory-7", blocked);
    e.stub.on("POST /v1/tasks/factory-7/notes", note(&task_id));
    // First attempt: note succeeds, release fails transiently (503).
    e.stub.on_error(
        "POST /v1/tasks/factory-7/release",
        503,
        "internal",
        "restarting",
        "retry",
    );
    e.daemon.state.owed.push(board::daemon::Owed::Answer {
        board: factory(),
        ask: ask_id,
        asker: "claude:gone".parse().unwrap(),
        check_gone: false,
        noted: false,
        at: at(),
    });
    e.daemon.save().unwrap();
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-7/notes").len(),
        1
    );

    // Restart from daemon.db: `noted = true` was persisted BEFORE release, so retrying
    // releases the task WITHOUT posting a duplicate decision note!
    let mut opened = detail("factory-7", false, &[]).task;
    opened.status = TaskStatus::Open;
    opened.owner = None;
    e.stub.on("POST /v1/tasks/factory-7/release", opened);

    let mut restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    restarted.reconcile(minute(1)).await.unwrap();
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-7/notes").len(),
        1,
        "decision note must NOT be posted twice after crash between note and release"
    );
    assert_eq!(
        e.stub.requests_to("POST /v1/tasks/factory-7/release").len(),
        2
    );
    assert!(restarted.state.owed.is_empty());
}

#[tokio::test]
async fn trail_rule_records_daemon_drops_give_backs_refusals_and_usage_holds() {
    use board::daemon::WAKE_TTL;

    // 1. Expired wake-up, expired unsure wake, and non-transient lookup failure -> daemon-drops.jsonl.
    let (mut e, agent) = owner_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_unblocked(&mut e, 10, &["factory-1"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    // Queue another wake and advance past WAKE_TTL so both unsure and queued wakes expire.
    queue_unblocked(&mut e, 11, &["factory-2"], at()).await;
    e.daemon
        .flush(at() + WAKE_TTL + chrono::Duration::minutes(1))
        .await;

    // Non-transient lookup failure (400).
    let bad_agent = "t3:bad-agent";
    e.stub.on_error(
        &format!("GET {}", agent_path(bad_agent)),
        400,
        "invalid",
        "bad agent",
        "fix",
    );
    e.daemon.state.wakes.insert(
        bad_agent.parse().unwrap(),
        board::daemon::WakeQueue {
            wakes: vec![board::daemon::Wake {
                board: factory(),
                about: board::daemon::WakeAbout::Answered {
                    ask: 1,
                    asker: bad_agent.parse().unwrap(),
                },
                queued_at: at(),
                forwarded: false,
                resent: false,
            }],
            backoff: None,
        },
    );
    e.daemon.flush(at()).await;

    // Expired owed step + reconcile-dropped owed step for gone board.
    e.daemon.state.owed.push(board::daemon::Owed::Answer {
        board: "vanished".parse().unwrap(),
        ask: 99,
        asker: "claude:old".parse().unwrap(),
        check_gone: false,
        noted: false,
        at: at(),
    });
    e.daemon.reconcile(at()).await.unwrap();

    // Non-transient event error (400 on GET /v1/tasks/factory-99).
    e.stub.on_error(
        "GET /v1/tasks/factory-99",
        400,
        "invalid",
        "bad task",
        "fix",
    );
    e.daemon
        .state
        .retry
        .push(event(99, EventType::TaskDone, Some("factory-99"), "user"));
    e.daemon.retry_failed(at()).await;

    // Refused coordinator claim + usage hold.
    let mut e2 = env();
    let board = autopilot(Some("dispatch:mac-other"), &[DEVICE]);
    e2.stub.on("GET /v1/boards", [board]);
    e2.stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    e2.stub.on_error(
        "POST /v1/boards/factory/coordinator",
        409,
        "claim_refused",
        "another device holds it",
        "wait",
    );
    e2.daemon.reconcile(at()).await.unwrap();

    let drops = read_daemon_drops(&e);
    let kinds: Vec<&str> = drops.iter().filter_map(|d| d["kind"].as_str()).collect();
    for expected in [
        "unsure_wake_expired",
        "wake_expired",
        "wake_lookup_failed",
        "reconcile_dropped_owed",
        "event_non_transient",
    ] {
        assert!(
            kinds.contains(&expected),
            "expected trail drop {expected:?} in {kinds:?} (all: {drops:#?})"
        );
    }
    let drops2 = read_daemon_drops(&e2);
    assert!(
        drops2.iter().any(|d| d["kind"] == "claim_refused"),
        "expected claim_refused trail drop in {drops2:#?}"
    );
    let _ = agent;
}

#[tokio::test]
async fn weighted_resource_ledger_enforces_heavy_standard_light_and_interactive_units() {
    use board::local_db::ResourceOrigin;
    use board_proto::{Priority::P1, ResourceWeight};

    let mut heavy = ready_task("factory-101", P1, 0);
    heavy.weight = Some(ResourceWeight::Heavy); // 4 units
    let mut light = ready_task("factory-102", P1, 1);
    light.weight = Some(ResourceWeight::Light); // 1 unit

    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let t3 = FakeT3::default();
    // 6 total units, reserve_runs = 1 (2 units reserved) => 4 daemon units available.
    // With 1 interactive thread (2 units), only 2 free units remain:
    // heavy (4 units) MUST be held while light (1 unit) fits!
    let config = Config {
        server: Some(stub.url.clone()),
        device: Some(DEVICE.parse().unwrap()),
        weight_units: Some(6),
        reserve_runs: Some(1),
        projects: projects(),
        ..Config::default()
    };
    let mut daemon = daemon_configured(dir.path(), &stub, &t3, None, config);
    let mut b = daemon_board(Some(DISPATCH));
    b.parallel_tasks = 4;
    stub.on("GET /v1/boards", [b.clone()]);
    stub.on("POST /v1/boards/factory/coordinator", b);
    stub.on(
        "POST /v1/agents",
        agent_fixture(DISPATCH, DEVICE, AgentState::Running),
    );
    stub.on("GET /v1/tasks", [heavy.clone(), light.clone()]);
    stub.on("GET /v1/tasks/factory-101", task_detail_for(heavy.clone()));
    stub.on("GET /v1/tasks/factory-102", task_detail_for(light.clone()));
    stub.on("POST /v1/tasks/factory-101/notes", note(&heavy.id));
    stub.on("POST /v1/tasks/factory-102/notes", note(&light.id));
    stub.on(
        "GET /v1/boards/factory/judges",
        Vec::<board_proto::Judge>::new(),
    );
    t3.running.lock().unwrap().push("interactive-1".into());

    // First reconcile: heavy (4 units) is first in priority order and exceeds free_units (2),
    // so it is held.
    daemon.reconcile(at()).await.unwrap();
    assert!(
        worker_starts(&t3).is_empty(),
        "4-unit heavy task must not start when only 2 free units remain"
    );
    let ledger = daemon.db().list_resources().unwrap();
    assert!(
        ledger
            .iter()
            .any(|r| r.origin == ResourceOrigin::Interactive && r.cost_units == 2),
        "interactive turn accounted as 2 units: {ledger:?}"
    );
    assert_eq!(daemon.db().free_units(6, 2).unwrap(), 2);

    // Clear the interactive turn: now 4 free units are available, so heavy (4 units) starts!
    t3.running.lock().unwrap().clear();
    daemon.reconcile(minute(1)).await.unwrap();
    assert_eq!(worker_starts(&t3).len(), 1);
    assert!(daemon.state.workers.contains_key(&heavy.id));
    assert_eq!(daemon.state.workers[&heavy.id].cost_units, Some(4));
    assert_eq!(daemon.db().active_units().unwrap(), 4);
    assert_eq!(daemon.db().free_units(6, 2).unwrap(), 0);

    // Weighted hold record must report exact weighted unit numbers (never usize::MAX!).
    let holds_text =
        std::fs::read_to_string(dir.path().join("home/state/device-holds.jsonl")).unwrap();
    assert!(
        holds_text.contains("weight units active") && holds_text.contains("need 4"),
        "expected weighted unit numbers in device-holds.jsonl: {holds_text}"
    );
    assert!(
        !holds_text.contains(&usize::MAX.to_string()),
        "must never print usize::MAX in hold record: {holds_text}"
    );

    // With heavy (4 units) occupying all 4 daemon units, light (1 unit) is ALSO held.
    assert!(!daemon.state.workers.contains_key(&light.id));

    // Also verify send_heartbeat refreshes interactive turns even without a reconcile:
    // adding an interactive turn reduces free_units, and clearing it restores free_units.
    let hb_route = format!("PUT /v1/devices/{DEVICE}/heartbeat");
    stub.on(
        &hb_route,
        board_proto::DeviceEntry {
            name: DEVICE.parse().unwrap(),
            role: board_proto::DeviceRole::Runner,
            status: board_proto::DeviceStatus::Online,
            drain: false,
            sleepable: false,
            os: None,
            arch: None,
            capacity: None,
            free_units: None,
            capabilities: Vec::new(),
            runners: Vec::new(),
            repos: Vec::new(),
            taints: Vec::new(),
            preference: 0,
            protocol_features: Vec::new(),
            heartbeat_at: Some(at()),
        },
    );
    daemon.state.workers.clear();
    t3.running.lock().unwrap().push("interactive-hb".into());
    assert!(daemon.send_heartbeat(minute(2)).await.unwrap());
    let hb1: board_proto::DeviceHeartbeat =
        serde_json::from_value(stub.requests_to(&hb_route)[0].body.clone()).unwrap();
    assert_eq!(hb1.free_units, Some(2));
    t3.running.lock().unwrap().clear();
    assert!(daemon.send_heartbeat(minute(3)).await.unwrap());
    let hb2: board_proto::DeviceHeartbeat =
        serde_json::from_value(stub.requests_to(&hb_route)[1].body.clone()).unwrap();
    assert_eq!(hb2.free_units, Some(4));
}

#[tokio::test]
async fn periodic_device_heartbeat_reports_capacity_free_units_and_degrades_on_404() {
    use board_proto::{DeviceEntry, DeviceHeartbeat, DeviceRole, DeviceStatus};

    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let t3 = FakeT3::default();
    let config = Config {
        server: Some(stub.url.clone()),
        device: Some(DEVICE.parse().unwrap()),
        weight_units: Some(12),
        reserve_runs: Some(1),
        capabilities: vec!["git:cloneable".into(), "tool:xcode".into()],
        runners: vec!["t3".into()],
        protocol_features: vec!["v1-json".into(), "sse-v1".into()],
        projects: projects(),
        ..Config::default()
    };
    let mut daemon =
        daemon_configured(dir.path(), &stub, &t3, None, config).with_host_memory(Some(48 << 30));

    let hb_route = format!("PUT /v1/devices/{DEVICE}/heartbeat");
    stub.on(
        &hb_route,
        DeviceEntry {
            name: DEVICE.parse().unwrap(),
            role: DeviceRole::Runner,
            status: DeviceStatus::Online,
            drain: false,
            sleepable: false,
            os: Some(board::daemon::device::host_os().into()),
            arch: Some(board::daemon::device::host_arch().into()),
            capacity: None,
            free_units: Some(10),
            capabilities: Vec::new(),
            runners: Vec::new(),
            repos: Vec::new(),
            taints: Vec::new(),
            preference: 0,
            protocol_features: Vec::new(),
            heartbeat_at: Some(at()),
        },
    );

    // Direct heartbeat sends full capacity, free_units (12 - 2 reserve = 10), capabilities,
    // runners, normalized repos, and protocol features.
    assert!(daemon.send_heartbeat(at()).await.unwrap());
    let reqs = stub.requests_to(&hb_route);
    assert_eq!(reqs.len(), 1);
    let sent: DeviceHeartbeat = serde_json::from_value(reqs[0].body.clone()).unwrap();
    assert_eq!(sent.os.as_deref(), Some(board::daemon::device::host_os()));
    assert_eq!(
        sent.arch.as_deref(),
        Some(board::daemon::device::host_arch())
    );
    let cap = sent.capacity.expect("capacity present");
    assert_eq!(cap.weight_units, 12);
    assert_eq!(cap.mem_gib, Some(48));
    assert!(cap.cpu_cores.is_some());
    assert_eq!(sent.free_units, Some(10));
    assert!(sent.capabilities.contains(&"git:cloneable".to_owned()));
    assert!(sent.capabilities.contains(&"tool:xcode".to_owned()));
    assert!(
        sent.capabilities
            .contains(&format!("os:{}", board::daemon::device::host_os()))
    );
    assert_eq!(sent.runners, vec!["t3"]);
    assert_eq!(sent.repos, vec![REPO]);
    assert_eq!(sent.protocol_features, vec!["v1-json", "sse-v1"]);

    // Periodic heartbeats fire inside `Daemon::follow()` according to `timing.heartbeat_every`.
    stub.on("GET /v1/boards", Vec::<Board>::new());
    stub.on("GET /v1/events", "");
    daemon.timing.heartbeat_every = Duration::from_millis(50);
    daemon.timing.reconcile_every = Duration::from_secs(60);
    daemon.timing.flush_every = Duration::from_secs(60);
    let (stopper, mut shutdown) = board::daemon::Shutdown::manual();
    let stub_ref = stub.clone();
    let hb_route_wait = hb_route.clone();
    let wait_for_hbs = async move {
        let deadline = Instant::now() + Duration::from_secs(5);
        while stub_ref.requests_to(&hb_route_wait).len() < 3 {
            assert!(Instant::now() < deadline, "periodic heartbeats never fired");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stopper.stop();
    };
    let (res, ()) = tokio::join!(daemon.follow(&mut shutdown), wait_for_hbs);
    res.unwrap();
    assert!(stub.requests_to(&hb_route).len() >= 3);

    // Older server returning 404 degrades gracefully: disables further heartbeats without
    // returning an error or spamming repeated 404 requests.
    let dir2 = tempfile::tempdir().unwrap();
    let stub2 = Stub::start();
    let mut old_daemon = daemon_at(dir2.path(), &stub2, projects(), &t3);
    stub2.on_error(&hb_route, 404, "not_found", "no such route", "upgrade");
    assert!(!old_daemon.send_heartbeat(at()).await.unwrap());
    assert!(!old_daemon.send_heartbeat(minute(1)).await.unwrap());
    assert_eq!(
        stub2.requests_to(&hb_route).len(),
        1,
        "must stop calling heartbeat after first 404"
    );
}

fn task_detail_for(task: board_proto::Task) -> TaskDetail {
    TaskDetail {
        task,
        notes: vec![],
        evidence: vec![],
        blocks: vec![],
        children: vec![],
        child_tasks: vec![],
        history: vec![],
        handles: Default::default(),
    }
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn daemon_drops(home: &Home) -> Vec<serde_json::Value> {
    let path = home.state_dir().join("daemon-drops.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn a_task_claimed_or_closed_after_next_tasks_gets_no_worker_worktree_or_branch() {
    use board::runner::FakeRunner;
    use board_proto::Priority::P1;

    let init_repo = || -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "--quiet", "--initial-branch=main"]);
        git(
            &origin,
            &["commit", "--quiet", "--allow-empty", "-m", "init"],
        );
        git(dir.path(), &["clone", "--quiet", "origin", "repo"]);
        let repo = dir.path().join("repo");
        (dir, repo)
    };

    // 1. Claimed by another agent between `next_tasks` and `start_worker`: no worker thread,
    //    no worktree, no branch, and on a 1-slot board the newly-doing task occupies the slot
    //    so a second queued task does not start beside it.
    {
        let (_git_dir, repo) = init_repo();
        let mut e = env_with_projects([(REPO.to_owned(), repo.clone())].into());
        let home = Home::at(e._dir.path().join("home"));
        let fake = FakeRunner::new("acp")
            .with_home(home.clone())
            .with_clock(std::sync::Arc::new(at));
        e.daemon = e.daemon.with_runner(fake.clone());

        let t1 = ready_task("factory-1", P1, 0);
        let t2 = ready_task("factory-2", P1, 1);
        let mut t1_claimed = t1.clone();
        t1_claimed.status = TaskStatus::Doing;
        t1_claimed.ready = false;
        t1_claimed.owner = Some("t3:other".parse().unwrap());

        e.stub.on("GET /v1/boards", [daemon_board(None)]);
        e.stub.on(
            "POST /v1/boards/factory/coordinator",
            daemon_board(Some(DISPATCH)),
        );
        e.stub.on(
            "POST /v1/agents",
            agent_fixture(DISPATCH, DEVICE, AgentState::Running),
        );
        e.stub.on("GET /v1/tasks", [t1.clone(), t2.clone()]);
        e.stub
            .on("GET /v1/tasks/factory-1", task_detail_for(t1_claimed));
        e.stub
            .on("GET /v1/tasks/factory-2", task_detail_for(t2.clone()));
        e.stub.on("POST /v1/tasks/factory-1/notes", note(&t1.id));
        e.stub.on("POST /v1/tasks/factory-2/notes", note(&t2.id));
        judges_at_work(&e, &[]);

        e.daemon.reconcile(at()).await.unwrap();

        assert_eq!(e.stub.requests_to("GET /v1/tasks/factory-1").len(), 1);
        assert!(fake.spawns().is_empty(), "claimed task must not spawn");
        assert!(e.t3.calls().is_empty());
        assert!(e.daemon.state.workers.is_empty());
        assert!(
            git_output(&repo, &["branch", "--list", "task/*"])
                .trim()
                .is_empty(),
            "no task/* branch may be created"
        );
        assert_eq!(
            git_output(&repo, &["worktree", "list", "--porcelain"])
                .matches("worktree ")
                .count(),
            1,
            "only the main checkout worktree may exist"
        );
        let drops = daemon_drops(&home);
        assert!(
            drops
                .iter()
                .any(|d| d["kind"] == "worker_skipped_not_ready" && d["task"] == "factory-1"),
            "expected trail rule record for skipped claimed task: {drops:#?}"
        );
    }

    // 2. Closed (`Done` or `Dropped`) between `next_tasks` and `start_worker`: skipped with
    //    no worker, worktree or branch, and the next ready task starts instead.
    for closed_status in [TaskStatus::Done, TaskStatus::Dropped] {
        let (_git_dir, repo) = init_repo();
        let mut e = env_with_projects([(REPO.to_owned(), repo.clone())].into());
        let home = Home::at(e._dir.path().join("home"));
        let fake = FakeRunner::new("acp")
            .with_home(home.clone())
            .with_clock(std::sync::Arc::new(at));
        e.daemon = e.daemon.with_runner(fake.clone());

        let t1 = ready_task("factory-1", P1, 0);
        let t2 = ready_task("factory-2", P1, 1);
        let mut t1_closed = t1.clone();
        t1_closed.status = closed_status;
        t1_closed.ready = false;

        e.stub.on("GET /v1/boards", [daemon_board(None)]);
        e.stub.on(
            "POST /v1/boards/factory/coordinator",
            daemon_board(Some(DISPATCH)),
        );
        e.stub.on(
            "POST /v1/agents",
            agent_fixture(DISPATCH, DEVICE, AgentState::Running),
        );
        e.stub.on("GET /v1/tasks", [t1.clone(), t2.clone()]);
        e.stub
            .on("GET /v1/tasks/factory-1", task_detail_for(t1_closed));
        e.stub
            .on("GET /v1/tasks/factory-2", task_detail_for(t2.clone()));
        e.stub.on("POST /v1/tasks/factory-1/notes", note(&t1.id));
        e.stub.on("POST /v1/tasks/factory-2/notes", note(&t2.id));
        judges_at_work(&e, &[]);

        e.daemon.reconcile(at()).await.unwrap();

        let spawns = fake.spawns();
        assert_eq!(
            spawns.len(),
            1,
            "only factory-2 should start ({closed_status:?})"
        );
        assert!(
            spawns[0].spec.title.starts_with("factory-2 "),
            "{:?}",
            spawns[0].spec.title
        );
        assert!(
            !e.daemon.state.workers.contains_key(&t1.id),
            "closed task must not be recorded as a worker"
        );
        assert!(
            git_output(&repo, &["branch", "--list", "task/factory-1*"])
                .trim()
                .is_empty(),
            "no branch for closed factory-1"
        );
        let drops = daemon_drops(&home);
        assert!(
            drops
                .iter()
                .any(|d| d["kind"] == "worker_skipped_not_ready" && d["task"] == "factory-1"),
            "expected trail rule record for closed task: {drops:#?}"
        );
    }

    // 3. `auto=false`, `ready=false`, live owner (`owner_stale=false`), and `404 not_found`
    //    are all skipped; a stale owner (`owner_stale=true`) on an open, ready, auto task is
    //    still eligible and starts.
    for (label, mutate) in [
        (
            "auto_false",
            Box::new(|t: &mut board_proto::Task| {
                t.auto = false;
            }) as Box<dyn Fn(&mut board_proto::Task)>,
        ),
        (
            "ready_false",
            Box::new(|t: &mut board_proto::Task| {
                t.ready = false;
            }),
        ),
        (
            "live_owner",
            Box::new(|t: &mut board_proto::Task| {
                t.owner = Some("t3:live".parse().unwrap());
                t.owner_stale = false;
            }),
        ),
    ] {
        let mut e = dispatch_env(&[ready_task("factory-1", P1, 0)]);
        let mut updated = ready_task("factory-1", P1, 0);
        mutate(&mut updated);
        e.stub
            .on("GET /v1/tasks/factory-1", task_detail_for(updated));
        e.daemon.reconcile(at()).await.unwrap();
        assert!(
            worker_starts(&e.t3).is_empty(),
            "{label}: task must be skipped"
        );
        assert!(e.daemon.state.workers.is_empty(), "{label}");
    }

    // Deleted (`404`) between `next_tasks` and `start_worker`: skipped cleanly.
    {
        let mut e = dispatch_env(&[ready_task("factory-1", P1, 0)]);
        e.stub.on_error(
            "GET /v1/tasks/factory-1",
            404,
            "not_found",
            "no task factory-1",
            "list tasks",
        );
        e.daemon.reconcile(at()).await.unwrap();
        assert!(worker_starts(&e.t3).is_empty());
        assert!(e.daemon.state.workers.is_empty());
    }

    // Stale owner (`owner_stale=true`) on an open, ready, auto task: no live owner, so it starts.
    {
        let mut e = dispatch_env(&[ready_task("factory-1", P1, 0)]);
        let mut stale = ready_task("factory-1", P1, 0);
        stale.owner = Some("t3:stale".parse().unwrap());
        stale.owner_stale = true;
        e.stub.on("GET /v1/tasks/factory-1", task_detail_for(stale));
        e.daemon.reconcile(at()).await.unwrap();
        assert_eq!(worker_starts(&e.t3).len(), 1);
    }

    // Transient server error on re-read: does not start a worker.
    {
        let mut e = dispatch_env(&[ready_task("factory-1", P1, 0)]);
        e.stub.on_error(
            "GET /v1/tasks/factory-1",
            500,
            "internal",
            "db busy",
            "retry",
        );
        e.daemon.reconcile(at()).await.unwrap();
        assert!(worker_starts(&e.t3).is_empty());
        assert!(e.daemon.state.workers.is_empty());
    }
}

#[tokio::test]
async fn pull_dispatch_cancels_claimed_worker_when_task_is_no_longer_ready() {
    use board::config::DispatchMode;
    use board::local_db::LocalRunState;
    use board::runner::FakeRunner;
    use board_proto::{
        ClaimedRun, DispatchClaimResponse, Priority::P1, Run, RunId, RunKind, RunOrigin, RunState,
        ScoreBreakdown,
    };

    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &origin,
        &["commit", "--quiet", "--allow-empty", "-m", "init"],
    );
    git(dir.path(), &["clone", "--quiet", "origin", "repo"]);
    let repo = dir.path().join("repo");

    let stub = Stub::start();
    let t3 = FakeT3::default();
    let home = Home::at(dir.path().join("home"));
    let config = Config {
        server: Some(stub.url.clone()),
        device: Some(DEVICE.parse().unwrap()),
        projects: [(REPO.to_owned(), repo.clone())].into(),
        dispatch_mode: DispatchMode::Pull,
        ..Config::default()
    };
    let fake = FakeRunner::new("acp")
        .with_home(home.clone())
        .with_clock(std::sync::Arc::new(at));
    let mut daemon =
        daemon_configured(dir.path(), &stub, &t3, None, config).with_runner(fake.clone());
    daemon.set_dispatch_claim_supported(true);

    let board = daemon_board(None);
    stub.on("GET /v1/boards", [&board]);
    stub.on(&format!("GET /v1/devices/{DEVICE}/wakes"), json!([]));

    let run_id: RunId = "run-stale-worker-1".parse().unwrap();
    let task_id: board_proto::TaskId = "factory-1".parse().unwrap();
    let leased_run = Run {
        id: run_id.clone(),
        board: Some(factory()),
        task_id: Some(task_id.clone()),
        origin: RunOrigin::Managed,
        kind: RunKind::Worker,
        state: RunState::Leased,
        device: DEVICE.parse().unwrap(),
        runner: "acp".to_owned(),
        agent_id: None,
        worktree_branch: None,
        cwd: None,
        title: Some("factory-1 Task factory-1".to_owned()),
        attach_hint: None,
        cost_units: 2,
        outcome: None,
        transcript_uri: None,
        transcript_sha256: None,
        conflict_locks: vec![],
        policy_version: None,
        lease_expires_at: Some(at() + chrono::Duration::minutes(3)),
        created_at: at(),
        started_at: None,
        updated_at: at(),
        ended_at: None,
    };
    let claimed = ClaimedRun {
        run: leased_run.clone(),
        board: factory(),
        task_id: Some(task_id.clone()),
        tasks: vec![task_id.clone()],
        score: 1.0,
        score_breakdown: ScoreBreakdown::default(),
        conflict_keys: vec![],
        housekeeping_action: None,
    };
    stub.once(
        "POST /v1/dispatch/claim",
        DispatchClaimResponse {
            claimed: vec![claimed],
            holds_summary: vec![],
        },
    );
    stub.on(
        "POST /v1/dispatch/claim",
        DispatchClaimResponse {
            claimed: vec![],
            holds_summary: vec![],
        },
    );

    // Task was claimed by another agent between `POST /v1/dispatch/claim` and `GET /v1/tasks/factory-1`.
    let mut claimed_elsewhere = ready_task("factory-1", P1, 0);
    claimed_elsewhere.status = TaskStatus::Doing;
    claimed_elsewhere.ready = false;
    claimed_elsewhere.owner = Some("t3:other".parse().unwrap());
    stub.on(
        "GET /v1/tasks/factory-1",
        task_detail_for(claimed_elsewhere),
    );
    let mut cancelled_run = leased_run.clone();
    cancelled_run.state = RunState::Cancelled;
    stub.on(&format!("PATCH /v1/runs/{run_id}"), cancelled_run.clone());

    daemon.reconcile(at()).await.unwrap();

    assert!(
        fake.spawns().is_empty(),
        "stale claimed worker must not spawn"
    );
    assert!(
        git_output(&repo, &["branch", "--list", "task/*"])
            .trim()
            .is_empty(),
        "no task/* branch may be created"
    );
    assert_eq!(
        git_output(&repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1,
        "only the main checkout worktree may exist"
    );
    let patches = stub.requests_to(&format!("PATCH /v1/runs/{run_id}"));
    assert_eq!(patches.len(), 1);
    assert_eq!(
        patches[0].body["state"], "cancelled",
        "stale worker run must be cancelled, not failed: {:?}",
        patches[0].body
    );
    let local = daemon
        .db()
        .get_local_run(run_id.as_str())
        .unwrap()
        .expect("local run persisted");
    assert_eq!(local.state, LocalRunState::Cancelled);
    assert!(
        daemon.db().list_resources().unwrap().is_empty(),
        "resource ledger entry must be released"
    );
    let drops = daemon_drops(&home);
    assert!(
        drops
            .iter()
            .any(|d| d["kind"] == "worker_skipped_not_ready" && d["task"] == "factory-1"),
        "expected trail rule record for skipped pull worker: {drops:#?}"
    );

    // Deleted task (`404 not_found`) right after `POST /v1/dispatch/claim`: also cancelled
    // cleanly without spawning a worker or creating a worktree/branch.
    let run_id_2: RunId = "run-stale-worker-2".parse().unwrap();
    let task_id_2: board_proto::TaskId = "factory-2".parse().unwrap();
    let mut leased_run_2 = leased_run;
    leased_run_2.id = run_id_2.clone();
    leased_run_2.task_id = Some(task_id_2.clone());
    let claimed_2 = ClaimedRun {
        run: leased_run_2.clone(),
        board: factory(),
        task_id: Some(task_id_2.clone()),
        tasks: vec![task_id_2],
        score: 1.0,
        score_breakdown: ScoreBreakdown::default(),
        conflict_keys: vec![],
        housekeeping_action: None,
    };
    stub.once(
        "POST /v1/dispatch/claim",
        DispatchClaimResponse {
            claimed: vec![claimed_2],
            holds_summary: vec![],
        },
    );
    stub.on_error(
        "GET /v1/tasks/factory-2",
        404,
        "not_found",
        "no task factory-2",
        "list tasks",
    );
    let mut cancelled_run_2 = leased_run_2;
    cancelled_run_2.state = RunState::Cancelled;
    stub.on(&format!("PATCH /v1/runs/{run_id_2}"), cancelled_run_2);

    daemon.reconcile(minute(1)).await.unwrap();

    assert!(fake.spawns().is_empty(), "deleted task must not spawn");
    let patches_2 = stub.requests_to(&format!("PATCH /v1/runs/{run_id_2}"));
    assert_eq!(patches_2.len(), 1);
    assert_eq!(patches_2[0].body["state"], "cancelled");
    assert!(daemon.db().list_resources().unwrap().is_empty());
}
