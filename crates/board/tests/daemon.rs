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
use board::daemon::{Adoption, Coordinator, Daemon, load_adoption, save_adoption};
use board::identity::RepoInfo;
use board::ops::Ctx;
use board::t3::{ListedThread, NewWorktree, RecentMessages, T3, ThreadInfo, TimedOut, UserMessage};
use board_proto::{
    AgentState, Ask, AskKind, AskStatus, Board, Event, EventType, Runtime, TaskDetail, TaskStatus,
    Timestamp,
};
use common::*;
use serde_json::json;

const REPO: &str = "github.com/syz51/agent-board";
const NEW_THREAD: &str = "thread:project:p1:new";
const ORPHAN: &str = "thread:project:p1:orphan";

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
    /// Saved by the next `t3 thread send`: a newer `board board adopt` arriving meanwhile.
    adopt_during_send: Arc<Mutex<Option<(Home, Adoption)>>>,
    /// Each thread's `runCount`, as `t3 thread show` gives it.
    runs: Arc<Mutex<BTreeMap<String, u64>>>,
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

    fn lists(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| matches!(c, Call::List { .. }))
            .count()
    }

    /// Sets `thread`'s `runCount`.
    fn set_runs(&self, thread: &str, runs: u64) {
        self.runs.lock().unwrap().insert(thread.to_owned(), runs);
    }

    /// A message to `thread` from someone other than the daemon, such as the user.
    fn message(&self, thread: &str, text: &str) {
        self.received
            .lock()
            .unwrap()
            .entry(thread.to_owned())
            .or_default()
            .push(text.to_owned());
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
        self.live(NEW_THREAD);
        self.list(title, NEW_THREAD, now, true);
        Ok(NEW_THREAD.to_owned())
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
        if let Some((home, adoption)) = self.adopt_during_send.lock().unwrap().take() {
            save_adoption(&home, &"factory".parse().unwrap(), &adoption).unwrap();
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
}

struct Env {
    _dir: tempfile::TempDir,
    stub: Stub,
    t3: FakeT3,
    daemon: Daemon<FakeT3>,
}

/// A project folder named `name` that exists, since the daemon runs no coordinator in a
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
    let ctx = Ctx {
        home: Home::at(dir.join("home")),
        device: DEVICE.parse().unwrap(),
        client: Client::new(&stub.url, TOKEN.into(), None, timeout).unwrap(),
        config,
        repo: RepoInfo::default(),
    };
    Daemon::new(ctx, t3.clone())
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
async fn a_home_device_starts_a_coordinator_when_none_is_live() {
    let mut e = env();
    e.stub
        .on("GET /v1/boards", [autopilot(None, &["mac-air", DEVICE])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );

    e.daemon.reconcile(at()).await.unwrap();

    let claims = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0].body, json!({"device": DEVICE, "agent": null}));
    assert_eq!(claims[0].agent, None, "the first claim is the device's");
    let agent = format!("t3:{NEW_THREAD}");
    assert_eq!(claims[1].body, json!({"device": DEVICE, "agent": agent}));
    assert_eq!(claims[1].agent.as_deref(), Some(agent.as_str()));

    let calls = e.t3.calls();
    let [
        Call::Start {
            project,
            title,
            message,
            worktree: None,
        },
    ] = calls.as_slice()
    else {
        panic!("{calls:?}");
    };
    assert_eq!(project, &src("agent-board"));
    assert_eq!(title, "factory coordinator");
    assert!(message.starts_with("# Coordinator"));
    assert!(
        message
            .ends_with("You are the coordinator of board factory. Start now: call board_snapshot.")
    );

    let register = &e.stub.requests_to("POST /v1/agents")[0].body;
    assert_eq!(register["id"], agent);
    assert_eq!(register["role"], "coordinator");
    assert_eq!(register["repo"], REPO);
    assert_eq!(
        e.daemon.state.coordinators[&"factory".parse::<board_proto::BoardSlug>().unwrap()].thread,
        NEW_THREAD
    );
}

#[tokio::test]
async fn no_start_off_home_devices_with_a_live_coordinator_or_a_refused_claim() {
    // Not a home device.
    let mut e = env();
    e.stub.on("GET /v1/boards", [autopilot(None, &["mac-air"])]);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());
    assert!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .is_empty()
    );

    // A live coordinator on another device.
    let mut e = env();
    e.stub.on(
        "GET /v1/boards",
        [autopilot(Some("t3:elsewhere"), &[DEVICE])],
    );
    e.stub.on(
        &format!("GET {}", agent_path("t3:elsewhere")),
        agent_fixture("t3:elsewhere", "mac-air", AgentState::Idle),
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());

    // A paused coordinator is live too: never replaced or woken early.
    let mut e = env();
    e.stub.on(
        "GET /v1/boards",
        [autopilot(Some("t3:elsewhere"), &[DEVICE])],
    );
    let mut paused = agent_fixture("t3:elsewhere", DEVICE, AgentState::Paused);
    paused.paused_until = Some(at() + chrono::Duration::hours(2));
    e.stub
        .on(&format!("GET {}", agent_path("t3:elsewhere")), &paused);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());

    // Another device won the lease.
    let mut e = env();
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on_error(
        "POST /v1/boards/factory/coordinator",
        409,
        "claim_refused",
        "mac-air holds it",
        "wait",
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());
    assert_eq!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_failed_thread_check_releases_an_ended_coordinators_lease_and_renews_a_working_one() {
    let mut e = env();
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(starts(&e.t3), 1);
    let agent = format!("t3:{NEW_THREAD}");
    e.stub
        .on("GET /v1/boards", [autopilot(Some(&agent), &[DEVICE])]);
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    let lease = |e: &Env, method: &str| {
        e.stub
            .requests_to(&format!("{method} /v1/boards/factory/coordinator"))
            .len()
    };
    e.t3.fail_info.store(true, Ordering::SeqCst);

    // At work: a failed check says nothing of the thread, so the lease is renewed.
    e.stub.on(
        &format!("GET {}", agent_path(&agent)),
        agent_fixture(&agent, DEVICE, AgentState::Idle),
    );
    e.stub.clear();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(16))
        .await
        .unwrap();
    assert_eq!((lease(&e, "POST"), lease(&e, "DELETE")), (1, 0));

    // T3 answers again: the failures are forgotten.
    e.t3.fail_info.store(false, Ordering::SeqCst);
    e.stub.clear();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(20))
        .await
        .unwrap();
    assert_eq!((lease(&e, "POST"), lease(&e, "DELETE")), (0, 0));

    // Its session ended (T3 closed it while idle) and T3 cannot answer: not renewed while
    // the checks fail, and released once they have failed for CHECK_FAIL_RELEASE, so another
    // home device can take over. The record stays.
    e.t3.fail_info.store(true, Ordering::SeqCst);
    e.stub.on(
        &format!("GET {}", agent_path(&agent)),
        agent_fixture(&agent, DEVICE, AgentState::Ended),
    );
    e.stub.clear();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(26))
        .await
        .unwrap();
    assert_eq!((lease(&e, "POST"), lease(&e, "DELETE")), (0, 0));
    e.stub.clear();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(30))
        .await
        .unwrap();
    assert_eq!((lease(&e, "POST"), lease(&e, "DELETE")), (0, 0));
    e.stub.clear();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(36))
        .await
        .unwrap();
    assert_eq!((lease(&e, "POST"), lease(&e, "DELETE")), (0, 1));
    let slug: board_proto::BoardSlug = "factory".parse().unwrap();
    assert_eq!(e.daemon.state.coordinators[&slug].thread, NEW_THREAD);

    // T3 answers again and no one took the board: the thread is adopted, not replaced.
    e.t3.fail_info.store(false, Ordering::SeqCst);
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.clear();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(48))
        .await
        .unwrap();
    let claims = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].body["agent"], agent);
    assert_eq!(starts(&e.t3), 1, "no new thread");
}

#[tokio::test]
async fn an_ended_coordinator_of_another_device_keeps_its_lease_and_ours_is_adopted() {
    // Another device's coordinator whose session ended: that device's daemon renews or
    // releases its lease, so this one waits for the lease to expire.
    let mut e = env();
    e.stub.on(
        "GET /v1/boards",
        [autopilot(Some("t3:elsewhere"), &[DEVICE, "mac-air"])],
    );
    e.stub.on(
        &format!("GET {}", agent_path("t3:elsewhere")),
        agent_fixture("t3:elsewhere", "mac-air", AgentState::Ended),
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());
    assert!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .is_empty()
    );
    // Once the lease expires the board names no coordinator, and this device starts one.
    e.stub
        .on("GET /v1/boards", [autopilot(None, &[DEVICE, "mac-air"])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(46))
        .await
        .unwrap();
    assert_eq!(starts(&e.t3), 1);

    // Ours, its thread still in T3, but unknown to this daemon's state (lost or reset):
    // adopted again, with no new thread.
    let mut e = env();
    let thread = "thread:project:p1:idle";
    let agent = format!("t3:{thread}");
    e.t3.threads.lock().unwrap().insert(
        thread.into(),
        ThreadInfo {
            archived: false,
            runtime: Runtime::Claude,
            worktree: None,
            runs: None,
        },
    );
    e.stub
        .on("GET /v1/boards", [autopilot(Some(&agent), &[DEVICE])]);
    e.stub.on(
        &format!("GET {}", agent_path(&agent)),
        agent_fixture(&agent, DEVICE, AgentState::Ended),
    );
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(Some(&agent), &[DEVICE]),
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(starts(&e.t3), 0);
    let claims = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].body["agent"], agent);
    let slug: board_proto::BoardSlug = "factory".parse().unwrap();
    assert_eq!(e.daemon.state.coordinators[&slug].thread, thread);
    assert_eq!(
        e.t3.infos.load(Ordering::SeqCst),
        1,
        "the next reconcile does not ask T3 again"
    );
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(e.t3.infos.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_ended_coordinator_of_ours_gives_way_to_an_adoption_and_a_gone_thread_is_released() {
    // Ours and its thread live, but the user asked to adopt another thread: the lease is
    // released, so the adopted thread is named, not the ended one.
    let mut e = adoption_env();
    let thread = "thread:project:p1:idle";
    let old = format!("t3:{thread}");
    e.t3.live(thread);
    e.stub
        .on("GET /v1/boards", [autopilot(Some(&old), &[DEVICE])]);
    e.stub.on(
        &format!("GET {}", agent_path(&old)),
        agent_fixture(&old, DEVICE, AgentState::Ended),
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(starts(&e.t3), 0);
    let order: Vec<String> = e
        .stub
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/boards/factory/coordinator")
        .map(|r| r.method)
        .collect();
    assert_eq!(order, ["DELETE", "POST", "POST"]);
    let claims = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(claims[1].body["agent"], format!("t3:{ADOPTED}"));
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, ADOPTED);

    // Ours, its thread gone: released before a new thread starts, so the gone one is not
    // named meanwhile.
    let mut e = env();
    e.stub
        .on("GET /v1/boards", [autopilot(Some(&old), &[DEVICE])]);
    e.stub.on(
        &format!("GET {}", agent_path(&old)),
        agent_fixture(&old, DEVICE, AgentState::Ended),
    );
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(starts(&e.t3), 1);
    let order: Vec<String> = e
        .stub
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/boards/factory/coordinator")
        .map(|r| r.method)
        .collect();
    assert_eq!(order.first().map(String::as_str), Some("DELETE"));
}

#[tokio::test]
async fn an_ended_coordinator_is_replaced_and_a_live_one_renewed() {
    let mut e = env();
    e.stub
        .on("GET /v1/boards", [autopilot(Some("t3:old"), &[DEVICE])]);
    e.stub.on(
        &format!("GET {}", agent_path("t3:old")),
        agent_fixture("t3:old", DEVICE, AgentState::Ended),
    );
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(e.t3.calls().len(), 1, "a new coordinator thread");

    // Ours and live: renew only once the renewal interval has passed.
    let agent = format!("t3:{NEW_THREAD}");
    e.stub
        .on("GET /v1/boards", [autopilot(Some(&agent), &[DEVICE])]);
    e.stub.on(
        &format!("GET {}", agent_path(&agent)),
        agent_fixture(&agent, DEVICE, AgentState::Idle),
    );
    e.stub.clear();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(5))
        .await
        .unwrap();
    assert!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .is_empty()
    );
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(11))
        .await
        .unwrap();
    let renew = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(renew.len(), 1);
    assert_eq!(renew[0].body["agent"], agent);
    assert_eq!(e.t3.calls().len(), 1, "no second thread");

    // Thread archived: start a fresh one, after releasing the lease that still names the
    // archived thread, so the board never names it while the new one starts.
    e.t3.threads.lock().unwrap().insert(
        NEW_THREAD.into(),
        ThreadInfo {
            archived: true,
            runtime: Runtime::Claude,
            worktree: None,
            runs: None,
        },
    );
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.clear();
    // Found live a minute ago: T3 is not asked again until THREAD_RECHECK has passed.
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(12))
        .await
        .unwrap();
    assert_eq!(e.t3.calls().len(), 1, "not checked again yet");
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(16))
        .await
        .unwrap();
    assert_eq!(e.t3.calls().len(), 2);
    let lease: Vec<_> = e
        .stub
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/boards/factory/coordinator")
        .map(|r| r.method)
        .collect();
    assert_eq!(
        lease,
        ["DELETE", "POST", "POST"],
        "release, claim, claim as the agent"
    );
}

/// A coordinator of ours, named and idle, in the thread `coord`, whose board maps [`REPO`]
/// to `project`.
fn coordinator_in(project: &Path) -> Env {
    let mut e = env_with_projects([(REPO.to_owned(), project.to_path_buf())].into());
    let agent = "t3:coord";
    e.t3.live("coord");
    e.daemon.state.coordinators.insert(
        factory(),
        Coordinator {
            thread: "coord".into(),
            agent: agent.parse().unwrap(),
            started_at: at(),
            renewed_at: at(),
            resumed_after: None,
            registered: true,
            briefed: Some(board::daemon::prompt_digest(false).to_owned()),
            rebrief_backoff: None,
            rebrief_unsure: None,
            fresh: Default::default(),
        },
    );
    e.stub
        .on("GET /v1/boards", [autopilot(Some(agent), &[DEVICE])]);
    e.stub.on(
        &format!("GET {}", agent_path(agent)),
        agent_fixture(agent, DEVICE, AgentState::Ended),
    );
    for method in ["POST", "DELETE"] {
        e.stub.on(
            &format!("{method} /v1/boards/factory/coordinator"),
            autopilot(None, &[DEVICE]),
        );
    }
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(&format!("t3:{NEW_THREAD}"), DEVICE, AgentState::Running),
    );
    e
}

/// T3 fails every run of a thread whose folder is gone, at once, so each wake-up sent to it
/// would be lost: its coordinator is released and no longer woken, and no new one starts
/// until the folder is back.
#[tokio::test]
async fn a_coordinator_whose_project_folder_is_gone_is_released_and_not_woken() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("factory");
    std::fs::create_dir(&project).unwrap();
    let mut e = coordinator_in(&project);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(releases(&e), 0, "live while its folder is there");
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();
    assert!(
        !e.daemon.state.wakes.is_empty(),
        "a wake-up for the coordinator"
    );

    // Found live just now, but the folder is checked at once, not after the recheck delay.
    std::fs::remove_dir(&project).unwrap();
    e.daemon
        .reconcile(at() + chrono::Duration::seconds(30))
        .await
        .unwrap();
    assert_eq!(releases(&e), 1);
    assert!(!e.daemon.state.coordinators.contains_key(&factory()));
    // Its ended agent registered from no folder: the reconcile's verdict holds it back.
    e.daemon.flush(at()).await;
    assert!(e.t3.calls().is_empty(), "no send, no new thread");
    assert!(e.daemon.state.wakes.is_empty(), "its wake-ups are dropped");
    assert!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .is_empty(),
        "a device without the folder leaves the lease alone"
    );

    // Tried again after the back-off, still without the folder: nothing starts.
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(2))
        .await
        .unwrap();
    assert_eq!(starts(&e.t3), 0);

    // The folder is back: the next try starts a coordinator in it.
    std::fs::create_dir(&project).unwrap();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(10))
        .await
        .unwrap();
    assert_eq!(started(&e.t3).0, project);
}

#[tokio::test]
async fn a_coordinator_whose_worktree_is_gone_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = coordinator_in(dir.path());
    e.t3.threads.lock().unwrap().insert(
        "coord".into(),
        ThreadInfo {
            archived: false,
            runtime: Runtime::Claude,
            worktree: Some(dir.path().join("worktree-removed")),
            runs: None,
        },
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(releases(&e), 1);
    assert_eq!(started(&e.t3).0, dir.path(), "a new thread in the project");
    assert_eq!(
        e.daemon.state.coordinators[&factory()].thread,
        NEW_THREAD,
        "{:?}",
        e.daemon.state.coordinators
    );
}

