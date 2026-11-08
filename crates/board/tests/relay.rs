//! The device daemon's hook relay: hooks reach the server through `$BOARD_HOME/run/hook.sock`
//! with the caller's agent and the daemon's token, and fall back to calling the server
//! directly when the relay is gone.

mod common;

use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use board::config::Home;
use board::relay;
use board_proto::{AgentState, SnapshotKind, TouchResult};
use common::*;
use serde_json::{Value, json};

const AGENT: &str = "claude:00893aaf-19fa-41d2-8238-13269b9b3ca0";
const SESSION: &str = "00893aaf-19fa-41d2-8238-13269b9b3ca0";
/// The relay's token, unlike the one hooks would send directly ([`TOKEN`]).
const DAEMON_TOKEN: &str = "daemon-device-token-9876543210";

struct Env {
    _dir: tempfile::TempDir,
    stub: Stub,
    home: PathBuf,
    repo: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let home = home_for(dir.path(), &stub.url);
    let repo = git_repo(dir.path(), "git@github.com:syz51/agent-board.git");
    stub.on(
        &format!("POST /v1/agents/{}/state", enc(AGENT)),
        agent_fixture(AGENT, DEVICE, AgentState::Running),
    )
    .on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Delta, "<board>\nalerts: x\n</board>", 9),
    )
    .on("POST /v1/touches", TouchResult { overlaps: vec![] });
    Env {
        _dir: dir,
        stub,
        home,
        repo,
    }
}

