//! The device daemon's start and wake decisions, with a fake `t3`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use board::client::Client;
use board::config::{Config, Home};
use board::daemon::{Coordinator, Daemon};
use board::identity::RepoInfo;
use board::ops::Ctx;
use board::t3::{T3, ThreadInfo};
use board_proto::{
    AgentState, Ask, AskKind, AskStatus, Board, Event, EventType, Runtime, TaskDetail, Timestamp,
};
use common::*;
use serde_json::json;

const REPO: &str = "github.com/syz51/agent-board";
const NEW_THREAD: &str = "thread:project:p1:new";

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
}

#[derive(Clone, Default)]
struct FakeT3 {
    calls: Arc<Mutex<Vec<Call>>>,
    threads: Arc<Mutex<BTreeMap<String, ThreadInfo>>>,
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
                Call::Start { .. } => None,
            })
            .collect()
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
        self.live(NEW_THREAD);
        Ok(NEW_THREAD.to_owned())
    }

    async fn send(&self, thread: &str, message: &str) -> Result<()> {
        self.calls.lock().unwrap().push(Call::Send {
            thread: thread.to_owned(),
            message: message.to_owned(),
        });
        Ok(())
    }

    async fn thread_info(&self, thread: &str) -> Result<Option<ThreadInfo>> {
        Ok(self.threads.lock().unwrap().get(thread).cloned())
    }
}

struct Env {
    _dir: tempfile::TempDir,
    stub: Stub,
    t3: FakeT3,
    daemon: Daemon<FakeT3>,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let home = Home::at(dir.path().join("home"));
    let config = Config {
        server: Some(stub.url.clone()),
        device: Some(DEVICE.parse().unwrap()),
        projects: [(REPO.to_owned(), PathBuf::from("/src/agent-board"))].into(),
        ..Config::default()
    };
    let ctx = Ctx {
        home,
        device: DEVICE.parse().unwrap(),
        client: Client::new(&stub.url, TOKEN.into(), None, None).unwrap(),
        config,
        repo: RepoInfo::default(),
    };
    let t3 = FakeT3::default();
    let daemon = Daemon::new(ctx, t3.clone());
    Env {
        _dir: dir,
        stub,
        t3,
        daemon,
    }
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

    // Thread archived: start a fresh one.
    e.t3.threads.lock().unwrap().insert(
        NEW_THREAD.into(),
        ThreadInfo {
            archived: true,
            runtime: Runtime::Claude,
        },
    );
    e.daemon
        .reconcile(at() + chrono::Duration::minutes(12))
        .await
        .unwrap();
    assert_eq!(e.t3.calls().len(), 2);
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
    e.daemon.flush().await;
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
    e.daemon.flush().await;
    assert_eq!(e.t3.sends().len(), 1);
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
    e.daemon.flush().await;
    let sends = e.t3.sends();
    assert_eq!(sends.len(), 1);
    assert!(
        sends[0]
            .1
            .starts_with("board factory: factory-9 is done; its blocker cleared for factory-12\n"),
        "{sends:?}"
    );
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
    e.daemon.flush().await;
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
    e.daemon.flush().await;
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
    e.daemon.flush().await;
    assert!(e.t3.sends().is_empty());
}

#[tokio::test]
async fn an_answered_ask_wakes_the_asking_worker_and_the_coordinator() {
    let (mut e, _) = coordinator_env(AgentState::Idle, None);
    e.daemon.reconcile(at()).await.unwrap();
    let ask = |id: i64, agent: &str| Ask {
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
    };
    e.stub
        .on("GET /v1/asks", [ask(3, "t3:worker"), ask(4, "t3:remote")]);
    e.stub.on(
        &format!("GET {}", agent_path("t3:worker")),
        agent_fixture("t3:worker", DEVICE, AgentState::Idle),
    );
    e.stub.on(
        &format!("GET {}", agent_path("t3:remote")),
        agent_fixture("t3:remote", "mac-air", AgentState::Idle),
    );

    let mut answered = event(30, EventType::AskAnswered, None, "user");
    answered.payload = json!({"ask": 3});
    e.daemon.on_event(&answered, at()).await.unwrap();
    e.daemon.flush().await;
    let sends = e.t3.sends();
    let threads: Vec<&str> = sends.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(threads, ["coord", "worker"]);
    let message = &sends[1].1;
    assert!(
        message.starts_with("the user answered ask #3 about factory-7: \"sqlite "),
        "{message}"
    );
    assert!(
        !message.contains('<') && !message.contains("Which db"),
        "question text is not relayed: {message}"
    );
    let q = e.stub.requests_to("GET /v1/asks")[0].query_pairs();
    assert_eq!(
        (q["status"].as_str(), q["board"].as_str()),
        ("answered", "factory")
    );

    // A worker on another device is that device's daemon's to wake.
    let mut remote = event(31, EventType::AskAnswered, None, "user");
    remote.payload = json!({"ask": 4});
    e.daemon.on_event(&remote, at()).await.unwrap();
    e.daemon.flush().await;
    let threads: Vec<String> = e.t3.sends().into_iter().map(|(t, _)| t).collect();
    assert_eq!(threads, ["coord", "worker", "coord"]);
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