#[tokio::test]
async fn a_coordinator_whose_worktree_goes_is_replaced_at_once_and_its_answers_follow() {
    let dir = tempfile::tempdir().unwrap();
    let worktree = dir.path().join("worktree");
    std::fs::create_dir(&worktree).unwrap();
    let mut e = coordinator_in(dir.path());
    e.t3.threads.lock().unwrap().insert(
        "coord".into(),
        ThreadInfo {
            archived: false,
            runtime: Runtime::Claude,
            worktree: Some(worktree.clone()),
            runs: None,
        },
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(releases(&e), 0, "live while its worktree is there");
    let ev = answered(&e, 11, 3, "t3:coord");
    e.daemon.on_event(&ev, at()).await.unwrap();

    // Found live just now, but its worktree is checked at once, not after the recheck delay.
    std::fs::remove_dir(&worktree).unwrap();
    e.daemon
        .reconcile(at() + chrono::Duration::seconds(30))
        .await
        .unwrap();
    assert_eq!(releases(&e), 1);
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, NEW_THREAD);

    // The answer to the gone coordinator's ask goes to its replacement, not to it.
    let new = format!("t3:{NEW_THREAD}");
    e.stub.on(
        &format!("GET {}", agent_path(&new)),
        agent_fixture(&new, DEVICE, AgentState::Idle),
    );
    e.daemon.flush(at() + chrono::Duration::seconds(31)).await;
    e.daemon.flush(at() + chrono::Duration::seconds(32)).await;
    let sends = e.t3.sends();
    assert_eq!(threads(&e.t3), [NEW_THREAD], "{sends:?}");
    assert!(sends[0].1.contains("ask #3"), "{sends:?}");
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn an_ended_worker_whose_folder_is_gone_has_its_answer_passed_to_the_coordinator() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    // One in a worktree T3 names, one in the folder its session registered from.
    e.t3.threads.lock().unwrap().insert(
        "worker".into(),
        ThreadInfo {
            archived: false,
            runtime: Runtime::Claude,
            worktree: Some(dir.path().join("worktree-removed")),
            runs: None,
        },
    );
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Ended),
    );
    e.t3.live("other");
    let mut other = agent_fixture("t3:other", DEVICE, AgentState::Ended);
    other.cwd = Some(dir.path().join("deleted").display().to_string());
    e.stub
        .on(&format!("GET {}", agent_path("t3:other")), &other);
    let ev = answered(&e, 11, 3, "t3:worker");
    e.daemon.on_event(&ev, at()).await.unwrap();
    let ev = answered(&e, 12, 4, "t3:other");
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    e.daemon.flush(at() + chrono::Duration::seconds(1)).await;
    let sends = e.t3.sends();
    assert!(
        sends.iter().all(|(thread, _)| thread == "coord"),
        "{sends:?}"
    );
    let text: String = sends.iter().map(|(_, m)| m.as_str()).collect();
    assert!(
        text.contains("ask #3") && text.contains("ask #4"),
        "{sends:?}"
    );
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn an_adoption_waits_for_its_project_folder_and_not_for_a_gone_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("factory");
    let mut e = adoption_env_with([(REPO.to_owned(), project.clone())].into());
    // No folder: nothing is claimed or asked, and the adoption waits.
    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .is_empty()
    );
    assert!(e.t3.calls().is_empty());
    assert!(load_adoption(&home_of(&e), &factory()).is_some(), "kept");

    // The folder is back, but the thread's own worktree is gone: a new thread starts.
    std::fs::create_dir(&project).unwrap();
    e.t3.threads.lock().unwrap().insert(
        ADOPTED.into(),
        ThreadInfo {
            archived: false,
            runtime: Runtime::Claude,
            worktree: Some(dir.path().join("worktree-removed")),
            runs: None,
        },
    );
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(10))
        .await
        .unwrap();
    assert_eq!(load_adoption(&home_of(&e), &factory()), None, "dropped");
    assert!(threads(&e.t3).is_empty(), "the gone thread is not briefed");
    assert_eq!(started(&e.t3).0, project);
}

fn coordinator_env(
    coordinator_state: AgentState,
    paused_until: Option<Timestamp>,
) -> (Env, String) {
    let mut e = env();
    let agent = "t3:coord".to_owned();
    e.t3.live("coord");
    e.daemon.state.coordinators.insert(
        "factory".parse().unwrap(),
        Coordinator {
            thread: "coord".into(),
            agent: agent.parse().unwrap(),
            started_at: at(),
            renewed_at: at(),
            resumed_after: None,
            registered: true,
            briefed: Some(board::daemon::prompt_digest(false).to_owned()),
            rebrief_backoff: None,
            rebrief_unsure: None,
            fresh: Default::default(),
        },
    );
    e.stub
        .on("GET /v1/boards", [autopilot(Some(&agent), &[DEVICE])]);
    let mut info = agent_fixture(&agent, DEVICE, coordinator_state);
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

#[tokio::test]
async fn a_ready_task_wakes_the_coordinator_once_with_board_text() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.stub
        .on("GET /v1/tasks/factory-5", detail("factory-5", false, &[]));

    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();
    e.daemon
        .on_event(
            &event(11, EventType::TaskUpdated, Some("factory-4"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();
    e.daemon
        .on_event(
            &event(12, EventType::TaskCreated, Some("factory-5"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends(),
        [(
            "coord".to_owned(),
            "board factory: factory-4 is ready (p1)\nRun board_snapshot and continue.".to_owned()
        )]
    );
    assert_eq!(e.daemon.state.last_seq, Some(12));

    // The coordinator's own changes do not wake it.
    e.daemon
        .on_event(
            &event(13, EventType::TaskReleased, Some("factory-4"), "t3:coord"),
            at(),
        )
        .await
        .unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends().len(), 1);
}

#[tokio::test]
async fn a_task_moved_to_review_wakes_the_coordinator_and_each_send_is_logged() {
    let logs = Logs::default();
    let _guard = logs.capture_at(tracing::Level::INFO);
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    let mut review = detail("factory-6", false, &[]);
    review.task.status = TaskStatus::Review;
    e.stub.on("GET /v1/tasks/factory-6", &review);
    let mut moved = event(20, EventType::TaskUpdated, Some("factory-6"), "t3:worker");
    moved.payload = json!({ "fields": ["status"], "from": "doing", "to": "review" });
    e.daemon.on_event(&moved, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends(),
        [(
            "coord".to_owned(),
            "board factory: factory-6 moved to review; check it against its acceptance checks, \
             or note what it still waits on\nRun board_snapshot and continue."
                .to_owned()
        )]
    );
    assert!(
        logs.text().contains("woke t3:coord: review"),
        "{}",
        logs.text()
    );

    // Moved on before the flush: nothing to check.
    let mut again = moved.clone();
    again.seq = 21;
    e.daemon.on_event(&again, at()).await.unwrap();
    review.task.status = TaskStatus::Done;
    e.stub.on("GET /v1/tasks/factory-6", &review);
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends().len(), 1);
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
                 Run board_snapshot and continue."
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
async fn an_idle_notice_for_an_owner_no_device_can_wake_goes_to_the_coordinator() {
    let line = |owner: &str| {
        let since = (at() - chrono::Duration::minutes(40)).format("%Y-%m-%d %H:%MZ");
        (
            "coord".to_owned(),
            format!(
                "board factory: factory-7 is in doing, and its owner {owner}, which cannot be \
                 woken, has been away from it since {since}. Check it, then continue it, hand it \
                 over, or park it by what it waits on\nRun board_snapshot and continue."
            ),
        )
    };
    let notice_for = |owner: &str| {
        let since = at() - chrono::Duration::minutes(40);
        let mut notice = event(30, EventType::TaskUpdated, Some("factory-7"), "server");
        notice.payload = json!({ "fields": [], "idle_doing": since, "away": owner });
        notice
    };
    let doing = |owner: &str| {
        let mut doing = detail("factory-7", false, &[]);
        doing.task.status = TaskStatus::Doing;
        doing.task.owner = Some(owner.parse().unwrap());
        doing
    };

    // Not a T3 thread: queued for the coordinator at once.
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    let codex = "codex:b";
    e.stub.on("GET /v1/tasks/factory-7", doing(codex));
    e.stub.on(
        &format!("GET {}", agent_path(codex)),
        agent_fixture(codex, DEVICE, AgentState::Idle),
    );
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.on_event(&notice_for(codex), at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends(), [line(codex)]);

    // A T3 owner whose thread is gone: found at the flush, then passed on.
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    let gone = "t3:gone";
    e.stub.on("GET /v1/tasks/factory-7", doing(gone));
    e.stub.on(
        &format!("GET {}", agent_path(gone)),
        agent_fixture(gone, DEVICE, AgentState::Ended),
    );
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.on_event(&notice_for(gone), at()).await.unwrap();
    e.daemon.flush(at()).await;
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends(), [line(gone)]);

    // The owner running again before the send: the passed-on line no longer holds.
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.stub.on("GET /v1/tasks/factory-7", doing(codex));
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.on_event(&notice_for(codex), at()).await.unwrap();
    let mut back = event(31, EventType::AgentStateChanged, None, codex);
    back.payload = json!({ "from": "idle", "to": "running", "paused_until": null });
    e.daemon.on_event(&back, at()).await.unwrap();
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn a_move_to_review_by_the_coordinators_own_delegates_does_not_wake_it() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    let mut review = detail("factory-6", false, &[]);
    review.task.status = TaskStatus::Review;
    e.stub.on("GET /v1/tasks/factory-6", &review);
    for (agent, parent) in [("t3:child", "coord"), ("t3:grandchild", "child")] {
        let mut info = agent_fixture(agent, DEVICE, AgentState::Running);
        info.native_ids
            .insert("t3_parent".to_owned(), parent.to_owned());
        e.stub.on(&format!("GET {}", agent_path(agent)), info);
    }
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Running),
    );
    for (seq, mover) in [(20, "t3:coord"), (21, "t3:child"), (22, "t3:grandchild")] {
        let mut moved = event(seq, EventType::TaskUpdated, Some("factory-6"), mover);
        moved.payload = json!({ "fields": ["status"], "from": "doing", "to": "review" });
        e.daemon.on_event(&moved, at()).await.unwrap();
    }
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());

    // Any other mover wakes it.
    let mut moved = event(23, EventType::TaskUpdated, Some("factory-6"), "t3:worker");
    moved.payload = json!({ "fields": ["status"], "from": "doing", "to": "review" });
    e.daemon.on_event(&moved, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends().len(), 1);
}

#[tokio::test]
async fn a_filed_proposal_wakes_the_coordinator_to_review_it() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    let mut proposal = detail("factory-8", true, &[]);
    proposal.task.auto = false;
    proposal.task.body = "Proposal: the wake drops lines.".into();
    e.stub.on("GET /v1/tasks/factory-8", proposal);
    let mut plain = detail("factory-9", true, &[]);
    plain.task.auto = false;
    e.stub.on("GET /v1/tasks/factory-9", plain);

    for (seq, kind, id) in [
        (40, EventType::TaskCreated, "factory-8"),
        // Edits to a pending proposal and new non-auto tasks that are not proposals wake no one.
        (41, EventType::TaskUpdated, "factory-8"),
        (42, EventType::TaskCreated, "factory-9"),
    ] {
        e.daemon
            .on_event(&event(seq, kind, Some(id), "t3:worker"), at())
            .await
            .unwrap();
    }
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends(),
        [(
            "coord".to_owned(),
            "board factory: factory-8 is a proposal to review\nRun board_snapshot and continue."
                .to_owned()
        )]
    );

    // A proposal declined before the flush is not sent.
    let mut declined = detail("factory-10", false, &[]);
    declined.task.auto = false;
    declined.task.body = "Proposal: another.".into();
    e.stub.on("GET /v1/tasks/factory-10", &declined);
    e.daemon
        .on_event(
            &event(43, EventType::TaskCreated, Some("factory-10"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();
    declined.task.status = board_proto::TaskStatus::Done;
    e.stub.on("GET /v1/tasks/factory-10", &declined);
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends().len(), 1);
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn a_passed_not_before_wakes_the_coordinator() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.stub
        .on("GET /v1/tasks/factory-7", detail("factory-7", true, &[]));
    // What the server's sweeper records when the clock passes the task's not_before.
    let mut passed = event(30, EventType::TaskUpdated, Some("factory-7"), "server");
    passed.payload = json!({ "fields": [], "not_before_passed": "2026-11-07T00:00:00Z" });
    e.daemon.on_event(&passed, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends(),
        [(
            "coord".to_owned(),
            "board factory: factory-7 is ready (p1)\nRun board_snapshot and continue.".to_owned()
        )]
    );
}

#[tokio::test]
async fn a_cleared_blocker_wakes_the_coordinator() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.stub.on(
        "GET /v1/tasks/factory-9",
        detail("factory-9", false, &["factory-12", "factory-13"]),
    );
    e.stub
        .on("GET /v1/tasks/factory-12", detail("factory-12", true, &[]));
    e.stub
        .on("GET /v1/tasks/factory-13", detail("factory-13", false, &[]));
    e.daemon
        .on_event(
            &event(20, EventType::TaskDone, Some("factory-9"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();
    e.daemon.flush(at()).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 1);
    assert!(
        sends[0]
            .1
            .starts_with("board factory: factory-9 is done; its blocker cleared for factory-12\n"),
        "{sends:?}"
    );
}

/// A blocker on another board: its done event comes from that board, and wakes the
/// coordinator of the waiting task's board and the owner of a task blocked on it.
#[tokio::test]
async fn a_blocker_done_on_another_board_wakes_the_waiting_boards_coordinator_and_owner() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
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
    let mut sends = e.t3.sends();
    sends.sort();
    assert_eq!(
        sends,
        [
            (
                "coord".to_owned(),
                "board factory: other-3 is done; its blocker cleared for factory-12\nRun board_snapshot and continue."
                    .to_owned()
            ),
            (
                "worker".to_owned(),
                "board factory: every blocker of your blocked factory-13 is done; resume it (task_update factory-13 status=doing)\nRun board_snapshot and continue."
                    .to_owned()
            ),
        ]
    );
    assert!(e.daemon.state.wakes.is_empty());

    // Checked again when sent: an owner who resumed already, or a task claimed meanwhile,
    // is not woken.
    e.daemon.on_event(&ev, at()).await.unwrap();
    blocked.task.status = board_proto::TaskStatus::Doing;
    e.stub.on("GET /v1/tasks/factory-13", &blocked);
    let mut claimed = detail("factory-12", false, &[]);
    claimed.task.status = board_proto::TaskStatus::Doing;
    e.stub.on("GET /v1/tasks/factory-12", &claimed);
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends().len(), 2);
    assert!(e.daemon.state.wakes.is_empty());

    // A not_before still ahead holds the move to doing: the wake names the date.
    blocked.task.status = board_proto::TaskStatus::Blocked;
    blocked.task.not_before = Some(at() + chrono::Duration::days(1));
    e.stub.on("GET /v1/tasks/factory-13", &blocked);
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 3, "{sends:?}");
    let wait = format!(
        "it waits until {}; resume it then",
        (at() + chrono::Duration::days(1)).format("%Y-%m-%d %H:%MZ")
    );
    assert!(sends[2].1.contains(&wait), "{sends:?}");
}

/// A task created with `claim` is in doing when the daemon reads it: no ready wake.
#[tokio::test]
async fn a_task_created_claimed_wakes_no_one() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
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

#[tokio::test]
async fn a_paused_coordinator_is_not_woken_before_its_reset() {
    let reset = at() + chrono::Duration::hours(1);
    let (mut e, _) = coordinator_env(AgentState::Paused, Some(reset));
    e.daemon.reconcile(at()).await.unwrap();
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "user"),
            at(),
        )
        .await
        .unwrap();
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty());

    // After the reset: the event wakes it, and reconcile resumes it once.
    let later = reset + chrono::Duration::minutes(1);
    e.daemon
        .on_event(
            &event(11, EventType::TaskCreated, Some("factory-4"), "user"),
            later,
        )
        .await
        .unwrap();
    e.daemon.flush(later).await;
    assert_eq!(e.t3.sends().len(), 1);
    e.daemon.reconcile(later).await.unwrap();
    e.daemon.reconcile(later).await.unwrap();
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert!(sends[1].1.contains("the usage limit has reset"));

    // Paused without a known reset time: left to the user.
    let (mut e, _) = coordinator_env(AgentState::Paused, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "user"),
            at(),
        )
        .await
        .unwrap();
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty());
}

#[tokio::test]
async fn a_coordinator_whose_session_ended_during_a_limit_waits_for_the_reset() {
    // Paused on a usage limit, then T3 closed its idle session: the server keeps the
    // reset on the ended agent. Waking it before the reset would only stop on the limit
    // again (and report another pause).
    let reset = at() + chrono::Duration::hours(1);
    let (mut e, agent) = coordinator_env(AgentState::Paused, Some(reset));
    e.daemon.reconcile(at()).await.unwrap();
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "user"),
            at(),
        )
        .await
        .unwrap();
    // Read as paused at the first flush, then followed through its events.
    e.daemon.flush(at()).await;
    e.daemon
        .on_event(
            &state_changed(11, &agent, AgentState::Ended, Some(reset)),
            at(),
        )
        .await
        .unwrap();
    e.daemon
        .on_event(
            &event(12, EventType::TaskCreated, Some("factory-4"), "user"),
            at(),
        )
        .await
        .unwrap();
    e.daemon.flush(at()).await;
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());

    // From the reset on, the ended thread is woken like any other.
    let later = reset + chrono::Duration::minutes(1);
    e.daemon.flush(later).await;
    assert_eq!(threads(&e.t3), ["coord"]);
}

#[tokio::test]
async fn a_coordinator_whose_session_ended_during_a_limit_is_resumed_at_the_reset() {
    // Nothing is queued for it: reconcile resumes it once the reset passes, as it does a
    // paused one.
    let reset = at() + chrono::Duration::hours(1);
    let (mut e, _) = coordinator_env(AgentState::Ended, Some(reset));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());

    let later = reset + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();
    e.daemon.reconcile(later).await.unwrap();
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 1, "{sends:?}");
    assert!(sends[0].1.contains("the usage limit has reset"));
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

#[tokio::test]
async fn a_live_coordinator_thread_is_asked_about_once_per_recheck_and_after_a_failure() {
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(Some(&agent), &[DEVICE]),
    );
    let infos = |e: &Env| e.t3.infos.load(Ordering::SeqCst);
    for minute in 0..=10 {
        e.daemon
            .reconcile(at() + chrono::Duration::minutes(minute))
            .await
            .unwrap();
    }
    assert_eq!(infos(&e), 3, "at minutes 0, 5 and 10, not every minute");

    // A failed send to the thread asks again on the next reconcile.
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "t3:worker"),
            at() + chrono::Duration::minutes(10),
        )
        .await
        .unwrap();
    e.t3.fail_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at() + chrono::Duration::minutes(10)).await;
    e.t3.fail_send.store(false, Ordering::SeqCst);
    let before = infos(&e);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(11))
        .await
        .unwrap();
    assert_eq!(infos(&e), before + 1, "asked again after the failed send");

    // So does its agent ending.
    e.daemon
        .on_event(
            &state_changed(11, &agent, AgentState::Ended, None),
            at() + chrono::Duration::minutes(11),
        )
        .await
        .unwrap();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(12))
        .await
        .unwrap();
    assert_eq!(infos(&e), before + 2, "asked again after the agent ended");
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(13))
        .await
        .unwrap();
    assert_eq!(infos(&e), before + 2, "then not again within the interval");
}

