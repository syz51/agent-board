//! The device daemon's start and wake decisions, with a fake `t3`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use board::client::Client;
use board::config::{Config, Home};
use board::daemon::{Daemon, RELEASE_GRACE};
use board::gh::{Checks, Gh, MergeState, PrState, PullRequest, Updated};
use board::identity::RepoInfo;
use board::ops::Ctx;
use board::t3::{ListedThread, NewWorktree, RecentMessages, T3, ThreadInfo, TimedOut, UserMessage};
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
    },
    Send {
        thread: String,
        message: String,
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
                Call::Send { thread, message } => Some((thread, message)),
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
    ) -> Result<String> {
        self.calls.lock().unwrap().push(Call::Start {
            project: project.to_path_buf(),
            title: title.to_owned(),
            message: message.to_owned(),
            worktree: worktree.cloned(),
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
        self.calls.lock().unwrap().push(Call::Send {
            thread: thread.to_owned(),
            message: message.to_owned(),
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
        Ok(self
            .threads
            .lock()
            .unwrap()
            .get(thread)
            .cloned()
            .map(|info| ThreadInfo { runs, ..info }))
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
    // An entry this version cannot read is skipped, not the rest of the state.
    let path = home_of(&e).state_dir().join("watch.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved["unsure_wakes"]["t3:other"] = json!({ "messages": 5 });
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();

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
    let path = home_of(&e).state_dir().join("watch.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
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
    board::state::load_named(&Home::at(e._dir.path().join("home")), "watch")
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
    let saved: board::daemon::WatchState = board::state::load_named(&Home::at(home), "watch");
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
    let saved: board::daemon::WatchState = board::state::load_named(&Home::at(home), "watch");
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
        e.stub
            .on(&format!("POST /v1/tasks/{}/notes", task.id), note(&task.id));
    }
    e
}

#[test]
fn the_worker_brief_says_how_to_run_a_climb() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let brief = words(board::brief::WORKER_PROMPT);
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
    let path = home_of(&e).state_dir().join("watch.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
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
    e.stub.on(
        "GET /v1/tasks",
        [
            doing_task("factory-9", "t3:other", &[]),
            waits,
            ready_task("factory-5", P2, 1),
        ],
    );
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

    // factory-4 ends: factory-5 starts again at once.
    task_at(&e, "factory-4", TaskStatus::Review, Some(&owner));
    e.stub.on("GET /v1/tasks", &tasks[1..]);
    e.daemon.reconcile(much_later).await.unwrap();
    let starts = titles(&e.t3);
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[2], "factory-5 Task factory-5");
    assert!(e.daemon.state.worker_skips.is_empty());
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
        head_ref: "feat/limit".to_owned(),
        head_oid: "9c1e44d0a7b2c3".to_owned(),
        cross_repository: false,
        merge_commit: None,
        merge_state,
        checks,
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
    assert_eq!(
        worker_starts(&e.t3),
        [(
            "goal review factory".to_owned(),
            board::brief::judge_brief(board::brief::JudgeKind::Goal, &factory(), &[])
        )]
    );
    assert_eq!(
        registered(&e),
        [json!({"agent": format!("t3:{NEW_THREAD}"), "kind": "goal", "tasks": []})]
    );

    // One review per finished task: the next waits for another task to finish.
    judges_at_work(&e, &[judge(NEW_THREAD, "goal", &[], false)]);
    e.daemon
        .reconcile(at() + minute + board::daemon::JUDGE_RETRY)
        .await
        .unwrap();
    assert_eq!(worker_starts(&e.t3).len(), 1);

    // A board with work left gets a worker, and no goal review.
    let mut e = judges_env(&[ready_task("factory-4", board_proto::Priority::P1, 0)]);
    e.stub.on("GET /v1/boards", [goal_board(None)]);
    e.daemon.state.goal_due.insert(factory());
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);
    assert!(e.daemon.state.goal_due.contains(&factory()), "still due");
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
        title: "goal review other".to_owned(),
        started_at: at(),
        registered: false,
        noted: false,
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
    // Three runs at once: two of them may be workers, and the other board's judge is one.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(3);
    other_board_with_a_judge(&mut e, 3);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3).len(), 2);

    // Two runs at once: the judge has one, and a worker takes the other.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(2);
    other_board_with_a_judge(&mut e, 3);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);

    // A paused board's judge still runs, so it keeps its slot.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(2);
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
    assert_eq!(titles(&e.t3).len(), 3, "four runs leave three to workers");
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
    // Two threads the dispatcher did not start run on the device: one of its three runs
    // is left, so one worker starts and the next task is held, with a note that says why.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(3);
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
        "has 3 of 3 runs: 1 of the dispatcher, 2 turns of other threads",
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
    assert!(cause.contains("has 3 of 3 runs"), "{cause}");

    // The dispatcher's own run is among the running threads and counts once.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(2);
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

    // A T3 that does not answer leaves the count to the dispatcher's own runs.
    let mut e = slots_env(3, &tasks);
    e.daemon = e.daemon.with_default_run_limit(2);
    *e.t3.running.lock().unwrap() = vec!["user-1".to_owned(), "user-2".to_owned()];
    e.t3.fail_list.store(true, Ordering::SeqCst);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3), ["factory-4 Task factory-4"]);

    // A device without a limit asks T3 for nothing.
    let mut e = slots_env(3, &tasks);
    *e.t3.running.lock().unwrap() = vec!["user-1".to_owned()];
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(titles(&e.t3).len(), 3);
    assert_eq!(running_lists(&e.t3), 0);
}

#[tokio::test]
async fn a_judge_waits_while_other_threads_fill_the_device() {
    let tasks = [review_task("factory-4", 0)];
    let mut e = judges_env(&tasks);
    review_detail(&e, &tasks[0], vec![]);
    e.daemon = e.daemon.with_default_run_limit(2);
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

    // One of them ends: the judge starts.
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
    e.daemon = e.daemon.with_default_run_limit(2);
    other_board_with_a_judge(&mut e, 1);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(5))
        .await
        .unwrap();
    assert_eq!(titles(&e.t3), ["review factory-4 Task factory-4"]);
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
