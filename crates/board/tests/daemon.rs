//! The device daemon's start and wake decisions, with a fake `t3`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use board::client::Client;
use board::config::{Config, Home};
use board::daemon::{Adoption, Coordinator, Daemon, load_adoption, save_adoption};
use board::identity::RepoInfo;
use board::ops::Ctx;
use board::t3::{ListedThread, T3, ThreadInfo, TimedOut};
use board_proto::{
    AgentState, Ask, AskKind, AskStatus, Board, Event, EventType, Runtime, TaskDetail, Timestamp,
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

#[derive(Clone, Default)]
struct FakeT3 {
    calls: Arc<Mutex<Vec<Call>>>,
    threads: Arc<Mutex<BTreeMap<String, ThreadInfo>>>,
    /// `t3 thread start` fails while set.
    fail_start: Arc<AtomicBool>,
    /// While set, `t3 thread start` creates [`ORPHAN`] and then fails, as when the daemon is
    /// killed or `t3` times out mid-start; the value says whether its message got through.
    die_after_create: Arc<Mutex<Option<bool>>>,
    /// `t3 thread list` and `t3 thread send` fail while set.
    fail_list: Arc<AtomicBool>,
    fail_send: Arc<AtomicBool>,
    /// `t3 thread send` runs out of time while set, after it may have delivered.
    time_out_send: Arc<AtomicBool>,
    /// Threads whose `t3 thread show` never answers.
    hang_info: Arc<Mutex<Vec<String>>>,
    /// `t3 thread show` fails while set.
    fail_info: Arc<AtomicBool>,
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
}

impl FakeT3 {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn sends(&self) -> Vec<(String, String)> {
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

    fn live(&self, thread: &str) {
        self.threads.lock().unwrap().insert(
            thread.to_owned(),
            ThreadInfo {
                archived: false,
                runtime: Runtime::Claude,
            },
        );
    }
}

impl T3 for FakeT3 {
    async fn start_thread(&self, project: &Path, title: &str, message: &str) -> Result<String> {
        self.calls.lock().unwrap().push(Call::Start {
            project: project.to_path_buf(),
            title: title.to_owned(),
            message: message.to_owned(),
        });
        if self.fail_start.load(Ordering::SeqCst) {
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
        if self.time_out_send.load(Ordering::SeqCst) {
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
        if self.hang_info.lock().unwrap().iter().any(|t| t == thread) {
            std::future::pending::<()>().await;
        }
        if self.fail_info.load(Ordering::SeqCst) {
            anyhow::bail!("t3 thread show failed: server not running");
        }
        Ok(self.threads.lock().unwrap().get(thread).cloned())
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

fn env() -> Env {
    env_with_projects([(REPO.to_owned(), PathBuf::from("/src/agent-board"))].into())
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
        },
    ] = calls.as_slice()
    else {
        panic!("{calls:?}");
    };
    assert_eq!(project, Path::new("/src/agent-board"));
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
        },
    );
    e.stub.on(
        "DELETE /v1/boards/factory/coordinator",
        autopilot(None, &[DEVICE]),
    );
    e.stub.clear();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(12))
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
        let logs = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
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
    let gone = "waking t3:coord: it ended and T3 no longer has its thread, or archived it";
    assert_eq!(
        text.matches(&format!("{gone}; 1 wake-ups dropped")).count(),
        1,
        "{text}"
    );
    assert_eq!(
        text.matches(&format!(
            "{gone}; 1 answers to its asks dropped, with no other coordinator here to pass them to"
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

#[tokio::test]
async fn a_send_that_timed_out_is_not_sent_again() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    queue_ready(&mut e, 10, &["factory-1"], at()).await;
    e.t3.time_out_send.store(true, Ordering::SeqCst);
    e.daemon.flush(at()).await;
    e.daemon.flush(at() + chrono::Duration::minutes(30)).await;
    assert_eq!(e.t3.sends().len(), 1, "T3 may have delivered it");
    assert!(e.daemon.state.wakes.is_empty());
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
            .all(|r| r.method == "GET" && r.path == "/v1/boards"),
        "only board reads: {:?}",
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
    [(REPO.to_owned(), PathBuf::from("/src/agent-board"))].into()
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
            project: PathBuf::from("/src/agent-board"),
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
    let e = env();
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
    let prompt = board::daemon::coordinator_message(&factory(), false);
    for rule in [
        "A climb is a task whose acceptance is a score command",
        "note starting `score:`",
        "rounds in a row gain no more than the score's noise",
        "merges only on the user's decision",
    ] {
        assert!(prompt.contains(rule), "missing {rule:?}");
    }
    // Generic: no tool paths of one setup.
    assert!(!prompt.contains("factory/cli.js"));
}

#[test]
fn the_coordinator_prompt_reports_to_the_user_apart_from_chat() {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let prompt = words(&board::daemon::coordinator_message(&factory(), false));
    for rule in [
        "with evidence (each check with its exit code, the commit, the PR) and a `summary` for the user",
        "finish with `task_done`, evidence and a `summary`",
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

/// Reconciles once with `config` on a home device of the autopilot `board`, whose
/// coordinator lease is free, and returns the fake `t3` and the stub.
async fn start_with(mut board: Board, config: Config) -> (FakeT3, Stub) {
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
    Daemon::new(ctx, t3.clone()).reconcile(at()).await.unwrap();
    (t3, stub)
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
        board_projects: [("board-feedback".parse().unwrap(), "/src/feedback".into())].into(),
        ..Config::default()
    };
    let (t3, stub) = start_with(board_fixture("board-feedback", &[]), config).await;
    let (project, _) = started(&t3);
    assert_eq!(project, Path::new("/src/feedback"));
    let register = &stub.requests_to("POST /v1/agents")[0].body;
    assert_eq!(register["role"], "coordinator");
    assert_eq!(register["repo"], serde_json::Value::Null);
    assert_eq!(register["cwd"], "/src/feedback");

    // A board repo under [projects] wins over [board_projects].
    let config = Config {
        projects: projects(),
        board_projects: [(factory(), "/src/elsewhere".into())].into(),
        ..Config::default()
    };
    let (t3, stub) = start_with(board_fixture("factory", &[REPO]), config).await;
    assert_eq!(started(&t3).0, Path::new("/src/agent-board"));
    assert_eq!(stub.requests_to("POST /v1/agents")[0].body["repo"], REPO);
}

#[tokio::test]
async fn only_the_feedback_boards_coordinator_is_briefed_to_triage() {
    let triage = board::daemon::TRIAGE_PROMPT;
    assert!(triage.starts_with("# Triage"));

    // The default feedback board.
    let config = Config {
        board_projects: [("board-feedback".parse().unwrap(), "/src/feedback".into())].into(),
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
            ("board-feedback".parse().unwrap(), "/src/feedback".into()),
            ("fb".parse().unwrap(), "/src/fb".into()),
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
    board::daemon::lock_watch(&home).expect("free again once released");
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

#[tokio::test]
async fn a_hanging_thread_check_holds_up_no_other_board() {
    let mut e = env();
    e.daemon.timing.thread_check = Duration::from_millis(300);
    let mut boards = Vec::new();
    for (slug, thread) in [("factory", "coord"), ("other", "hung"), ("third", "slow")] {
        let agent = format!("t3:{thread}");
        e.t3.live(thread);
        e.daemon.state.coordinators.insert(
            slug.parse().unwrap(),
            Coordinator {
                thread: thread.into(),
                agent: agent.parse().unwrap(),
                started_at: at(),
                renewed_at: at(),
                resumed_after: None,
                registered: true,
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
    // T3 answers for no thread: one after another, the checks would take 900 ms.
    e.t3.hang_info
        .lock()
        .unwrap()
        .extend(["coord".into(), "hung".into(), "slow".into()]);

    let started = Instant::now();
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(11))
        .await
        .unwrap();
    let took = started.elapsed();
    assert!(
        took < Duration::from_millis(700),
        "the checks ran side by side, each cut at 300 ms: {took:?}"
    );
    for slug in ["factory", "other", "third"] {
        assert_eq!(
            e.stub
                .requests_to(&format!("POST /v1/boards/{slug}/coordinator"))
                .len(),
            1,
            "{slug}: renewed, a check that never answers being no sign the thread is gone"
        );
    }
    assert_eq!(e.daemon.state.coordinators.len(), 3, "none replaced");
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
    use std::os::unix::fs::PermissionsExt as _;

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
    std::fs::write(
        &t3,
        format!(
            "#!/bin/sh\ncase \"$1 $2\" in\n\
             'thread start') cat >/dev/null; echo '{{\"threadId\":\"{NEW_THREAD}\"}}' ;;\n\
             'thread show') echo '{{\"thread\":{{\"archived\":false}}}}' ;;\n\
             'thread list') echo '{{\"threads\":[]}}' ;;\n\
             *) cat >/dev/null ;;\nesac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&t3, std::fs::Permissions::from_mode(0o755)).unwrap();
    let home = home_for(dir.path(), &stub.url);
    let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "{config}t3_bin = {:?}\n[projects]\n{REPO:?} = \"/src/agent-board\"\n",
            t3.display().to_string()
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