fn threads(t3: &FakeT3) -> Vec<String> {
    t3.sends().into_iter().map(|(t, _)| t).collect()
}

#[tokio::test]
async fn an_answered_ask_wakes_the_asker_only() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Idle),
    );

    let ev = answered(&e, 30, 3, "t3:worker");
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["worker"], "not the coordinator");
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

    // The coordinator's own ask wakes the coordinator.
    let ev = answered(&e, 31, 5, "t3:coord");
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["worker", "coord"]);
    assert!(
        e.t3.sends()[1]
            .1
            .starts_with("the user answered ask #5 about")
    );
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn an_answer_its_asker_cannot_be_woken_for_goes_to_the_coordinator() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    let mut no_reset = agent_fixture("t3:napper", DEVICE, AgentState::Paused);
    no_reset.paused_until = None;
    for (agent, info) in [
        (
            "t3:remote",
            agent_fixture("t3:remote", "mac-air", AgentState::Idle),
        ),
        (
            "t3:ended",
            agent_fixture("t3:ended", DEVICE, AgentState::Ended),
        ),
        ("t3:napper", no_reset),
    ] {
        e.stub.on(&format!("GET {}", agent_path(agent)), info);
    }
    // Not a T3 thread, on another device, ended with no thread left in T3, unknown to the
    // server, paused with no reset.
    let askers = [
        (4, "claude:s1"),
        (6, "t3:remote"),
        (7, "t3:ended"),
        (8, "t3:gone"),
        (9, "t3:napper"),
    ];
    for (seq, (ask, asker)) in (40..).zip(askers) {
        let ev = answered(&e, seq, ask, asker);
        e.daemon.on_event(&ev, at()).await.unwrap();
    }
    e.daemon.flush(at()).await;
    e.daemon.flush(at()).await;
    e.daemon.flush(at()).await;
    let sends = e.t3.sends();
    assert!(sends.iter().all(|(t, _)| t == "coord"), "{sends:?}");
    let text: String = sends.iter().map(|(_, m)| m.as_str()).collect();
    for (ask, asker) in askers {
        let line = format!("the user answered ask #{ask} by {asker} about factory-7: ");
        assert_eq!(text.matches(&line).count(), 1, "{line} in {text}");
    }
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
    let (mut e, _) = coordinator_env(AgentState::Ended, None);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty(), "the live thread stays coordinator");
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends(),
        [(
            "coord".to_owned(),
            "board factory: factory-4 is ready (p1)\nRun board_snapshot and continue.".to_owned()
        )]
    );

    // An ended asker whose thread is live hears its own answer, not the coordinator.
    e.t3.live("worker");
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Ended),
    );
    let ev = answered(&e, 11, 3, "t3:worker");
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["coord", "worker"]);
    assert!(
        e.t3.sends()[1]
            .1
            .starts_with("the user answered ask #3 about factory-7")
    );
    assert!(e.daemon.state.wakes.is_empty());
}

#[tokio::test]
async fn an_ended_coordinator_whose_thread_is_archived_is_not_woken_and_says_so() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let (mut e, _) = coordinator_env(AgentState::Ended, None);
    e.daemon.reconcile(at()).await.unwrap();
    // Archived after the reconcile, which would otherwise replace it.
    e.t3.threads.lock().unwrap().insert(
        "coord".into(),
        ThreadInfo {
            archived: true,
            runtime: Runtime::Claude,
            worktree: None,
            runs: None,
        },
    );
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();
    // An answer to the coordinator's own ask has no other coordinator to go to.
    let ev = answered(&e, 11, 3, "t3:coord");
    e.daemon.on_event(&ev, at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty());
    assert!(e.daemon.state.wakes.is_empty());
    let text = logs.text();
    let gone =
        "waking t3:coord: T3 no longer has its thread, archived it, or the thread's folder is gone";
    assert_eq!(
        text.matches(&format!("{gone}; 1 wake-ups dropped")).count(),
        1,
        "{text}"
    );
    assert_eq!(
        text.matches(&format!(
            "{gone}; 1 answers to its asks or idle notices dropped, with no other coordinator \
             here to pass them to"
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
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
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
async fn a_coordinator_whose_lease_lapsed_is_still_woken_until_a_reconcile_settles_it() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    // The lease lapsed, and claiming it again failed for now: the board names no one.
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on_error(
        "POST /v1/boards/factory/coordinator",
        503,
        "unavailable",
        "server busy",
        "retry",
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.daemon.state.coordinators.contains_key(&factory()));
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["coord"]);
}

#[tokio::test]
async fn a_paused_board_keeps_its_coordinator_and_wakes_it_on_resume() {
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    let mut paused = autopilot(Some(&agent), &[DEVICE]);
    paused.status = board_proto::BoardStatus::Paused;
    e.stub.on("GET /v1/boards", [&paused]);
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );

    let mut pause = event(20, EventType::BoardUpdated, None, "user");
    pause.payload = json!({"fields": ["status"], "status": "paused"});
    assert!(e.daemon.on_event(&pause, at()).await.unwrap());
    let later = at() + chrono::Duration::hours(2);
    e.daemon.reconcile(later).await.unwrap();
    // An answer for a worker on the paused board starts no turn either.
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Idle),
    );
    e.t3.live("worker");
    let ev = answered(&e, 21, 3, "t3:worker");
    e.daemon.on_event(&ev, later).await.unwrap();
    e.daemon.flush(later).await;
    assert_eq!(releases(&e), 0, "the paused board keeps its coordinator");
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, "coord");
    assert!(e.t3.sends().is_empty(), "a pause wakes no one");
    assert!(e.daemon.state.wakes.is_empty(), "dropped, not kept");
    assert_eq!(starts(&e.t3), 0);

    e.stub
        .on("GET /v1/boards", [autopilot(Some(&agent), &[DEVICE])]);
    let mut resume = event(22, EventType::BoardUpdated, None, "user");
    resume.payload = json!({"fields": ["status"], "status": "active"});
    assert!(e.daemon.on_event(&resume, later).await.unwrap());
    e.daemon.reconcile(later).await.unwrap();
    e.daemon.flush(later).await;
    let sends = e.t3.sends();
    let [(thread, text)] = sends.as_slice() else {
        panic!("{sends:?}");
    };
    assert_eq!(thread, "coord");
    assert!(
        text.contains("board factory: the user resumed this board after a pause"),
        "{text}"
    );
    assert_eq!(starts(&e.t3), 0, "the same thread carries on");
}

#[tokio::test]
async fn a_paused_askers_answer_is_kept_until_its_reset() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
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
    // The coordinator hears of it now, once.
    assert_eq!(threads(&e.t3), ["coord"]);
    assert!(e.t3.sends()[0].1.contains("ask #3 by t3:worker"));

    // Kept past the time to live while the limit holds, and survives a restart.
    let before = reset - chrono::Duration::minutes(1);
    e.daemon.flush(before).await;
    assert_eq!(threads(&e.t3), ["coord"]);
    e.daemon.save().unwrap();
    let mut restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    restarted.reconcile(at()).await.unwrap();
    restarted.flush(before).await;
    assert_eq!(
        threads(&e.t3),
        ["coord"],
        "forwarded once, also after a restart"
    );

    // After the reset the asker gets it.
    restarted.flush(reset + chrono::Duration::minutes(1)).await;
    assert_eq!(threads(&e.t3), ["coord", "worker"]);
    assert!(
        e.t3.sends()[1]
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

/// Queues "ready" wake-ups for `ids` at `now`.
async fn queue_ready(e: &mut Env, first_seq: i64, ids: &[&str], now: Timestamp) {
    for (seq, id) in (first_seq..).zip(ids) {
        e.stub
            .on(&format!("GET /v1/tasks/{id}"), detail(id, true, &[]));
        e.daemon
            .on_event(&event(seq, EventType::TaskCreated, Some(id), "user"), now)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_queued_ready_line_is_checked_again_when_sent() {
    use board_proto::TaskStatus;
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    for (seq, id) in [(10, "factory-1"), (11, "factory-2"), (12, "factory-3")] {
        e.stub
            .on(&format!("GET /v1/tasks/{id}"), detail(id, true, &[]));
        e.daemon
            .on_event(
                &event(seq, EventType::TaskReleased, Some(id), "t3:worker"),
                at(),
            )
            .await
            .unwrap();
    }
    e.stub.on(
        "GET /v1/tasks/factory-9",
        detail("factory-9", false, &["factory-4", "factory-5"]),
    );
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.stub
        .on("GET /v1/tasks/factory-5", detail("factory-5", true, &[]));
    e.daemon
        .on_event(
            &event(13, EventType::TaskDone, Some("factory-9"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();

    // Before the flush: one claimed, one blocked, one done, one cleared task made manual.
    e.stub.on(
        "GET /v1/tasks/factory-1",
        task_now("factory-1", TaskStatus::Doing, false, true),
    );
    e.stub.on(
        "GET /v1/tasks/factory-2",
        task_now("factory-2", TaskStatus::Blocked, false, true),
    );
    e.stub.on(
        "GET /v1/tasks/factory-3",
        task_now("factory-3", TaskStatus::Done, false, true),
    );
    e.stub.on(
        "GET /v1/tasks/factory-5",
        task_now("factory-5", TaskStatus::Open, true, false),
    );
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends(),
        [(
            "coord".to_owned(),
            "board factory: factory-9 is done; its blocker cleared for factory-4\nRun board_snapshot and continue."
                .to_owned()
        )]
    );

    // Nothing left: a later flush sends nothing.
    assert!(e.daemon.state.wakes.is_empty());
    e.stub.on(
        "GET /v1/tasks/factory-4",
        task_now("factory-4", TaskStatus::Doing, false, true),
    );
    e.daemon
        .on_event(
            &event(14, EventType::TaskReleased, Some("factory-1"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends().len(), 1);
}

#[tokio::test]
async fn lines_for_a_running_agent_are_held_and_sent_as_one_message_when_it_goes_idle() {
    let (mut e, agent) = coordinator_env(AgentState::Running, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.stub.clear();
    let info = format!("GET {}", agent_path(&agent));
    queue_ready(&mut e, 10, &["factory-1"], at()).await;
    e.daemon.flush(at()).await;
    queue_ready(&mut e, 11, &["factory-2"], at()).await;
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
            "coord".to_owned(),
            "board factory: factory-1 is ready (p1)\nboard factory: factory-2 is ready (p1)\nRun board_snapshot and continue."
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
    let (mut e, _) = coordinator_env(AgentState::Running, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_ready(&mut e, 10, &["factory-1"], at()).await;
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
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_ready(&mut e, 10, &["factory-1", "factory-2"], at()).await;
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
        "board factory: factory-2 is ready (p1)\nRun board_snapshot and continue."
    );
    e.daemon.flush(next).await;
    assert_eq!(e.t3.sends().len(), 2, "sent once");

    // A send that keeps failing waits twice as long each time.
    e.t3.fail_send.store(true, Ordering::SeqCst);
    queue_ready(&mut e, 12, &["factory-2"], next).await;
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
    e.t3.threads.lock().unwrap().remove("coord");
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
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    let reads = |e: &Env| e.t3.message_reads.load(Ordering::SeqCst);
    let line = "board factory: factory-1 is ready (p1)\nRun board_snapshot and continue.";
    e.daemon.reconcile(at()).await.unwrap();
    queue_ready(&mut e, 10, &["factory-1"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    assert_eq!(e.t3.sends(), [("coord".to_owned(), line.to_owned())]);
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
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_ready(&mut e, 10, &["factory-1"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.t3.time_out_delivers.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    assert!(unsure_wakes(&e, &agent).is_some());

    // A line queued meanwhile waits for the check, so it goes with the lost lines if any.
    let soon = at() + chrono::Duration::seconds(10);
    queue_ready(&mut e, 11, &["factory-2"], soon).await;
    e.daemon.flush(soon).await;
    assert_eq!(e.t3.sends().len(), 1);

    // Found: settled, and only the new line goes.
    e.daemon.flush(minute(1)).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!(
        sends[1].1,
        "board factory: factory-2 is ready (p1)\nRun board_snapshot and continue."
    );
    assert_eq!(unsure_wakes(&e, &agent), None);
    e.daemon.flush(minute(30)).await;
    assert_eq!(e.t3.sends().len(), 2);
}

#[tokio::test]
async fn a_timed_out_wake_up_waits_for_a_busy_thread_and_a_failed_check_sends_nothing() {
    use board::daemon::WAKE_RETRY_FIRST;
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    let reads = |e: &Env| e.t3.message_reads.load(Ordering::SeqCst);
    e.daemon.reconcile(at()).await.unwrap();
    queue_ready(&mut e, 10, &["factory-1"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    e.t3.time_out_send.store(false, Ordering::SeqCst);

    // A run active or queued may still take it: looked for again a minute later, not on
    // every flush.
    e.t3.running.lock().unwrap().push("coord".to_owned());
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
    queue_ready(&mut e, 11, &["factory-2"], soon).await;
    e.daemon.flush(soon).await;
    assert_eq!(reads(&e), 2);
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!(
        sends[1].1,
        "board factory: factory-2 is ready (p1)\nRun board_snapshot and continue."
    );
    assert_eq!(unsure_wakes(&e, &agent), Some(unsure));

    // Its retry finds it missing: its line is sent again.
    e.daemon.flush(minute(2) + WAKE_RETRY_FIRST).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 3, "{sends:?}");
    assert_eq!(
        sends[2].1,
        "board factory: factory-1 is ready (p1)\nRun board_snapshot and continue."
    );
    assert_eq!(unsure_wakes(&e, &agent), None);
}

#[tokio::test]
async fn a_timed_out_wake_up_is_looked_for_after_a_restart() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_ready(&mut e, 10, &["factory-1"], at()).await;
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
    assert_eq!(restarted.state.coordinators, e.daemon.state.coordinators);
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
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_ready(&mut e, 10, &["factory-1"], at()).await;
    e.daemon.flush(at() + WAKE_TTL).await;
    assert!(e.t3.sends().is_empty(), "not sent at the boundary");
    assert!(e.daemon.state.wakes.is_empty());

    // Saved, then read by a daemon restarted hours later.
    queue_ready(&mut e, 11, &["factory-2"], at()).await;
    e.daemon.save().unwrap();
    let mut restarted = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    restarted.reconcile(at()).await.unwrap();
    restarted.flush(at() + chrono::Duration::hours(5)).await;
    assert!(e.t3.sends().is_empty());
    assert!(restarted.state.wakes.is_empty());
}

#[tokio::test]
async fn a_wake_up_this_version_cannot_read_leaves_the_rest_of_the_state() {
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_ready(&mut e, 10, &["factory-1"], at()).await;
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
    assert_eq!(state.coordinators, e.daemon.state.coordinators);
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

#[tokio::test]
async fn a_refused_renewal_hands_the_coordinator_over() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.stub.on_error(
        "POST /v1/boards/factory/coordinator",
        409,
        "claim_refused",
        "mac-air holds it",
        "wait",
    );
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(11))
        .await
        .unwrap();
    assert!(e.daemon.state.coordinators.is_empty());
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(12))
        .await
        .unwrap();
    assert!(
        e.t3.calls().is_empty(),
        "the live coordinator is not replaced"
    );
    assert_eq!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .len(),
        1
    );
}

fn factory() -> board_proto::BoardSlug {
    "factory".parse().unwrap()
}

fn starts(t3: &FakeT3) -> usize {
    t3.calls()
        .iter()
        .filter(|c| matches!(c, Call::Start { .. }))
        .count()
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

#[tokio::test]
async fn a_failed_start_backs_off_instead_of_looping() {
    let mut e = env();
    e.t3.fail_start.store(true, Ordering::SeqCst);
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    let claims = |e: &Env| {
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .len()
    };
    let secs = |s: i64| at() + chrono::Duration::seconds(s);

    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!((claims(&e), starts(&e.t3)), (1, 1));
    assert_eq!(
        e.stub
            .requests_to("DELETE /v1/boards/factory/coordinator")
            .len(),
        1,
        "the lease is released after the failed start"
    );

    // The daemon's own claim does not ask for another reconcile.
    let mut claimed = event(40, EventType::BoardUpdated, None, "user");
    claimed.payload = json!({"fields": ["coordinator"], "coordinator": null, "device": DEVICE});
    assert!(!e.daemon.on_event(&claimed, secs(1)).await.unwrap());
    let mut patched = event(41, EventType::BoardUpdated, None, "user");
    patched.payload = json!({"fields": ["repos"]});
    assert!(e.daemon.on_event(&patched, secs(1)).await.unwrap());

    // Reconciling again (the release event, the flush tick) waits out the delay.
    for s in [3, 6, 30, 59] {
        e.daemon.reconcile(secs(s)).await.unwrap();
    }
    assert_eq!(claims(&e), 1, "no retry inside the first minute");
    e.daemon.reconcile(secs(61)).await.unwrap();
    assert_eq!((claims(&e), starts(&e.t3)), (2, 2));
    // The delay doubles.
    e.daemon.reconcile(secs(61 + 90)).await.unwrap();
    assert_eq!(claims(&e), 2);
    e.daemon.reconcile(secs(61 + 121)).await.unwrap();
    assert_eq!(claims(&e), 3);

    // A change to the board's hosting settings retries at once, from the first delay.
    e.stub
        .on("GET /v1/boards", [autopilot(None, &[DEVICE, "mac-air"])]);
    e.t3.fail_start.store(false, Ordering::SeqCst);
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    e.daemon.reconcile(secs(61 + 122)).await.unwrap();
    assert_eq!(
        (claims(&e), starts(&e.t3)),
        (5, 4),
        "claim, start, claim as the agent"
    );
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, NEW_THREAD);
}

#[tokio::test]
async fn a_board_whose_start_failed_waits_behind_the_others() {
    let (stuck, other) = ("github.com/o/alpha", "github.com/o/zeta");
    let mut e = env_with_projects(
        [
            (stuck.to_owned(), src("alpha")),
            (other.to_owned(), src("zeta")),
        ]
        .into(),
    );
    e.t3.fail_start_in.lock().unwrap().push(src("alpha"));
    let board = |slug: &str, repo: &str| {
        let mut board = board_fixture(slug, &[repo]);
        board.autopilot = true;
        board.home_devices = vec![DEVICE.parse().unwrap()];
        board
    };
    let (alpha, zeta) = (board("alpha", stuck), board("zeta", other));
    for b in [&alpha, &zeta] {
        let path = format!("/v1/boards/{}/coordinator", b.slug);
        e.stub.on(&format!("POST {path}"), b);
        e.stub.on(&format!("DELETE {path}"), b);
    }
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    let started = |e: &Env| -> Vec<PathBuf> {
        e.t3.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Start { project, .. } => Some(project),
                _ => None,
            })
            .collect()
    };

    e.stub.on("GET /v1/boards", [&alpha]);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(started(&e), [src("alpha")]);

    // Due again, alpha comes after zeta, which sorts after it: a retry that waits out a
    // timeout cannot use the time zeta's start needs.
    e.stub.on("GET /v1/boards", [&alpha, &zeta]);
    e.daemon
        .reconcile(at() + chrono::Duration::seconds(61))
        .await
        .unwrap();
    assert_eq!(started(&e), [src("alpha"), src("zeta"), src("alpha")]);
}

/// An env whose board needs a coordinator, with the lease, its release and registration stubbed.
fn start_env() -> Env {
    let e = env();
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    *e.t3.clock.lock().unwrap() = at() + chrono::Duration::seconds(1);
    e
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
async fn a_start_killed_after_creating_its_thread_briefs_that_thread_after_a_restart() {
    let mut e = start_env();
    // An empty coordinator thread from before, which no start of this daemon made.
    e.t3.list(
        "factory coordinator",
        "old",
        at() - chrono::Duration::days(1),
        false,
    );
    *e.t3.die_after_create.lock().unwrap() = Some(false);

    // The start dies after T3 created its thread, before the message was sent.
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(
        (starts(&e.t3), e.t3.lists()),
        (1, 0),
        "no listing without a mark"
    );
    assert!(!e.daemon.state.coordinators.contains_key(&factory()));
    assert_eq!(
        saved_state(&e).pending_starts.get(&factory()),
        Some(&at()),
        "the mark is on disk before t3 runs"
    );

    // A restarted daemon (no in-memory backoff) briefs that thread instead of starting one.
    *e.t3.die_after_create.lock().unwrap() = None;
    e.daemon = daemon_at(e._dir.path(), &e.stub, projects(), &e.t3);
    e.daemon
        .reconcile(at() + chrono::Duration::seconds(10))
        .await
        .unwrap();
    assert_eq!(starts(&e.t3), 1, "no second thread");
    let lists: Vec<Call> =
        e.t3.calls()
            .into_iter()
            .filter(|c| matches!(c, Call::List { .. }))
            .collect();
    assert_eq!(
        lists,
        [Call::List {
            project: src("agent-board"),
            title: "factory coordinator".into()
        }]
    );
    let sends = e.t3.sends();
    let [(thread, message)] = sends.as_slice() else {
        panic!("{sends:?}");
    };
    assert_eq!(thread, ORPHAN);
    assert_eq!(
        message,
        &board::daemon::coordinator_message(&factory(), false)
    );
    assert!(
        e.t3.listed
            .lock()
            .unwrap()
            .iter()
            .all(|(_, t)| t.ran || t.id == "old"),
        "no thread of this start is left idle"
    );
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, ORPHAN);
    // This binary briefed it.
    assert_eq!(
        e.daemon.state.coordinators[&factory()].briefed.as_deref(),
        Some(board::daemon::prompt_digest(false))
    );
    assert!(saved_state(&e).pending_starts.is_empty());
    let claims = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(claims.last().unwrap().body["agent"], format!("t3:{ORPHAN}"));
}

#[tokio::test]
async fn a_start_that_timed_out_after_delivery_adopts_its_thread_without_briefing_it() {
    let mut e = start_env();
    *e.t3.die_after_create.lock().unwrap() = Some(true);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(releases(&e), 1);

    *e.t3.die_after_create.lock().unwrap() = None;
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(2))
        .await
        .unwrap();
    assert_eq!(starts(&e.t3), 1, "no second thread");
    assert!(e.t3.sends().is_empty(), "its message already got through");
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, ORPHAN);
    // The earlier start briefed it, maybe with another prompt: it is re-briefed.
    assert_eq!(e.daemon.state.coordinators[&factory()].briefed, None);
    assert!(e.daemon.state.pending_starts.is_empty());
}

#[tokio::test]
async fn a_failed_listing_waits_and_a_failed_brief_gives_up_on_that_thread() {
    let mut e = start_env();
    let later = |m: i64| at() + chrono::Duration::minutes(m);
    *e.t3.die_after_create.lock().unwrap() = Some(false);
    e.daemon.reconcile(at()).await.unwrap();
    *e.t3.die_after_create.lock().unwrap() = None;

    // The listing fails: no new thread, the lease is released, the mark kept.
    e.t3.fail_list.store(true, Ordering::SeqCst);
    e.daemon.reconcile(later(40)).await.unwrap();
    assert_eq!((starts(&e.t3), releases(&e)), (1, 2));
    assert_eq!(e.daemon.state.pending_starts.get(&factory()), Some(&at()));

    // Briefing the found thread fails: the lease is released and the mark dropped.
    e.t3.fail_list.store(false, Ordering::SeqCst);
    e.t3.fail_send.store(true, Ordering::SeqCst);
    e.daemon.reconcile(later(80)).await.unwrap();
    let tried: Vec<String> = e.t3.sends().into_iter().map(|(t, _)| t).collect();
    assert_eq!(tried, [ORPHAN]);
    assert_eq!((starts(&e.t3), releases(&e)), (1, 3));
    assert!(saved_state(&e).pending_starts.is_empty());

    // So the next start makes a new thread rather than retrying that one forever.
    e.t3.fail_send.store(false, Ordering::SeqCst);
    e.daemon.reconcile(later(120)).await.unwrap();
    assert_eq!(starts(&e.t3), 2);
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, NEW_THREAD);
}

#[tokio::test]
async fn a_lapsed_lease_is_claimed_again_for_the_live_thread() {
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    // The device slept past the 45-minute lease: the board names no coordinator.
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(Some(&agent), &[DEVICE]),
    );
    let later = at() + chrono::Duration::minutes(50);
    e.daemon.reconcile(later).await.unwrap();

    let claims = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].body, json!({"device": DEVICE, "agent": agent}));
    assert!(
        e.stub.requests_to("POST /v1/agents").is_empty(),
        "already registered"
    );
    assert!(e.t3.calls().is_empty(), "no second thread");
    let local = &e.daemon.state.coordinators[&factory()];
    assert_eq!((local.thread.as_str(), local.renewed_at), ("coord", later));

    // Its thread is gone: a new one replaces it.
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.t3.threads.lock().unwrap().clear();
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(starts(&e.t3), 1);
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, NEW_THREAD);
}