impl Env {
    /// Starts a relay to the stub and waits for its warm-up request, then forgets it.
    async fn relay(&self) -> relay::Relay {
        let relay = relay::start(
            &Home::at(self.home.clone()),
            &self.stub.url,
            DAEMON_TOKEN.to_owned(),
        )
        .unwrap();
        for _ in 0..200 {
            if !self.stub.requests_to("GET /healthz").is_empty() {
                self.stub.clear();
                return relay;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the relay never warmed its connection");
    }

    /// Runs a hook off the runtime's workers; `token` is the hook's own (empty: none).
    async fn hook(&self, event: &'static str, payload: Value, token: &str) -> String {
        let (home, repo, token) = (self.home.clone(), self.repo.clone(), token.to_owned());
        tokio::task::spawn_blocking(move || {
            let out = board(
                &home,
                &repo,
                &[("BOARD_TOKEN", &token)],
                &["hook", event, "--runtime", "claude"],
                &payload.to_string(),
            );
            assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
            stdout(&out)
        })
        .await
        .unwrap()
    }

    fn prompt(&self) -> Value {
        json!({
            "session_id": SESSION,
            "cwd": self.repo,
            "hook_event_name": "UserPromptSubmit",
            "prompt": "continue",
        })
    }

    fn edit(&self) -> Value {
        json!({
            "session_id": SESSION,
            "cwd": self.repo,
            "hook_event_name": "PostToolUse",
            "tool_name": "Edit",
            "tool_input": {"file_path": self.repo.join("src/lib.rs")},
            "tool_use_id": "toolu_01",
        })
    }

    fn breaker(&self) -> Option<Value> {
        let text = std::fs::read(self.home.join("state/offline.json")).ok()?;
        serde_json::from_slice(&text).ok()
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_socket_is_private_and_replaced_and_removed_with_the_relay() {
    let e = env();
    // A directory left too open and a socket file left by a daemon that died.
    let dir = e.home.join("run");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    drop(std::os::unix::net::UnixListener::bind(dir.join("hook.sock")).unwrap());

    let relay = e.relay().await;
    assert_eq!(relay.path(), dir.join("hook.sock"));
    assert_eq!(mode(&dir), 0o700, "only its user may enter the directory");
    assert!(
        std::fs::symlink_metadata(relay.path())
            .unwrap()
            .file_type()
            .is_socket()
    );
    assert_eq!(mode(relay.path()), 0o600);
    assert!(relay::answers(relay.path()).await);
    assert_eq!(
        relay::safe_socket(&Home::at(e.home.clone())).as_deref(),
        Some(relay.path())
    );

    let path = relay.path().to_owned();
    drop(relay);
    assert!(!path.exists(), "a stopped relay removes its socket");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_use_the_relay_with_their_own_agent_and_the_daemons_token() {
    let e = env();
    let _relay = e.relay().await;
    let before = e.stub.connections();

    // No token of its own: the relay holds the device's.
    let text = e.hook("user-prompt-submit", e.prompt(), "").await;
    assert!(text.contains("alerts: x"), "{text}");
    // The agent's first PostToolUse: touches, then the heartbeat and the alerts at once.
    let text = e.hook("post-tool-use", e.edit(), "").await;
    assert!(text.contains("alerts: x"), "{text}");

    let requests = e.stub.requests();
    let routes: Vec<String> = requests
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    assert_eq!(routes.len(), 5, "{routes:?}");
    for r in &requests {
        assert_eq!(r.agent.as_deref(), Some(AGENT), "the caller's agent");
        assert_eq!(
            r.auth.as_deref(),
            Some(format!("Bearer {DAEMON_TOKEN}").as_str())
        );
    }
    assert_eq!(
        e.stub.requests_to("POST /v1/touches")[0].body["paths"],
        json!(["src/lib.rs"])
    );
    // The relay warmed one connection at start and keeps it; requests sent at once may
    // need one more.
    assert!(
        e.stub.connections() - before <= 1,
        "{} new connections",
        e.stub.connections() - before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_forwards_only_hook_requests_for_the_named_agent() {
    let e = env();
    let relay = e.relay().await;
    let http = reqwest::Client::builder()
        .unix_socket(relay.path())
        .build()
        .unwrap();
    let url = |path: &str| format!("http://relay.invalid{path}");

    // Another agent's state, a route hooks never call, and no agent at all: refused.
    let other = enc("claude:someone-else");
    let refused = [
        http.post(url(&format!("/v1/agents/{other}/state")))
            .header("x-board-agent", AGENT)
            .json(&json!({"state": "running"})),
        http.post(url("/v1/tasks/agent-board-1/claim"))
            .header("x-board-agent", AGENT)
            .json(&json!({"agent": AGENT})),
        http.get(url("/v1/snapshot?kind=alerts")),
    ];
    for req in refused {
        let resp = req.send().await.unwrap();
        assert!(resp.status().is_client_error(), "{}", resp.status());
    }
    assert!(e.stub.requests().is_empty(), "nothing reached the server");

    // What the caller sends besides its agent and body stays behind.
    let resp = http
        .get(url("/v1/snapshot?kind=alerts"))
        .header("x-board-agent", AGENT)
        .header("authorization", "Bearer someone-elses-token")
        .header("tailscale-user-login", "user@example.com")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let sent = &e.stub.requests()[0];
    assert_eq!(sent.agent.as_deref(), Some(AGENT));
    assert_eq!(
        sent.auth.as_deref(),
        Some(format!("Bearer {DAEMON_TOKEN}").as_str())
    );
    assert_eq!(sent.query_pairs()["kind"], "alerts");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_call_the_server_directly_when_the_relay_is_gone() {
    let e = env();
    // A socket left by a daemon that died: safe to use, but it refuses connections.
    let dir = e.home.join("run");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("hook.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(relay::safe_socket(&Home::at(e.home.clone())).is_some());
    assert!(!relay::answers(&path).await);
    e.stub.clear();

    let text = e.hook("user-prompt-submit", e.prompt(), TOKEN).await;
    assert!(text.contains("alerts: x"), "{text}");
    let requests = e.stub.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|r| r.auth.as_deref() == Some(format!("Bearer {TOKEN}").as_str())),
        "called directly with the hook's own token"
    );
    assert_eq!(e.breaker(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_reports_an_unreachable_server_and_a_dying_relay_trips_nothing() {
    let e = env();
    // A relay whose server refuses connections: the hook judges the server unreachable.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let relay = relay::start(&Home::at(e.home.clone()), &dead, DAEMON_TOKEN.to_owned()).unwrap();
    let text = e.hook("user-prompt-submit", e.prompt(), "").await;
    assert_eq!(text, "", "no cached snapshot to show");
    let breaker = e.breaker().expect("the breaker opened");
    assert!(breaker["until"].is_string(), "{breaker}");
    assert!(e.stub.requests().is_empty());
    drop(relay);
    std::fs::remove_file(e.home.join("state/offline.json")).unwrap();

    // A relay that accepts and then drops every connection, as a daemon stopping mid-hook:
    // the hook falls back to the cache and leaves the breaker alone.
    let path = e.home.join("run/hook.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        }
    });
    let text = e.hook("user-prompt-submit", e.prompt(), "").await;
    assert_eq!(text, "");
    assert_eq!(
        e.breaker(),
        None,
        "a failing relay says nothing about the server"
    );
}