#[tokio::test]
async fn a_failed_registration_is_retried_with_the_same_thread() {
    let mut e = env();
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    e.stub
        .once_error("POST /v1/agents", 503, "internal", "restarting");

    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(starts(&e.t3), 1);
    assert!(
        e.stub
            .requests_to("DELETE /v1/boards/factory/coordinator")
            .is_empty(),
        "the lease is kept for the started thread"
    );
    let local = &e.daemon.state.coordinators[&factory()];
    assert_eq!(
        (local.thread.as_str(), local.registered),
        (NEW_THREAD, false)
    );
    let saved: board::daemon::WatchState =
        board::state::load_named(&Home::at(e._dir.path().join("home")), "watch");
    assert_eq!(saved.coordinators[&factory()].thread, NEW_THREAD, "saved");

    e.daemon
        .reconcile(at() + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(starts(&e.t3), 1, "no second thread");
    assert_eq!(e.stub.requests_to("POST /v1/agents").len(), 2);
    let claims = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[1].body["agent"], format!("t3:{NEW_THREAD}"));
    assert!(e.daemon.state.coordinators[&factory()].registered);
}

#[tokio::test]
async fn an_event_that_failed_on_a_transient_error_is_retried() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.state.last_seq = Some(9);
    let page = |events: Vec<Event>, next_since: i64| board_proto::EventsPage { events, next_since };
    e.stub.on(
        "GET /v1/events",
        page(
            vec![event(10, EventType::TaskCreated, Some("factory-4"), "user")],
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
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
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
            vec![event(11, EventType::TaskCreated, Some("factory-5"), "user")],
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
    e.daemon.state.retry = vec![event(10, EventType::TaskCreated, Some("factory-4"), "user")];
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
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.state.last_seq = Some(9);
    let mut newer =
        serde_json::to_value(event(10, EventType::TaskCreated, Some("factory-4"), "user")).unwrap();
    newer["type"] = json!("board_renamed");
    let known =
        serde_json::to_value(event(11, EventType::TaskCreated, Some("factory-5"), "user")).unwrap();
    e.stub.on(
        "GET /v1/events",
        json!({ "events": [newer, known], "next_since": 11 }),
    );
    e.stub
        .on("GET /v1/tasks/factory-5", detail("factory-5", true, &[]));
    e.daemon.catch_up(at()).await.unwrap();
    assert_eq!(e.daemon.state.last_seq, Some(11));
    assert!(e.stub.requests_to("GET /v1/tasks/factory-4").is_empty());
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends().len(),
        1,
        "the known event still wakes the coordinator"
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

const ADOPTED: &str = "mcp:launch:existing";

fn home_of(e: &Env) -> Home {
    Home::at(e._dir.path().join("home"))
}

/// An env whose device was asked (`board board adopt`) to adopt the live thread [`ADOPTED`].
fn adoption_env() -> Env {
    adoption_env_with(projects())
}

/// [`adoption_env`] with these `[projects]`.
fn adoption_env_with(projects: BTreeMap<String, PathBuf>) -> Env {
    let e = env_with_projects(projects);
    e.t3.live(ADOPTED);
    save_adoption(
        &home_of(&e),
        &factory(),
        &Adoption {
            thread: ADOPTED.into(),
            requested_at: at(),
        },
    )
    .unwrap();
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    e
}

#[tokio::test]
async fn an_adopted_thread_is_briefed_and_named_coordinator_instead_of_a_new_thread() {
    let mut e = adoption_env();
    let agent = format!("t3:{ADOPTED}");

    e.daemon.reconcile(at()).await.unwrap();

    assert_eq!(starts(&e.t3), 0, "no new thread");
    let sends = e.t3.sends();
    let [(thread, brief)] = sends.as_slice() else {
        panic!("{sends:?}");
    };
    assert_eq!(thread, ADOPTED);
    assert!(brief.starts_with("# Coordinator"), "the standing prompt");
    assert!(
        brief.contains("now also the coordinator of board factory"),
        "{brief}"
    );
    assert!(brief.contains("A board task you already own counts as the board's task in progress"));
    assert!(brief.ends_with("Start now: call board_snapshot."));

    let claims = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0].body, json!({"device": DEVICE, "agent": null}));
    assert_eq!(claims[1].body, json!({"device": DEVICE, "agent": agent}));
    assert_eq!(claims[1].agent.as_deref(), Some(agent.as_str()));
    let register = &e.stub.requests_to("POST /v1/agents")[0];
    assert_eq!(register.body["id"], agent);
    assert_eq!(register.body["role"], "coordinator");
    assert_eq!(register.body["device"], DEVICE);
    assert_eq!(register.body["native_ids"]["t3_thread"], ADOPTED);

    let local = &e.daemon.state.coordinators[&factory()];
    assert_eq!((local.thread.as_str(), local.registered), (ADOPTED, true));
    assert_eq!(local.agent.as_str(), agent);
    assert_eq!(load_adoption(&home_of(&e), &factory()), None, "used up");
    let saved = board::daemon::load_watch_state(&home_of(&e));
    assert_eq!(saved.coordinators[&factory()].thread, ADOPTED, "saved");

    // From now on it is this device's coordinator: renewed, never re-briefed or replaced.
    e.stub
        .on("GET /v1/boards", [autopilot(Some(&agent), &[DEVICE])]);
    e.stub.on(
        &format!("GET {}", agent_path(&agent)),
        agent_fixture(&agent, DEVICE, AgentState::Idle),
    );
    e.stub.clear();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(5))
        .await
        .unwrap();
    assert!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .is_empty()
    );
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(11))
        .await
        .unwrap();
    let renew = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(renew.len(), 1);
    assert_eq!(renew[0].body["agent"], agent);

    // A lapsed lease (the device slept) is claimed again for the same thread.
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(Some(&agent), &[DEVICE]),
    );
    e.stub.clear();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(70))
        .await
        .unwrap();
    let claims = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].body, json!({"device": DEVICE, "agent": agent}));

    // A ready task wakes it.
    e.stub
        .on("GET /v1/boards", [autopilot(Some(&agent), &[DEVICE])]);
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(71))
        .await
        .unwrap();
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "t3:worker"),
            at() + chrono::Duration::minutes(71),
        )
        .await
        .unwrap();
    e.daemon.flush(at() + chrono::Duration::minutes(71)).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2, "the brief, then one wake-up: {sends:?}");
    assert_eq!(
        sends[1],
        (
            ADOPTED.to_owned(),
            "board factory: factory-4 is ready (p1)\nRun board_snapshot and continue.".to_owned()
        )
    );
    assert_eq!(starts(&e.t3), 0, "never a new thread");
}

#[tokio::test]
async fn a_failed_brief_releases_the_lease_keeps_the_adoption_and_retries_later() {
    let mut e = adoption_env();
    e.t3.fail_send.store(true, Ordering::SeqCst);
    let secs = |s: i64| at() + chrono::Duration::seconds(s);

    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(e.t3.sends().len(), 1);
    assert_eq!(
        e.stub
            .requests_to("DELETE /v1/boards/factory/coordinator")
            .len(),
        1,
        "the lease is released after the failed brief"
    );
    assert!(e.daemon.state.coordinators.is_empty());
    assert!(load_adoption(&home_of(&e), &factory()).is_some(), "kept");

    // Backs off like a failed start, then briefs once and adopts.
    e.daemon.reconcile(secs(30)).await.unwrap();
    assert_eq!(e.t3.sends().len(), 1, "no retry inside the first minute");
    e.t3.fail_send.store(false, Ordering::SeqCst);
    e.daemon.reconcile(secs(61)).await.unwrap();
    assert_eq!(e.t3.sends().len(), 2);
    assert_eq!(starts(&e.t3), 0);
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, ADOPTED);
    assert_eq!(load_adoption(&home_of(&e), &factory()), None);
}

#[tokio::test]
async fn an_adoption_waits_for_a_live_coordinator_or_another_devices_lease() {
    // A live coordinator: nothing happens, and the adoption stays.
    let mut e = adoption_env();
    e.stub.on(
        "GET /v1/boards",
        [autopilot(Some("t3:elsewhere"), &[DEVICE])],
    );
    e.stub.on(
        &format!("GET {}", agent_path("t3:elsewhere")),
        agent_fixture("t3:elsewhere", "mac-air", AgentState::Idle),
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());
    assert!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .is_empty()
    );
    assert!(load_adoption(&home_of(&e), &factory()).is_some());

    // Another device holds the lease: the thread is not briefed, the adoption stays.
    let mut e = adoption_env();
    e.stub.on_error(
        "POST /v1/boards/factory/coordinator",
        409,
        "claim_refused",
        "mac-air holds it",
        "wait",
    );
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());
    assert!(load_adoption(&home_of(&e), &factory()).is_some());
    assert!(e.daemon.state.coordinators.is_empty());

    // Not a home device: nothing at all.
    let mut e = adoption_env();
    e.stub.on("GET /v1/boards", [autopilot(None, &["mac-air"])]);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());
    assert!(load_adoption(&home_of(&e), &factory()).is_some());
}

#[tokio::test]
async fn an_adoption_of_a_gone_or_archived_thread_is_dropped_for_a_new_thread() {
    for archived in [false, true] {
        let mut e = adoption_env();
        if archived {
            e.t3.threads.lock().unwrap().insert(
                ADOPTED.into(),
                ThreadInfo {
                    archived: true,
                    runtime: Runtime::Claude,
                    worktree: None,
                    runs: None,
                },
            );
        } else {
            e.t3.threads.lock().unwrap().clear();
        }
        e.daemon.reconcile(at()).await.unwrap();
        assert!(e.t3.sends().is_empty(), "the gone thread is not briefed");
        assert_eq!(starts(&e.t3), 1);
        assert_eq!(e.daemon.state.coordinators[&factory()].thread, NEW_THREAD);
        assert_eq!(load_adoption(&home_of(&e), &factory()), None);
    }
}

#[tokio::test]
async fn an_adopted_thread_its_hooks_registered_keeps_its_agent_record() {
    let mut e = adoption_env();
    let agent = format!("t3:{ADOPTED}");
    let mut known = agent_fixture(&agent, DEVICE, AgentState::Paused);
    known.paused_until = Some(at() + chrono::Duration::hours(1));
    e.stub.on(&format!("GET {}", agent_path(&agent)), &known);

    e.daemon.reconcile(at()).await.unwrap();

    assert_eq!(e.t3.sends().len(), 1, "briefed");
    assert!(
        e.stub.requests_to("POST /v1/agents").is_empty(),
        "never re-registered: its repo, branch, cwd and pause stay"
    );
    let claims = e.stub.requests_to("POST /v1/boards/factory/coordinator");
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[1].body["agent"], agent, "named by the claim");
    assert!(e.daemon.state.coordinators[&factory()].registered);
}

#[tokio::test]
async fn an_adoption_claims_the_lease_before_asking_t3() {
    let mut e = adoption_env();
    e.stub.on_error(
        "POST /v1/boards/factory/coordinator",
        409,
        "claim_refused",
        "mac-air holds it",
        "wait",
    );
    // A thread T3 has lost is not looked up, so not dropped, while another device holds it.
    e.t3.threads.lock().unwrap().clear();
    e.daemon.reconcile(at()).await.unwrap();
    assert!(load_adoption(&home_of(&e), &factory()).is_some());
    assert_eq!(starts(&e.t3), 0);
}

#[tokio::test]
async fn a_stale_adoption_is_dropped_for_a_new_thread() {
    let mut e = adoption_env();
    e.daemon
        .reconcile(at() + board::daemon::ADOPTION_TTL)
        .await
        .unwrap();
    assert!(e.t3.sends().is_empty(), "the stale request is not acted on");
    assert_eq!(starts(&e.t3), 1);
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, NEW_THREAD);
    assert_eq!(load_adoption(&home_of(&e), &factory()), None);
}

#[tokio::test]
async fn an_adoption_newer_than_a_failed_start_skips_the_back_off() {
    let mut e = adoption_env();
    board::daemon::remove_adoption(&home_of(&e), &factory()).unwrap();
    e.t3.fail_start.store(true, Ordering::SeqCst);
    let secs = |s: i64| at() + chrono::Duration::seconds(s);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(starts(&e.t3), 1, "the failed start");

    save_adoption(
        &home_of(&e),
        &factory(),
        &Adoption {
            thread: ADOPTED.into(),
            requested_at: secs(10),
        },
    )
    .unwrap();
    e.daemon.reconcile(secs(20)).await.unwrap();
    assert_eq!(e.t3.sends().len(), 1, "briefed inside the back-off");
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, ADOPTED);
}

#[tokio::test]
async fn a_newer_adoption_saved_while_one_is_briefed_is_kept() {
    let mut e = adoption_env();
    let newer = Adoption {
        thread: "mcp:launch:newer".into(),
        requested_at: at() + chrono::Duration::seconds(5),
    };
    *e.t3.adopt_during_send.lock().unwrap() = Some((home_of(&e), newer.clone()));
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(e.daemon.state.coordinators[&factory()].thread, ADOPTED);
    assert_eq!(load_adoption(&home_of(&e), &factory()), Some(newer));
}

#[tokio::test]
async fn a_device_without_the_project_keeps_the_adoption_and_never_claims() {
    let mut e = env_with_projects(BTreeMap::new());
    e.t3.live(ADOPTED);
    save_adoption(
        &home_of(&e),
        &factory(),
        &Adoption {
            thread: ADOPTED.into(),
            requested_at: at(),
        },
    )
    .unwrap();
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.t3.calls().is_empty());
    assert!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .is_empty()
    );
    assert!(load_adoption(&home_of(&e), &factory()).is_some());
}

#[test]
fn the_coordinator_prompt_says_how_to_run_a_climb() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = board::daemon::coordinator_message(&factory(), false);
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
        assert!(words(&prompt).contains(rule), "missing {rule:?}");
    }
    // The stop test is the score command's own, not restated here.
    assert!(!prompt.contains("rounds in a row gain no more than"));
    // Generic: no tool paths of one setup.
    assert!(!prompt.contains("factory/cli.js"));
}

#[test]
fn the_coordinator_prompt_says_when_to_declare_a_revisit_and_how_to_run_it() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = words(&board::daemon::coordinator_message(&factory(), false));
    for rule in [
        // When to declare one, at the finish of the fixed task.
        "File its revisit too when Revisits below calls for one",
        "**Declare** one when you finish a task that changed a tuned value",
        "fixed a feedback item that recurs or that two or more agents reported, or when the user asks",
        "Routine tasks get none",
        // Its fields.
        "File it as a sub-task of the fixed task: it is scope the fix declared, not a proposal",
        "Title it `Revisit: …`, keep it `auto` with the fix's priority",
        "set `not_before` about 14 days after the deploy",
        "Start its body with `Revisit:` and name the deploy time",
        "the feedback items and error signatures it watches, the values under review, and the bar for \"held\"",
        // Its cap.
        "One revisit per fix, and at most three open revisits per board (`task_list` its open tasks titled `Revisit:`)",
        "add the new watch, with its own fixed task and deploy time, to the open revisit due soonest",
        "`task_note` that revisit's id on the fixed task",
        // How to run it.
        "Count the errors with `board errors --since <deploy time>` per watch",
        "say so when the window is cut",
        "read the feedback items routed to it and their reports",
        "`held` when under the bar; `adjust` when over the bar or the values no longer fit",
        "Adjust files a proposal, a child of the revisit, with the counts and the change",
        "for a value the user chose, `ask_user` instead",
        // How to record the outcome.
        "a `decision` note starting `revisit: held` or `revisit: adjust`, with the counts, the window and the bar, on each fixed task it watches and on each watched feedback item",
        "`task_done` the revisit with the counts as evidence",
        "A shipped adjustment gets its own revisit, which names the revisit before it",
        "a second adjustment of one value in a row goes to the user",
    ] {
        assert!(prompt.contains(rule), "missing {rule:?}");
    }
}

#[test]
fn coordinators_ask_the_user_only_for_a_named_reason() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = words(board::daemon::COORDINATOR_PROMPT);
    for rule in [
        "Ask the user (`ask_user`) only for one of these reasons, named in the ask as `Reason: <name>`",
        "`design`",
        "`cost` (new spending of money)",
        "`only-me` (a step that needs the user in person",
        "An `action` ask must be `only-me`",
        "Never ask because a hook or rule blocked a step",
        "to approve a step the user asked for or that follows from it (merging, pushing, deploying, cleaning up)",
        "for a choice with a sensible default (take it, record a `decision` note",
        "when reviewers disagree, a third reviewer decides",
        "Changes to this process and to security posture take the two accepts below, not the user",
        "a split goes to a third reviewer",
    ] {
        assert!(prompt.contains(rule), "missing {rule:?}");
    }
    let triage = words(board::daemon::TRIAGE_PROMPT);
    assert!(triage.contains("route the item to the most likely board"));
    assert!(triage.contains("Do not ask the user."));
}

#[test]
fn the_triage_prompt_routes_repeats_to_an_open_revisit() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let triage = words(board::daemon::TRIAGE_PROMPT);
    for rule in [
        // From PR #83: a recurring item goes to the open revisit, never as a duplicate.
        "A recurring item is never a duplicate of a closed item",
        "When the earlier task has an open revisit (a child task titled `Revisit: …`, or one a note on that task names), route the item to the revisit as the rule above says",
        // The dropped-proposal rule still follows the recurring rule it is named from.
        "when that task was dropped, the next rule applies. When the earlier item was `fixed` or `forwarded` (no task), route it to the setup's owner (see \"Finding the owner\") and cite the earlier item and its reason: a fix that did not hold goes to the owner, not back to you. - When the owner declines or merges a routed proposal",
        // Repeats without `recurs` go there too. A signal is noted on the revisit and keeps
        // its `not_before`, unless it shows p1 harm or already exceeds the whole-window bar.
        "Any item that repeats a problem whose fix has an open revisit goes to that revisit, with or without `recurs`",
        "Find open revisits with `task_list` on the owner's board",
        "`feedback_resolve` with kind `routed` and `task` the revisit, with no new task",
        "When the item is a `signal`, also `task_note` it on the revisit with its count and window",
        "Keep the revisit's `not_before`: a revisit judges a rate over its whole window",
        "so the revisit runs now only in two cases",
        "the signal shows p1 harm on the Priority scale in the coordinator instructions; counts alone never make p1",
        "already exceeds the whole-window bar: the bar's rate times the time from the watched deploy to `not_before`",
    ] {
        assert!(triage.contains(rule), "missing {rule:?}");
    }
    assert!(!triage.contains("also clear the revisit's `not_before`"));
}

#[test]
fn the_coordinator_prompt_reports_to_the_user_apart_from_chat() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = words(&board::daemon::coordinator_message(&factory(), false));
    for rule in [
        "with evidence (each check with its exit code, the commit, the PR) and a `summary` for the user",
        "end with a report of its evidence that names the tasks it filed",
        "The user does not read this chat as it happens",
        "end every turn with a short status",
        "put a question or an offer to the user in `ask_user`, never only in chat",
        "never end a turn on a question",
    ] {
        assert!(prompt.contains(rule), "missing {rule:?}");
    }
}

#[test]
fn the_coordinator_prompt_gates_proposals_behind_an_independent_review() {
    // Compare on words, so rewrapping the Markdown or CRLF checkouts change nothing.
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = words(&board::daemon::coordinator_message(&factory(), false));
    for rule in [
        "give a verdict on each pending proposal on this board that you did not propose",
        "file work it finds but won't do as a proposal",
        "`task_add` with `auto=false`, a body starting `Proposal:`",
        "Every task is `auto` unless the user must do or decide it, or it is a pending proposal",
        "and is not a pending proposal, set it `auto`",
        "File them as proposals and delegate their review to a reviewer child",
        "the next review waits for the next task you finish",
        "A pending proposal is a task with `auto=false`, a body starting `Proposal:`",
        "**Reviewer**: never the proposer",
        "reviewed by the coordinator of the board it was routed to",
        "a `decision` note starting `verdict:`",
        "once it has the accepts it needs",
        "need accepts from two reviewers",
        "Ask once per batch, each item with a recommendation",
        "The user's answer is the verdict",
        // Work that waits on the user reaches them through an ask.
        "Every non-auto task gets one ask or a `not_before`, unless it is a pending proposal",
        "A task blocked on the user gets an ask",
        "never set a task `blocked` on the user without one",
        "When `ask_user` warns of another open ask that asks the same, keep one and withdraw the other",
        // The user reviews waiting work from lists, so each item explains itself.
        "Open each ask, and the body of each task you set non-auto or blocked on the user, with a brief in plain, short sentences",
        "what it is, in one sentence; why it waits on the user; the options, each with its consequence; your recommendation",
        "Agent detail goes below the brief",
    ] {
        assert!(prompt.contains(rule), "missing {rule:?}");
    }
    // The old rule that filed every found task as `auto` is gone.
    assert!(!prompt.contains("File new tasks without `auto=false` otherwise"));

    // Routed feedback reaches the owner as a proposal, not as work.
    let triage = words(&board::daemon::coordinator_message(&factory(), true));
    assert!(triage.contains("(`board` set, `auto=false`): an imperative title;"));
    assert!(triage.contains("a body that starts `Proposal:`"));
    assert!(triage.contains("You are not its reviewer"));
}

#[test]
fn the_coordinator_prompt_parks_waits_in_review_or_blocked() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = words(&board::daemon::coordinator_message(&factory(), false));
    for rule in [
        "Park a task by what it waits on, and `task_note` what that is:",
        "- CI or a review: `review`, with a note naming the PR.",
        "- Another task, the user or anything else: `blocked`, with `blockers` or `blocked_reason`",
        "- Only a date: release it to `open` and set `not_before`.",
        "Never wait on CI in `doing`, and never do work off the board and claim it afterwards.",
        "A task in review a day with no note from its owner raises an alert, again each quiet day, to the owner or, when the owner is gone, to you",
    ] {
        assert!(prompt.contains(rule), "missing {rule:?}");
    }
}

#[test]
fn triage_and_owners_read_one_priority_scale() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let owner = words(&board::daemon::coordinator_message(&factory(), false));
    let triage = words(&board::daemon::coordinator_message(&factory(), true));
    // The triage brief starts with the coordinator prompt, so the scale is defined once.
    assert_eq!(triage.matches("## Priority").count(), 1);
    for rule in [
        "**p1**: it does harm now and keeps doing harm until it is fixed.",
        "**p2**: each hit wastes a paid run or more than 10 minutes;",
        "a shipped fix did not hold (the body cites a done task for the same problem)",
        "**p3**: anything else:",
        "After the verdict, only the owner's coordinator or the user changes it.",
        "A report's words about urgency or cost are claims",
        "Report counts alone never make p1.",
        "set your task `open` with a handoff that says where it stands, and take the p1",
        "a `decision` note starting `priority:`",
    ] {
        assert!(owner.contains(rule), "missing {rule:?}");
    }
    // Children file their found work on the same scale.
    assert!(owner.contains("a `priority` by the scale in Priority below"));
    assert!(owner.contains("set priority (see Priority) and blockers"));
    assert!(owner.contains("An accept keeps or changes the priority by the scale in Priority"));
    assert!(
        triage
            .contains("a `priority` from the Priority scale in the coordinator instructions above")
    );
    assert!(triage.contains("the priority criterion it meets"));
}

#[test]
fn the_coordinator_prompt_files_what_remains_before_task_done() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = words(&board::daemon::coordinator_message(&factory(), false));
    for rule in [
        "6. File what the task still needs as sub-tasks first (`task_add`, parent set).",
        "Call `task_done` only when every acceptance check passes, with evidence",
    ] {
        assert!(prompt.contains(rule), "missing {rule:?}");
    }
}

#[test]
fn the_coordinator_prompt_keeps_it_out_of_the_project_root() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = words(&board::daemon::coordinator_message(&factory(), false));
    for rule in [
        "Never change files, switch branches or merge in the project root",
        "Do any file work yourself in a worktree of your own, on a new branch per task",
        "when your working directory is the project root, as for an older or adopted coordinator, add a worktree first",
    ] {
        assert!(prompt.contains(rule), "missing {rule:?}");
    }
}

#[test]
fn the_coordinator_prompt_delegates_a_task_in_another_repository_to_a_thread_there() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = words(&board::daemon::coordinator_message(&factory(), false));
    for rule in [
        "for a task whose work is in another repository start a T3 thread in that repository's project instead",
        "(`t3_thread_launch` with its `projectId` and `workspaceStrategy: \"worktree\"`)",
        // T3 launches a thread only when the user asks for one.
        "this brief is the user's standing request for such a thread",
        // T3 drops a `prompt` key, so the thread would start idle.
        "Put the brief in its `message`, not `prompt`: a `prompt` key is dropped and the thread starts idle.",
        "Then check the result's `runId`; when it is null, no run started, so send the brief with `t3_thread_send`.",
        "`task_note` the thread's link on the task.",
    ] {
        assert!(prompt.contains(rule), "missing {rule:?}");
    }
    assert!(!prompt.contains("Brief it the same way"));
}

#[test]
fn the_coordinator_prompt_merges_after_ci_while_children_park_and_end() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = words(&board::daemon::coordinator_message(&factory(), false));
    for rule in [
        "open its PR, turn on auto-merge only where the base branch requires status checks or reviews",
        "`gh api repos/<owner>/<repo>/branches/<base>/protection`",
        "`gh api repos/<owner>/<repo>/rules/branches/<base>`",
        "and otherwise leave the PR open, park the task in `review` with a `task_note` naming the PR",
        "Never tell a child to wait for CI or merge after it",
        "The merge is yours, not the child's",
        "elsewhere the PR is still open, so call T3 `watch_pull_request` on the PR, and once its checks pass and your review holds, merge it by the repository's rules or brief a new child to",
    ] {
        assert!(prompt.contains(rule), "missing {rule:?}");
    }
    // In a repository with no required checks, auto-merge would merge before the review.
    assert!(!prompt.contains("turn on auto-merge where the repository's merge rules allow it"));
    // Children cannot wait for CI, so the brief no longer asks them to.
    assert!(!prompt.contains("while CI runs"));
    assert!(!prompt.contains("a live child finishes its own"));
}

#[test]
fn the_coordinator_prompt_names_the_closer_and_parks_a_task_without_a_pr() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = words(&board::daemon::coordinator_message(&factory(), false));
    for rule in [
        "Write each brief as yourself, never in the user's voice",
        "name who closes the task by role and agent id, such as \"the coordinator (t3:1a2b) reviews and closes it\"",
        "or for a task with no PR naming its result (a file, a note id, a report)",
        "A child says a task waits on the user only when the brief says the user must decide or act.",
        "each task in `review` whose PR has merged (or that has no PR) and whose child has ended",
    ] {
        assert!(prompt.contains(rule), "missing {rule:?}");
    }
}

/// Reconciles once with `config` on a home device of the autopilot `board`, whose
/// coordinator lease is free, and returns the fake `t3` and the stub.
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
    stub.on(&format!("POST /v1/boards/{slug}/coordinator"), board);
    stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
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
    let (t3, stub) = start_with(board_fixture("board-feedback", &[]), config).await;
    let (project, _) = started(&t3);
    assert_eq!(project, src("feedback"));
    let register = &stub.requests_to("POST /v1/agents")[0].body;
    assert_eq!(register["role"], "coordinator");
    assert_eq!(register["repo"], serde_json::Value::Null);
    assert_eq!(register["cwd"], src("feedback").to_str().unwrap());

    // A board repo under [projects] wins over [board_projects].
    let config = Config {
        projects: projects(),
        board_projects: [(factory(), src("elsewhere"))].into(),
        ..Config::default()
    };
    let (t3, stub) = start_with(board_fixture("factory", &[REPO]), config).await;
    assert_eq!(started(&t3).0, src("agent-board"));
    assert_eq!(stub.requests_to("POST /v1/agents")[0].body["repo"], REPO);
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
async fn a_git_project_starts_the_coordinator_in_its_own_worktree() {
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
    // The remote's default branch, not `main`, and a stable branch per board.
    let expected = NewWorktree {
        base: "origin/trunk".into(),
        branch: "coordinator/factory".into(),
    };
    assert_eq!(worktree.as_ref(), Some(&expected));

    // A re-start leaves the branch and worktree an earlier coordinator left, which a live
    // thread may still use, and takes a branch of its own.
    let old = dir.path().join("old");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "coordinator/factory",
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
    assert!(branch.starts_with("coordinator/factory-"), "{worktree:?}");
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
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
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
async fn a_project_added_to_config_toml_starts_the_coordinator_without_a_restart() {
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

#[tokio::test]
async fn only_the_feedback_boards_coordinator_is_briefed_to_triage() {
    let triage = board::daemon::TRIAGE_PROMPT;
    assert!(triage.starts_with("# Triage"));

    // The default feedback board.
    let config = Config {
        board_projects: [("board-feedback".parse().unwrap(), src("feedback"))].into(),
        ..Config::default()
    };
    let (t3, _) = start_with(board_fixture("board-feedback", &[]), config).await;
    let message = started(&t3).1;
    assert!(
        message.starts_with(board::daemon::COORDINATOR_PROMPT),
        "{message}"
    );
    assert!(message.contains(triage), "{message}");
    assert!(message.ends_with(
        "You are the coordinator of board board-feedback. Start now: call board_snapshot."
    ));
    assert_eq!(
        message,
        board::daemon::coordinator_message(&"board-feedback".parse().unwrap(), true)
    );

    // Another board is not; nor is `board-feedback` when the device names another one.
    let (t3, _) = start_with(
        board_fixture("factory", &[REPO]),
        Config {
            projects: projects(),
            ..Config::default()
        },
    )
    .await;
    assert!(!started(&t3).1.contains("# Triage"));
    let config = Config {
        feedback_board: Some("fb".parse().unwrap()),
        board_projects: [
            ("board-feedback".parse().unwrap(), src("feedback")),
            ("fb".parse().unwrap(), src("fb")),
        ]
        .into(),
        ..Config::default()
    };
    let (t3, _) = start_with(board_fixture("board-feedback", &[]), config.clone()).await;
    assert!(!started(&t3).1.contains("# Triage"));
    let (t3, _) = start_with(board_fixture("fb", &[]), config).await;
    assert!(started(&t3).1.contains(triage));

    // An adopted thread is briefed the same way.
    let adopted = board::daemon::adoption_message(&"fb".parse().unwrap(), true);
    assert!(adopted.contains(triage));
    assert!(!board::daemon::adoption_message(&factory(), false).contains("# Triage"));
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

/// Named, registered coordinators of boards `slugs`, whose threads `t3:<slug>` are live and
/// whose agents are idle.
fn named_coordinators(e: &mut Env, slugs: &[&str]) {
    let mut boards = Vec::new();
    for slug in slugs {
        let agent = format!("t3:{slug}");
        e.t3.live(slug);
        e.daemon.state.coordinators.insert(
            slug.parse().unwrap(),
            Coordinator {
                thread: (*slug).into(),
                agent: agent.parse().unwrap(),
                started_at: at(),
                renewed_at: at(),
                resumed_after: None,
                registered: true,
                briefed: Some(board::daemon::prompt_digest(false).to_owned()),
                rebrief_backoff: None,
                rebrief_unsure: None,
                fresh: Default::default(),
            },
        );
        let mut board = autopilot(Some(&agent), &[DEVICE]);
        board.slug = slug.parse().unwrap();
        e.stub
            .on(&format!("POST /v1/boards/{slug}/coordinator"), &board);
        e.stub.on(
            &format!("GET {}", agent_path(&agent)),
            agent_fixture(&agent, DEVICE, AgentState::Idle),
        );
        boards.push(board);
    }
    e.stub.on("GET /v1/boards", &boards);
}

#[tokio::test]
async fn hung_thread_checks_run_two_at_a_time_and_wait_before_asking_again() {
    let mut e = env();
    e.daemon.timing.thread_check = Duration::from_millis(300);
    // Checks start only in the first half of the budget: until 450 ms.
    e.daemon.timing.reconcile_budget = Duration::from_millis(900);
    let slugs = ["factory", "other", "third", "fourth", "fifth"];
    named_coordinators(&mut e, &slugs);
    // T3 answers for no thread.
    e.t3.hang_info
        .lock()
        .unwrap()
        .extend(slugs.map(String::from));
    let infos = |e: &Env| e.t3.infos.load(Ordering::SeqCst);
    let renewed = |e: &Env, slug: &str| {
        e.stub
            .requests_to(&format!("POST /v1/boards/{slug}/coordinator"))
            .len()
    };

    // Two checks start at 0 and two at 300 ms, each timing out after 300 ms; the fifth's
    // turn comes at 600 ms, past the deadline, so it never starts.
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(11))
        .await
        .unwrap();
    assert_eq!(
        e.t3.infos_at_once.load(Ordering::SeqCst),
        2,
        "two at a time"
    );
    assert_eq!(infos(&e), 4, "none started after the deadline");
    for slug in &slugs[..4] {
        assert_eq!(
            renewed(&e, slug),
            1,
            "{slug}: renewed, a check that never answers being no sign the thread is gone"
        );
    }
    assert_eq!(
        renewed(&e, "fifth"),
        0,
        "not checked, so left for the next reconcile"
    );
    assert_eq!(e.daemon.state.coordinators.len(), 5, "none replaced");

    // A minute later only the board not checked is asked: the timed-out ones wait.
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(12))
        .await
        .unwrap();
    assert_eq!(infos(&e), 5);
    assert_eq!(e.daemon.state.coordinators.len(), 5);

    // THREAD_RECHECK after their timeout, the first four are asked again; the fifth waits.
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(16))
        .await
        .unwrap();
    assert_eq!(infos(&e), 9);
    assert_eq!(e.daemon.state.coordinators.len(), 5);
}

#[tokio::test]
async fn a_hung_thread_check_holds_only_its_own_slot() {
    let mut e = env();
    e.daemon.timing.thread_check = Duration::from_millis(300);
    // Checks start only in the first 100 ms, before the hung one times out.
    e.daemon.timing.reconcile_budget = Duration::from_millis(200);
    let slugs = ["factory", "other", "third", "fourth", "fifth"];
    named_coordinators(&mut e, &slugs);
    e.t3.hang_info.lock().unwrap().push("factory".into());
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(11))
        .await
        .unwrap();
    assert_eq!(
        e.t3.infos.load(Ordering::SeqCst),
        5,
        "the answered checks freed their slot at once"
    );
    for slug in slugs {
        assert_eq!(
            e.stub
                .requests_to(&format!("POST /v1/boards/{slug}/coordinator"))
                .len(),
            1,
            "{slug}: renewed"
        );
    }
}

#[tokio::test]
async fn a_coordinator_whose_checks_time_out_is_still_replaced_once_t3_answers() {
    let archive = |e: &Env| {
        e.t3.hang_info.lock().unwrap().clear();
        e.t3.threads.lock().unwrap().insert(
            "coord".into(),
            ThreadInfo {
                archived: true,
                runtime: Runtime::Claude,
                worktree: None,
                runs: None,
            },
        );
    };
    let lease = |e: &Env, method: &str| {
        e.stub
            .requests_to(&format!("{method} /v1/boards/factory/coordinator"))
            .len()
    };
    let infos = |e: &Env| e.t3.infos.load(Ordering::SeqCst);
    let reconcile = async |e: &mut Env, minutes: i64| {
        e.stub.clear();
        e.daemon
            .reconcile(at() + chrono::Duration::minutes(minutes))
            .await
            .unwrap();
    };

    // At work: the timed-out check renews it. Its thread is archived meanwhile, and the
    // next check, THREAD_RECHECK later, replaces it.
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.timing.thread_check = Duration::from_millis(50);
    e.t3.hang_info.lock().unwrap().push("coord".into());
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    reconcile(&mut e, 11).await;
    assert_eq!(
        (infos(&e), lease(&e, "POST"), lease(&e, "DELETE")),
        (1, 1, 0)
    );
    archive(&e);
    reconcile(&mut e, 12).await;
    assert_eq!(infos(&e), 1, "not asked again yet");
    assert_eq!(starts(&e.t3), 0);
    reconcile(&mut e, 16).await;
    assert_eq!(infos(&e), 3, "asked, then the new thread's runtime read");
    assert_eq!(lease(&e, "DELETE"), 1, "the archived thread is released");
    assert_eq!(starts(&e.t3), 1, "and replaced");

    // Its session ended: timed-out checks, and the waits between them, still count toward
    // CHECK_FAIL_RELEASE, so the lease goes 10 minutes after the first timeout.
    let (mut e, _) = coordinator_env(AgentState::Ended, None);
    e.daemon.timing.thread_check = Duration::from_millis(50);
    e.t3.hang_info.lock().unwrap().push("coord".into());
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    for (minutes, asked) in [(11, 1), (12, 1), (16, 2)] {
        reconcile(&mut e, minutes).await;
        assert_eq!(infos(&e), asked, "at +{minutes}");
        assert_eq!(
            (lease(&e, "POST"), lease(&e, "DELETE")),
            (0, 0),
            "at +{minutes}"
        );
    }
    reconcile(&mut e, 21).await;
    assert_eq!(infos(&e), 3);
    assert_eq!(lease(&e, "DELETE"), 1, "released after CHECK_FAIL_RELEASE");
    // No one takes the board, and a thread the board does not name is asked each minute,
    // so it is adopted or replaced as soon as T3 answers: here it is archived.
    e.stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    archive(&e);
    reconcile(&mut e, 22).await;
    assert_eq!((infos(&e), starts(&e.t3)), (5, 1), "asked, then replaced");

    // Its agent ending means its thread may be gone: the next reconcile asks at once.
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    e.daemon.timing.thread_check = Duration::from_millis(50);
    e.t3.hang_info.lock().unwrap().push("coord".into());
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    reconcile(&mut e, 11).await;
    archive(&e);
    let mut ended = event(10, EventType::AgentStateChanged, None, &agent);
    ended.payload = json!({"from": "idle", "to": "ended"});
    e.daemon
        .on_event(&ended, at() + chrono::Duration::minutes(12))
        .await
        .unwrap();
    reconcile(&mut e, 12).await;
    assert_eq!(starts(&e.t3), 1, "replaced without waiting");

    // A wake-up that T3 refuses because the thread is archived means T3 answers again: the
    // next reconcile asks at once instead of waiting out THREAD_RECHECK.
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.timing.thread_check = Duration::from_millis(50);
    e.t3.hang_info.lock().unwrap().push("coord".into());
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("t3:x", DEVICE, AgentState::Running),
    );
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    reconcile(&mut e, 11).await;
    archive(&e);
    e.t3.refuse_send.lock().unwrap().push("coord".into());
    let now = at() + chrono::Duration::minutes(12);
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "t3:worker"),
            now,
        )
        .await
        .unwrap();
    e.daemon.flush(now).await;
    assert_eq!(e.t3.sends().len(), 1);
    assert_eq!(infos(&e), 2, "the failed send asked T3 about the thread");
    reconcile(&mut e, 13).await;
    assert_eq!(starts(&e.t3), 1, "replaced without waiting");
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
fn a_coordinator_start_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    stub.on("GET /healthz", json!({"ok": true, "seq": 5}));
    stub.on("GET /v1/boards", [autopilot(None, &[DEVICE])]);
    stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    stub.on(
        "POST /v1/agents",
        agent_fixture(&format!("t3:{NEW_THREAD}"), DEVICE, AgentState::Running),
    );
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
            "started coordinator thread {NEW_THREAD} for factory"
        )),
        "{log}"
    );
    assert!(
        log.contains(&format!("t3:{NEW_THREAD} coordinates factory")),
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

#[tokio::test]
async fn a_send_backing_off_still_passes_answers_on_to_the_coordinator() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_answer(&mut e, 30, 3, "t3:worker", at()).await;
    e.t3.live("worker");
    e.t3.fail_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    assert_eq!(threads(&e.t3), ["worker"]);
    let worker = &e.daemon.state.wakes[&"t3:worker".parse().unwrap()];
    assert!(worker.backoff.is_some(), "the send backs off");
    e.t3.fail_send.store(false, Ordering::SeqCst);

    // The worker's session ended while its send backed off, its thread still in T3: the
    // answer stays the worker's, sent once the back-off ends.
    let ended = state_changed(31, "t3:worker", AgentState::Ended, None);
    e.daemon.on_event(&ended, at()).await.unwrap();
    e.daemon.flush(at() + chrono::Duration::seconds(3)).await;
    e.daemon.flush(at() + chrono::Duration::seconds(4)).await;
    assert_eq!(threads(&e.t3), ["worker"]);
    let worker = &e.daemon.state.wakes[&"t3:worker".parse().unwrap()];
    assert_eq!(worker.wakes.len(), 1);
    assert!(!worker.wakes[0].forwarded, "not passed to the coordinator");
    assert!(
        !e.daemon
            .state
            .wakes
            .contains_key(&"t3:coord".parse().unwrap())
    );

    // Its thread is archived meanwhile, which no event announces: once the back-off ends,
    // the send fails, T3 says the thread is gone, and the answer goes to the coordinator.
    e.t3.threads.lock().unwrap().remove("worker");
    e.t3.refuse_send.lock().unwrap().push("worker".to_owned());
    e.daemon.flush(at() + chrono::Duration::seconds(31)).await;
    assert_eq!(threads(&e.t3), ["worker", "worker"]);
    assert!(
        !e.daemon
            .state
            .wakes
            .contains_key(&"t3:worker".parse().unwrap()),
        "dropped with its thread"
    );
    e.daemon.flush(at() + chrono::Duration::seconds(32)).await;
    assert_eq!(threads(&e.t3), ["worker", "worker", "coord"]);
    assert!(
        e.t3.sends()[2].1.contains("ask #3 by t3:worker"),
        "{:?}",
        e.t3.sends()
    );
}

#[tokio::test]
async fn coordinators_are_kept_while_the_event_stream_cannot_open() {
    let mut e = following_env(Duration::from_secs(5));
    e.daemon.timing.reconcile_every = Duration::from_millis(200);
    e.stub
        .on_error("GET /v1/events", 503, "unavailable", "restarting", "retry");
    let agent = "t3:coord";
    e.t3.live("coord");
    e.daemon.state.coordinators.insert(
        "factory".parse().unwrap(),
        Coordinator {
            thread: "coord".into(),
            agent: agent.parse().unwrap(),
            started_at: at(),
            renewed_at: at(),
            resumed_after: None,
            registered: true,
            briefed: Some(board::daemon::prompt_digest(false).to_owned()),
            rebrief_backoff: None,
            rebrief_unsure: None,
            fresh: Default::default(),
        },
    );
    e.stub
        .on("GET /v1/boards", [autopilot(Some(agent), &[DEVICE])]);
    e.stub.on(
        &format!("GET {}", agent_path(agent)),
        agent_fixture(agent, DEVICE, AgentState::Idle),
    );
    e.stub.on(
        "POST /v1/boards/factory/coordinator",
        autopilot(Some(agent), &[DEVICE]),
    );

    let (reached, _) = follow_until(&mut e, Duration::from_secs(3), |stub, _| {
        stub.requests_to("GET /v1/boards").len() >= 3
    })
    .await;
    assert!(
        reached < Duration::from_secs(2),
        "reconciled at start and on its tick: {reached:?}"
    );
    assert_eq!(
        e.stub
            .requests_to("POST /v1/boards/factory/coordinator")
            .len(),
        1,
        "the due lease was renewed"
    );
}

#[tokio::test]
async fn a_coordinator_briefed_with_an_older_prompt_gets_the_new_one_once() {
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    let slug = factory();
    // Briefed before this version, or with an older prompt.
    for old in [None, Some("0123".to_owned())] {
        e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = old;
        let before = e.t3.sends().len();
        e.daemon.reconcile(at()).await.unwrap();
        let sends = e.t3.sends();
        assert_eq!(sends.len(), before + 1);
        let (thread, text) = sends.last().unwrap();
        assert_eq!(thread, "coord");
        assert_eq!(*text, board::daemon::rebrief_message(&slug, false));
        assert!(text.contains(board::daemon::COORDINATOR_PROMPT));
        assert!(!text.contains("# Triage"));
        assert_eq!(
            e.daemon.state.coordinators[&slug].briefed,
            Some(board::daemon::prompt_digest(false).to_owned())
        );
        // Saved, so a restart does not send it again.
        let home = Home::at(e._dir.path().join("home"));
        assert_eq!(
            board::daemon::load_watch_state(&home).coordinators[&slug].briefed,
            Some(board::daemon::prompt_digest(false).to_owned())
        );
        // Briefed with the current prompt: later reconciles send nothing.
        e.daemon.reconcile(at()).await.unwrap();
        assert_eq!(e.t3.sends().len(), before + 1);
    }

    // Held while the coordinator is paused, or runs and was seen lately.
    let briefed = |e: &Env| e.daemon.state.coordinators[&slug].briefed.clone();
    let current = Some(board::daemon::prompt_digest(false).to_owned());
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
    for held in [AgentState::Running, AgentState::Paused] {
        let mut info = agent_fixture(&agent, DEVICE, held);
        info.paused_until = None;
        e.stub.on(&format!("GET {}", agent_path(&agent)), &info);
        e.daemon.reconcile(at()).await.unwrap();
        assert_eq!(e.t3.sends().len(), 2, "{held:?}");
    }
    e.stub.on(
        &format!("GET {}", agent_path(&agent)),
        agent_fixture(&agent, DEVICE, AgentState::Idle),
    );
    // A failed send backs off, then is tried again.
    e.t3.fail_send.store(true, Ordering::SeqCst);
    e.daemon.reconcile(at()).await.unwrap();
    e.t3.fail_send.store(false, Ordering::SeqCst);
    assert_eq!(briefed(&e), None);
    assert_eq!(e.t3.sends().len(), 3);
    e.daemon
        .reconcile(at() + chrono::Duration::seconds(10))
        .await
        .unwrap();
    assert_eq!(e.t3.sends().len(), 3, "backing off");
    let later = at() + chrono::Duration::seconds(31);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(e.t3.sends().len(), 4);
    assert_eq!(briefed(&e), current);

    // Marked running but not seen for longer than a wake-up is held: sent.
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
    let mut stuck = agent_fixture(&agent, DEVICE, AgentState::Running);
    stuck.last_seen = later - board::daemon::WAKE_HOLD_MAX - chrono::Duration::seconds(1);
    e.stub.on(&format!("GET {}", agent_path(&agent)), &stuck);
    e.daemon.reconcile(later).await.unwrap();
    assert_eq!(e.t3.sends().len(), 5);
    assert_eq!(briefed(&e), current);

    // A send that timed out but got through counts as sent and is not repeated (see
    // `a_timed_out_re_brief_is_sent_again_only_when_the_thread_never_got_it`).
    // Only this send can have put the message in the thread.
    e.t3.received.lock().unwrap().clear();
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.t3.time_out_delivers.store(true, Ordering::SeqCst);
    e.daemon.reconcile(later).await.unwrap();
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    assert_eq!(e.t3.sends().len(), 6);
    assert_eq!(briefed(&e), current);
    let checked = later + board::daemon::SEND_CHECK_AFTER;
    e.daemon.reconcile(checked).await.unwrap();
    assert_eq!(e.t3.sends().len(), 6);
    assert_eq!(e.daemon.state.coordinators[&slug].rebrief_unsure, None);
}

#[tokio::test]
async fn the_feedback_boards_coordinator_is_re_briefed_with_the_triage_prompt() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    let slug = factory();
    // This device's feedback board, set by editing config.toml while the daemon runs.
    let home = Home::at(e._dir.path().join("home"));
    std::fs::create_dir_all(home.config_path().parent().unwrap()).unwrap();
    std::fs::write(home.config_path(), "feedback_board = \"factory\"\n").unwrap();
    e.daemon.reconcile(at()).await.unwrap();
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].1, board::daemon::rebrief_message(&slug, true));
    assert!(sends[0].1.contains(board::daemon::TRIAGE_PROMPT));
    assert_eq!(
        e.daemon.state.coordinators[&slug].briefed,
        Some(board::daemon::prompt_digest(true).to_owned())
    );
    assert_ne!(
        board::daemon::prompt_digest(true),
        board::daemon::prompt_digest(false)
    );
}

#[tokio::test]
async fn a_started_coordinator_is_recorded_as_briefed_with_the_current_prompt() {
    let (_, _, daemon, _dir) = start_daemon(
        board_fixture("factory", &[REPO]),
        Config {
            projects: projects(),
            ..Config::default()
        },
    )
    .await;
    assert_eq!(
        daemon.state.coordinators[&factory()].briefed,
        Some(board::daemon::prompt_digest(false).to_owned())
    );
}

/// The `factory` board as the coordinator env lists it, with these counts.
fn counted(agent: &str, doing: u32, ready_auto: Option<u32>, open_asks: u32) -> Board {
    let mut board = autopilot(Some(agent), &[DEVICE]);
    board.counts.doing = doing;
    board.counts.ready = ready_auto.unwrap_or(0);
    board.counts.ready_auto = ready_auto;
    board.open_asks = open_asks;
    board
}

#[tokio::test]
async fn an_idle_boards_coordinator_gets_the_new_prompt_with_its_next_wake_up() {
    use board::daemon::WAKE_RETRY_FIRST;
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    let slug = factory();
    let current = Some(board::daemon::prompt_digest(false).to_owned());
    let briefed = |e: &Env| e.daemon.state.coordinators[&slug].briefed.clone();
    let old = Some("0123".to_owned());
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = old.clone();
    e.stub
        .on("GET /v1/boards", [counted(&agent, 0, Some(0), 0)]);

    // Nothing to do on the board: no turn spent on the new prompt, which stays owed.
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());
    assert_eq!(briefed(&e), old);

    // The next wake-up carries it, in front of its lines; a failed send leaves it owed.
    queue_ready(&mut e, 10, &["factory-4"], at()).await;
    let ready = "board factory: factory-4 is ready (p1)\nRun board_snapshot and continue.";
    let expected = format!(
        "{}\n\n{ready}",
        board::daemon::rebrief_message(&slug, false)
    );
    e.t3.fail_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    e.t3.fail_send.store(false, Ordering::SeqCst);
    assert_eq!(e.t3.sends(), [("coord".to_owned(), expected.clone())]);
    assert_eq!(briefed(&e), old);
    // The wake-up's back-off times the retry, which carries it again. Meanwhile the
    // board got work, but a reconcile leaves the prompt to the queued wake-up.
    e.stub
        .on("GET /v1/boards", [counted(&agent, 0, Some(1), 0)]);
    e.daemon.reconcile(at()).await.unwrap();
    assert_eq!(e.t3.sends().len(), 1);
    assert_eq!(e.daemon.state.coordinators[&slug].rebrief_backoff, None);
    let next = at() + WAKE_RETRY_FIRST;
    e.daemon.flush(next).await;
    assert_eq!(e.t3.sends().len(), 2);
    assert_eq!(e.t3.sends()[1], ("coord".to_owned(), expected));
    assert_eq!(briefed(&e), current);

    // Briefed now: a reconcile of the board with work, and later wake-ups, send no prompt.
    e.daemon.reconcile(next).await.unwrap();
    queue_ready(&mut e, 11, &["factory-5"], next).await;
    e.daemon.flush(next).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 3, "{sends:?}");
    assert_eq!(
        sends[2].1,
        "board factory: factory-5 is ready (p1)\nRun board_snapshot and continue."
    );

    // A send that timed out may have got through: the prompt counts as sent. (It did, so
    // the check a minute later finds it and the next lines go on their own.)
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
    queue_ready(&mut e, 12, &["factory-6"], next).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.t3.time_out_delivers.store(true, Ordering::SeqCst);
    e.daemon.flush(next).await;
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    assert_eq!(e.t3.sends().len(), 4);
    assert!(
        e.t3.sends()[3]
            .1
            .contains(board::daemon::COORDINATOR_PROMPT)
    );
    assert_eq!(briefed(&e), current);

    // A thread that coordinates two boards gets each prompt it is owed.
    let next = next + board::daemon::SEND_CHECK_AFTER;
    let other: board_proto::BoardSlug = "other".parse().unwrap();
    let mut second = e.daemon.state.coordinators[&slug].clone();
    second.briefed = None;
    e.daemon.state.coordinators.insert(other.clone(), second);
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
    queue_ready(&mut e, 13, &["factory-7"], next).await;
    e.daemon.flush(next).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 5);
    assert_eq!(
        sends[4].1,
        format!(
            "{}\n\n{}\n\nboard factory: factory-7 is ready (p1)\nRun board_snapshot and continue.",
            board::daemon::rebrief_message(&slug, false),
            board::daemon::rebrief_message(&other, false)
        )
    );
    assert_eq!(e.daemon.state.coordinators[&other].briefed, current);
    assert_eq!(briefed(&e), current);
}

#[tokio::test]
async fn a_board_with_work_or_unknown_ready_work_re_briefs_its_coordinator_at_once() {
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    let slug = factory();
    let current = Some(board::daemon::prompt_digest(false).to_owned());
    let mut in_review = counted(&agent, 0, Some(0), 0);
    in_review.counts.review = 1;
    for (n, board) in [
        counted(&agent, 0, Some(2), 0),
        counted(&agent, 1, Some(0), 0),
        in_review,
        counted(&agent, 0, Some(0), 1),
        // A server from before `ready_auto`: the ready work is unknown.
        counted(&agent, 0, None, 0),
    ]
    .into_iter()
    .enumerate()
    {
        e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
        e.stub.on("GET /v1/boards", [board.clone()]);
        e.daemon.reconcile(at()).await.unwrap();
        let sends = e.t3.sends();
        assert_eq!(sends.len(), n + 1, "{:?}", board.counts);
        assert_eq!(
            sends[n],
            (
                "coord".to_owned(),
                board::daemon::rebrief_message(&slug, false)
            )
        );
        assert_eq!(e.daemon.state.coordinators[&slug].briefed, current);
    }
    // Its next wake-up carries no prompt.
    queue_ready(&mut e, 10, &["factory-4"], at()).await;
    e.daemon.flush(at()).await;
    assert_eq!(
        e.t3.sends()[5].1,
        "board factory: factory-4 is ready (p1)\nRun board_snapshot and continue."
    );
}

#[tokio::test]
async fn an_idle_coordinator_resumed_at_a_usage_reset_gets_the_new_prompt_with_it() {
    let reset = at() + chrono::Duration::hours(1);
    let (mut e, agent) = coordinator_env(AgentState::Ended, Some(reset));
    let slug = factory();
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
    e.stub
        .on("GET /v1/boards", [counted(&agent, 0, Some(0), 0)]);
    let later = reset + chrono::Duration::minutes(1);
    e.daemon.reconcile(later).await.unwrap();
    e.daemon.reconcile(later).await.unwrap();
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 1, "{sends:?}");
    assert_eq!(
        sends[0].1,
        format!(
            "{}\n\nboard factory: the usage limit has reset. Run board_snapshot and continue your loop.",
            board::daemon::rebrief_message(&slug, false)
        )
    );
    assert_eq!(
        e.daemon.state.coordinators[&slug].briefed,
        Some(board::daemon::prompt_digest(false).to_owned())
    );
}

/// Whether `factory`'s coordinator waits to learn if a timed-out re-brief reached it.
fn unsure(e: &Env) -> Option<board::daemon::UnsureRebrief> {
    e.daemon.state.coordinators[&factory()]
        .rebrief_unsure
        .clone()
}

fn minute(n: i64) -> Timestamp {
    at() + chrono::Duration::minutes(n)
}

#[tokio::test]
async fn a_timed_out_re_brief_is_sent_again_only_when_the_thread_never_got_it() {
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    let slug = factory();
    let current = Some(board::daemon::prompt_digest(false).to_owned());
    let message = board::daemon::rebrief_message(&slug, false);
    let reads = |e: &Env| e.t3.message_reads.load(Ordering::SeqCst);
    let agent_is = |e: &Env, info: &board_proto::Agent| {
        e.stub.on(&format!("GET {}", agent_path(&agent)), info);
    };
    assert!(message.ends_with(&board::daemon::rebrief_marker(&slug, false)));

    // The send times out and T3 drops it: counted as sent for now, and saved as unsure.
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.reconcile(at()).await.unwrap();
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    assert_eq!(e.t3.sends(), [("coord".to_owned(), message.clone())]);
    assert_eq!(e.daemon.state.coordinators[&slug].briefed, current);
    let home = Home::at(e._dir.path().join("home"));
    let saved = board::daemon::load_watch_state(&home).coordinators[&slug].clone();
    assert_eq!(saved.rebrief_unsure, unsure(&e));
    assert!(unsure(&e).is_some_and(|u| !u.resent && u.since == at()));

    // Not looked for while T3 may still be handling the timed-out request.
    e.daemon
        .reconcile(at() + chrono::Duration::seconds(30))
        .await
        .unwrap();
    assert_eq!(reads(&e), 0);

    // Looked for while the coordinator works too. Missing while T3 has a run active or
    // queued, which may take it as its next turn: not owed yet.
    let mut running = agent_fixture(&agent, DEVICE, AgentState::Running);
    running.last_seen = minute(2);
    agent_is(&e, &running);
    e.t3.running.lock().unwrap().push("coord".to_owned());
    e.daemon.reconcile(minute(1)).await.unwrap();
    assert_eq!(reads(&e), 1);
    assert!(unsure(&e).is_some_and(|u| !u.resent));
    e.t3.running.lock().unwrap().clear();

    // Missing with no run: owed again, sent once the coordinator is between turns.
    e.daemon.reconcile(minute(2)).await.unwrap();
    assert_eq!((reads(&e), e.t3.sends().len()), (2, 1));
    assert_eq!(e.daemon.state.coordinators[&slug].briefed, None);
    assert!(unsure(&e).is_some_and(|u| u.resent));
    agent_is(&e, &agent_fixture(&agent, DEVICE, AgentState::Idle));
    e.daemon.reconcile(minute(3)).await.unwrap();
    assert_eq!((reads(&e), e.t3.sends().len()), (2, 2));
    assert_eq!(e.t3.sends()[1], ("coord".to_owned(), message.clone()));
    assert_eq!(e.daemon.state.coordinators[&slug].briefed, current);
    assert_eq!(unsure(&e), None);
    e.daemon.reconcile(minute(4)).await.unwrap();
    assert_eq!((reads(&e), e.t3.sends().len()), (2, 2));

    // A resend that times out too counts as delivered: never a third send.
    e.t3.received.lock().unwrap().clear();
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.reconcile(minute(5)).await.unwrap();
    e.daemon.reconcile(minute(6)).await.unwrap();
    assert_eq!((reads(&e), e.t3.sends().len()), (3, 4));
    assert_eq!(unsure(&e), None);
    e.daemon.reconcile(minute(8)).await.unwrap();
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    assert_eq!((reads(&e), e.t3.sends().len()), (3, 4));
    assert_eq!(e.daemon.state.coordinators[&slug].briefed, current);
}

#[tokio::test]
async fn a_timed_out_re_brief_that_reached_the_thread_is_not_sent_twice() {
    use board::daemon::WAKE_RETRY_FIRST;
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    let slug = factory();
    let reads = |e: &Env| e.t3.message_reads.load(Ordering::SeqCst);
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.t3.time_out_delivers.store(true, Ordering::SeqCst);
    e.daemon.reconcile(at()).await.unwrap();
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    assert_eq!(e.t3.sends().len(), 1);
    assert!(unsure(&e).is_some());

    // The check fails: it backs off and sends nothing, since the message may be there.
    e.t3.fail_messages.store(true, Ordering::SeqCst);
    e.daemon.reconcile(minute(1)).await.unwrap();
    e.t3.fail_messages.store(false, Ordering::SeqCst);
    assert_eq!((reads(&e), e.t3.sends().len()), (1, 1));
    assert!(unsure(&e).is_some());
    let backoff = e.daemon.state.coordinators[&slug].rebrief_backoff.clone();
    assert_eq!(
        backoff.map(|b| b.retry_at),
        Some(minute(1) + WAKE_RETRY_FIRST)
    );
    e.daemon
        .reconcile(minute(1) + chrono::Duration::seconds(10))
        .await
        .unwrap();
    assert_eq!(reads(&e), 1, "backing off");

    // Found in the thread: settled, never sent again.
    e.daemon.reconcile(minute(2)).await.unwrap();
    assert_eq!((reads(&e), e.t3.sends().len()), (2, 1));
    assert_eq!(unsure(&e), None);
    assert_eq!(e.daemon.state.coordinators[&slug].rebrief_backoff, None);
    e.daemon.reconcile(minute(3)).await.unwrap();
    assert_eq!((reads(&e), e.t3.sends().len()), (2, 1));
    assert_eq!(
        e.daemon.state.coordinators[&slug].briefed,
        Some(board::daemon::prompt_digest(false).to_owned())
    );
}

#[tokio::test]
async fn only_the_newest_re_brief_of_the_board_in_the_thread_counts() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    let slug = factory();
    let digest = board::daemon::prompt_digest(false).to_owned();
    let message = board::daemon::rebrief_message(&slug, false);
    let look = |e: &mut Env, newer: &str| {
        e.t3.received
            .lock()
            .unwrap()
            .insert("coord".to_owned(), vec![message.clone(), newer.to_owned()]);
        let local = e.daemon.state.coordinators.get_mut(&factory()).unwrap();
        local.briefed = Some(digest.clone());
        local.rebrief_unsure = Some(board::daemon::UnsureRebrief {
            digest: digest.clone(),
            resent: false,
            since: at(),
        });
    };
    // A later re-brief of another board leaves this one found.
    look(&mut e, "...\n(re-brief 0123456789abcdef of board other)");
    e.daemon.reconcile(minute(1)).await.unwrap();
    assert_eq!(unsure(&e), None);
    assert!(e.t3.sends().is_empty());
    // A later re-brief of this board with another prompt replaced it: this one is missing.
    look(&mut e, "...\n(re-brief 0123456789abcdef of board factory)");
    e.daemon.reconcile(minute(2)).await.unwrap();
    assert_eq!(e.t3.sends(), [("coord".to_owned(), message.clone())]);
}

#[tokio::test]
async fn a_lost_wake_up_with_a_re_brief_is_sent_again_as_one_message() {
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    let slug = factory();
    let current = Some(board::daemon::prompt_digest(false).to_owned());
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
    e.stub
        .on("GET /v1/boards", [counted(&agent, 0, Some(0), 0)]);
    e.daemon.reconcile(at()).await.unwrap();
    assert!(
        e.t3.sends().is_empty(),
        "the idle board waits for a wake-up"
    );
    queue_ready(&mut e, 10, &["factory-4"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    assert_eq!(e.t3.sends().len(), 1);
    // The message tracks its re-brief: the reconcile leaves it to the flush.
    assert_eq!(unsure(&e), None);
    let message = &unsure_wakes(&e, &agent).unwrap().messages[0];
    assert_eq!(message.rebriefs.len(), 1);
    assert_eq!(message.wakes.len(), 1);
    let reads = e.t3.message_reads.load(Ordering::SeqCst);
    e.daemon.reconcile(minute(1)).await.unwrap();
    assert_eq!(e.t3.message_reads.load(Ordering::SeqCst), reads);
    assert_eq!(e.daemon.state.coordinators[&slug].briefed, current);

    // Missing: the re-brief and the line go again together, with the lines queued since.
    queue_ready(&mut e, 11, &["factory-5"], minute(1)).await;
    e.daemon.flush(minute(1)).await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(
        sends[1].1,
        format!(
            "{}\n\nboard factory: factory-5 is ready (p1)\nboard factory: factory-4 is ready (p1)\n\
             Run board_snapshot and continue.",
            board::daemon::rebrief_message(&slug, false)
        )
    );
    assert_eq!(e.daemon.state.coordinators[&slug].briefed, current);
    assert_eq!(unsure(&e), None);
    assert_eq!(unsure_wakes(&e, &agent), None);
    e.daemon.reconcile(minute(2)).await.unwrap();
    e.daemon.flush(minute(2)).await;
    assert_eq!(e.t3.sends().len(), 2);
}

#[tokio::test]
async fn a_second_timeout_before_the_check_keeps_both_messages() {
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    let line =
        |id: &str| format!("board factory: {id} is ready (p1)\nRun board_snapshot and continue.");
    e.daemon.reconcile(at()).await.unwrap();
    queue_ready(&mut e, 10, &["factory-1"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;

    // T3 stalls: the check fails, so the new line is not held, and its send times out too.
    e.t3.fail_messages.store(true, Ordering::SeqCst);
    queue_ready(&mut e, 11, &["factory-2"], minute(1)).await;
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
    let (mut e, agent) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_ready(&mut e, 10, &["factory-1"], at()).await;
    e.t3.hang_send.lock().unwrap().push("coord".to_owned());
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
async fn a_lost_message_to_a_gone_thread_passes_its_answer_on() {
    let (mut e, coordinator) = coordinator_env(AgentState::Idle, None);
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

    // The thread is gone by the check: the answer goes to the coordinator.
    e.t3.threads.lock().unwrap().remove("worker");
    e.t3.refuse_send.lock().unwrap().push("worker".to_owned());
    e.daemon.flush(minute(1)).await;
    e.daemon.flush(minute(1)).await;
    assert_eq!(unsure_wakes(&e, worker), None);
    let to_coordinator: Vec<_> =
        e.t3.sends()
            .into_iter()
            .filter(|(thread, _)| thread == "coord")
            .collect();
    assert_eq!(to_coordinator.len(), 1, "{to_coordinator:?}");
    let _ = coordinator;
}

#[tokio::test]
async fn a_fresh_session_notice_in_a_lost_message_is_due_again() {
    use board::daemon::FRESH_SESSION_RUNS;
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    wake_at_runs(&mut e, 0, minute(0), 10).await;
    e.t3.set_runs("coord", FRESH_SESSION_RUNS);
    e.daemon.reconcile(minute(6)).await.unwrap();
    queue_ready(&mut e, 11, &["factory-11"], minute(6)).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(minute(6)).await;
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    assert!(says_fresh(&e.t3.sends().last().unwrap().1));
    let since = |e: &Env| e.daemon.state.coordinators[&factory()].fresh.since;
    assert_eq!(since(&e), Some(FRESH_SESSION_RUNS));

    // Never got it: its line goes again, and the notice is due for the next wake-up.
    e.daemon.flush(minute(7)).await;
    let message = e.t3.sends().last().unwrap().1.clone();
    assert!(message.contains("factory-11"), "{message}");
    assert_eq!(since(&e), Some(0));
    queue_ready(&mut e, 12, &["factory-12"], minute(8)).await;
    e.daemon.flush(minute(8)).await;
    let message = e.t3.sends().last().unwrap().1.clone();
    assert!(message.contains("factory-12"), "{message}");
    assert!(says_fresh(&message), "{message}");
}

#[tokio::test]
async fn a_re_brief_sent_with_a_resume_that_timed_out_is_not_repeated_with_each_retry() {
    let reset = at() + chrono::Duration::hours(1);
    let (mut e, agent) = coordinator_env(AgentState::Ended, Some(reset));
    let slug = factory();
    e.daemon.state.coordinators.get_mut(&slug).unwrap().briefed = None;
    e.stub
        .on("GET /v1/boards", [counted(&agent, 0, Some(0), 0)]);
    let later = reset + chrono::Duration::minutes(1);
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.reconcile(later).await.unwrap();
    e.t3.time_out_send.store(false, Ordering::SeqCst);
    assert_eq!(e.t3.sends().len(), 1);
    assert!(unsure(&e).is_some());
    // The resume is tried again, without the re-brief, which is looked for instead.
    e.daemon.reconcile(later).await.unwrap();
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(
        sends[1].1,
        "board factory: the usage limit has reset. Run board_snapshot and continue your loop."
    );
    e.daemon
        .reconcile(later + chrono::Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(e.t3.message_reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        e.daemon.state.coordinators[&slug].briefed, None,
        "owed again"
    );
}

/// Reconciles at `now` with the coordinator's thread at `runs`, then queues `task` as ready
/// and flushes; returns the wake-up message sent.
async fn wake_at_runs(e: &mut Env, runs: u64, now: Timestamp, seq: i64) -> String {
    e.t3.set_runs("coord", runs);
    e.daemon.reconcile(now).await.unwrap();
    let task = format!("factory-{seq}");
    queue_ready(e, seq, &[&task], now).await;
    e.daemon.flush(now).await;
    let sends = e.t3.sends();
    let (thread, message) = sends.last().unwrap().clone();
    assert_eq!(thread, "coord");
    assert!(message.contains(&task), "{message}");
    message
}

fn says_fresh(message: &str) -> bool {
    message.ends_with(&board::daemon::fresh_session_line(&factory()))
}

#[tokio::test]
async fn a_wake_up_says_a_fresh_session_is_due_after_ten_runs_and_again_ten_later() {
    use board::daemon::FRESH_SESSION_RUNS;
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    let reads = |e: &Env| e.t3.message_reads.load(Ordering::SeqCst);
    // The first count read starts it; each reconcile is past the thread check's spare.
    let base = 500;
    assert!(!says_fresh(
        &wake_at_runs(&mut e, base, minute(0), 10).await
    ));
    let almost = base + FRESH_SESSION_RUNS - 1;
    assert!(!says_fresh(
        &wake_at_runs(&mut e, almost, minute(6), 11).await
    ));
    assert_eq!(reads(&e), 0, "messages read only once the count is reached");

    // Board, child and pull request messages are not the user's: they hold nothing.
    for text in [
        "board factory: factory-3 is ready (p1)",
        "Delegated task finished: factory-3",
        "Update on pull request #12",
    ] {
        e.t3.message("coord", text);
    }
    let message = wake_at_runs(&mut e, base + FRESH_SESSION_RUNS, minute(12), 12).await;
    assert!(says_fresh(&message), "{message}");
    assert_eq!(reads(&e), 1);
    assert_eq!(
        message,
        format!(
            "board factory: factory-12 is ready (p1)\nRun board_snapshot and continue.\n{}",
            board::daemon::fresh_session_line(&factory())
        )
    );

    // Said once: not again before ten more runs.
    let next = base + FRESH_SESSION_RUNS;
    for (i, runs) in [next + 1, next + FRESH_SESSION_RUNS - 1]
        .into_iter()
        .enumerate()
    {
        let i = i as i64;
        let message = wake_at_runs(&mut e, runs, minute(18 + 6 * i), 13 + i).await;
        assert!(!says_fresh(&message), "{runs}: {message}");
    }
    let message = wake_at_runs(&mut e, next + FRESH_SESSION_RUNS, minute(30), 15).await;
    assert!(says_fresh(&message), "{message}");
}

#[tokio::test]
async fn a_user_message_holds_the_fresh_session_for_an_hour() {
    use board::daemon::{FRESH_SESSION_RUNS, USER_QUIET};
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    let reads = |e: &Env| e.t3.message_reads.load(Ordering::SeqCst);
    let base = 40;
    wake_at_runs(&mut e, base, minute(0), 10).await;

    // The user writes to the coordinator: no fresh session while that is under an hour old.
    e.t3.message("coord", "Why did factory-3 stop?");
    let seen = minute(6);
    let reached = base + FRESH_SESSION_RUNS;
    assert!(!says_fresh(&wake_at_runs(&mut e, reached, seen, 11).await));
    assert_eq!(reads(&e), 1);
    // While it holds, the thread is not read again, however often it runs.
    let message = wake_at_runs(&mut e, reached + 3, minute(40), 12).await;
    assert!(!says_fresh(&message), "{message}");
    assert_eq!(reads(&e), 1);

    // An hour after it was first seen, a read finds nothing newer: due.
    let after = seen + USER_QUIET;
    let message = wake_at_runs(&mut e, reached + 4, after, 13).await;
    assert!(says_fresh(&message), "{message}");
    assert_eq!(reads(&e), 2);

    // Ten runs on, a newer user message holds it again, from when the daemon saw it.
    e.t3.message("coord", "Also check factory-5, please.");
    let again = reached + 4 + FRESH_SESSION_RUNS;
    let later = after + chrono::Duration::minutes(6);
    assert!(!says_fresh(&wake_at_runs(&mut e, again, later, 14).await));
    let message = wake_at_runs(&mut e, again + 1, later + USER_QUIET, 15).await;
    assert!(says_fresh(&message), "{message}");
}

#[tokio::test]
async fn a_fresh_session_notice_whose_send_failed_goes_with_the_retry() {
    use board::daemon::{FRESH_SESSION_RUNS, WAKE_RETRY_FIRST};
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    wake_at_runs(&mut e, 0, minute(0), 10).await;
    e.t3.set_runs("coord", FRESH_SESSION_RUNS);
    e.daemon.reconcile(minute(6)).await.unwrap();
    queue_ready(&mut e, 11, &["factory-11"], minute(6)).await;
    e.t3.fail_send.store(true, Ordering::SeqCst);
    e.daemon.flush(minute(6)).await;
    assert!(says_fresh(&e.t3.sends().last().unwrap().1));

    e.t3.fail_send.store(false, Ordering::SeqCst);
    e.daemon.flush(minute(6) + WAKE_RETRY_FIRST).await;
    let message = e.t3.sends().last().unwrap().1.clone();
    assert!(message.contains("factory-11"), "{message}");
    assert!(says_fresh(&message), "{message}");
    assert_eq!(
        e.daemon.state.coordinators[&factory()].fresh.since,
        Some(FRESH_SESSION_RUNS)
    );
}

#[test]
fn only_messages_not_from_the_board_children_or_pull_requests_are_the_users() {
    use board::daemon::is_user_message;
    for text in [
        "board factory: factory-3 is ready (p1)",
        "board agent-board: your standing instructions changed",
        &board::daemon::fresh_session_message(&factory()),
        "Delegated task finished",
        "Delegated tasks t1, t2 reached terminal states.",
        "Update on pull request #4",
        "/compact",
        "Background task completed.",
        &board::daemon::coordinator_message(&factory(), false),
        &board::daemon::adoption_message(&factory(), true),
    ] {
        assert!(!is_user_message(text), "{text}");
    }
    for text in [
        "Please look at the deploy",
        "board the train: it leaves at 6",
        "the board factory: hi",
    ] {
        assert!(is_user_message(text), "{text}");
    }
}

#[test]
fn the_coordinator_prompt_carries_the_fresh_session_message() {
    let message = board::daemon::fresh_session_message(&factory()).replace("factory", "<slug>");
    assert!(
        board::daemon::COORDINATOR_PROMPT.contains(&message),
        "{message}"
    );
}

#[test]
fn every_brief_starts_with_what_board_brief_prints() {
    for triage in [false, true] {
        let brief = board::daemon::brief(&factory(), triage);
        assert!(brief.contains(board::daemon::COORDINATOR_PROMPT));
        assert_eq!(brief.contains(board::daemon::TRIAGE_PROMPT), triage);
        assert!(board::daemon::coordinator_message(&factory(), triage).starts_with(&brief));
        assert!(
            board::daemon::rebrief_message(&factory(), triage).contains(&format!("\n\n{brief} "))
        );
    }
}

#[tokio::test]
async fn a_user_message_since_the_last_thread_check_holds_the_notice_it_would_go_with() {
    use board::daemon::FRESH_SESSION_RUNS;
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    wake_at_runs(&mut e, 0, minute(0), 10).await;
    e.t3.set_runs("coord", FRESH_SESSION_RUNS);
    e.daemon.reconcile(minute(6)).await.unwrap();
    // The user writes after the reconcile read the count: the flush reads it first.
    e.t3.message("coord", "Hold on, I am looking at factory-2.");
    queue_ready(&mut e, 11, &["factory-11"], minute(7)).await;
    e.daemon.flush(minute(7)).await;
    let message = e.t3.sends().last().unwrap().1.clone();
    assert!(message.contains("factory-11"), "{message}");
    assert!(!says_fresh(&message), "{message}");
}

#[tokio::test]
async fn a_failed_read_sends_the_wake_up_without_the_notice_and_tries_with_the_next() {
    use board::daemon::FRESH_SESSION_RUNS;
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    wake_at_runs(&mut e, 0, minute(0), 10).await;
    e.t3.fail_messages.store(true, Ordering::SeqCst);
    let message = wake_at_runs(&mut e, FRESH_SESSION_RUNS, minute(6), 11).await;
    assert!(!says_fresh(&message), "{message}");
    e.t3.fail_messages.store(false, Ordering::SeqCst);
    let message = wake_at_runs(&mut e, FRESH_SESSION_RUNS + 1, minute(12), 12).await;
    assert!(says_fresh(&message), "{message}");
}

#[test]
fn a_message_key_names_the_run_and_text_without_keeping_the_text() {
    use board::daemon::message_key;
    let message = |run: Option<&str>, text: &str| UserMessage {
        text: text.to_owned(),
        run: run.map(str::to_owned),
    };
    let first = message_key(&message(Some("run:7"), "secret token abc"));
    assert!(!first.contains("secret"));
    assert_eq!(
        first,
        message_key(&message(Some("run:7"), "secret token abc"))
    );
    // A message steered into the same run is a new one.
    assert_ne!(
        first,
        message_key(&message(Some("run:7"), "and another thing"))
    );
    assert_ne!(first, message_key(&message(None, "secret token abc")));
}

/// A reading the server would return: `week` percent of the weekly window, read at `read`.
fn usage_reading(week: f64, read: Timestamp) -> serde_json::Value {
    json!({
        "seven_day": { "percent": week, "resets_at": read + chrono::Duration::days(2) },
        "read_at": read,
        "device": DEVICE,
    })
}

#[tokio::test]
async fn high_usage_drops_new_work_wakes_and_wakes_the_coordinator_when_it_eases() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.stub.on("GET /v1/usage", usage_reading(95.0, at()));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.daemon.state.usage_held.contains(&factory()));

    // Over the board's stop: a ready task wakes no one.
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "t3:worker"),
            at(),
        )
        .await
        .unwrap();
    e.daemon.flush(at()).await;
    assert!(e.t3.sends().is_empty(), "{:?}", e.t3.sends());
    assert!(e.daemon.state.wakes.is_empty(), "dropped, not kept");

    // Below the stop: the coordinator is told once to start work again.
    let later = at() + chrono::Duration::minutes(1);
    e.stub.on("GET /v1/usage", usage_reading(40.0, later));
    e.daemon.reconcile(later).await.unwrap();
    assert!(e.daemon.state.usage_held.is_empty());
    e.daemon.flush(later).await;
    let sends = e.t3.sends();
    let [(thread, text)] = sends.as_slice() else {
        panic!("{sends:?}");
    };
    assert_eq!(thread, "coord");
    assert!(
        text.starts_with(
            "board factory: the account usage no longer stops new work on this board (stop at \
             90%)"
        ),
        "{text}"
    );
    e.daemon.reconcile(later).await.unwrap();
    e.daemon.flush(later).await;
    assert_eq!(e.t3.sends().len(), 1, "once");
}

#[tokio::test]
async fn a_stale_usage_reading_holds_nothing() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.stub.on("GET /v1/usage", usage_reading(95.0, at()));
    let stale = at() + chrono::Duration::minutes(board_proto::USAGE_FRESH_MINUTES + 1);
    e.daemon.reconcile(stale).await.unwrap();
    assert!(e.daemon.state.usage_held.is_empty());
    e.stub
        .on("GET /v1/tasks/factory-4", detail("factory-4", true, &[]));
    e.daemon
        .on_event(
            &event(10, EventType::TaskCreated, Some("factory-4"), "t3:worker"),
            stale,
        )
        .await
        .unwrap();
    e.daemon.flush(stale).await;
    assert_eq!(threads(&e.t3), ["coord"]);
}

#[tokio::test]
async fn the_daemon_reads_the_usage_endpoint_at_most_every_five_minutes_and_reports_it() {
    use board::daemon::USAGE_POLL_EVERY;
    use board::usage::{Credentials, UsageSource};

    let (mut e, _) = coordinator_env(AgentState::Idle, None);
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
    assert!(e.daemon.state.usage_held.contains(&factory()));

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

    let (mut e, _) = coordinator_env(AgentState::Idle, None);
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
    };
    e.stub
        .on("GET /v1/usage", usage_reading(95.0, at()))
        .on("PUT /v1/usage", usage_reading(95.0, at()));

    // An answer in an unknown shape is no reading: nothing is reported, and the server's
    // reading still holds the board.
    e.stub.on("GET /api/oauth/usage", json!({ "renamed": {} }));
    e.daemon.reconcile(at()).await.unwrap();
    assert!(e.stub.requests_to("PUT /v1/usage").is_empty());
    assert!(e.daemon.state.usage_held.contains(&factory()));

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
    assert!(e.daemon.state.usage_held.contains(&factory()));
}

#[tokio::test]
async fn a_daemon_without_a_reading_keeps_the_boards_it_held() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    // As after a restart: the saved state holds the board, and no reading is at hand.
    e.daemon.state.usage_held.insert(factory());
    e.stub
        .on_error("GET /v1/usage", 503, "unavailable", "down", "retry");
    e.daemon.reconcile(at()).await.unwrap();
    e.daemon.flush(at()).await;
    assert!(e.daemon.state.usage_held.contains(&factory()));
    assert!(
        e.t3.sends().is_empty(),
        "no false eased wake: {:?}",
        e.t3.sends()
    );
}
