//! The device daemon's hook relay: hooks reach the server through `$BOARD_HOME/run/hook.sock`
//! with the caller's agent and the daemon's token, and call the server directly when the
//! relay is gone, of another build, or on an earlier login.

mod common;

use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use board::config::Home;
use board::relay;
use board_proto::{AgentState, SnapshotKind, TouchResult};
use common::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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
    fn home(&self) -> Home {
        Home::at(self.home.clone())
    }

    /// Starts a relay to the stub, waits for its warm-up requests, then forgets them.
    async fn relay(&self) -> relay::Relay {
        let relay = relay::start(&self.home(), &self.stub.url, DAEMON_TOKEN.to_owned()).unwrap();
        for _ in 0..200 {
            if self.stub.requests_to("GET /healthz").len() >= 2 {
                self.stub.clear();
                return relay;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the relay never warmed its connections");
    }

    /// Runs a hook off the runtime's workers; `token` is the hook's own `BOARD_TOKEN`
    /// (empty: none).
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

    /// Bearer tokens the stub received, in order.
    fn tokens(&self) -> Vec<String> {
        self.stub
            .requests()
            .into_iter()
            .map(|r| r.auth.unwrap_or_default())
            .collect()
    }

    /// A private `run/` directory, as a daemon leaves it.
    fn run_dir(&self) -> PathBuf {
        let dir = self.home.join("run");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

fn http(socket: &Path) -> reqwest::Client {
    reqwest::Client::builder()
        .unix_socket(socket)
        .build()
        .unwrap()
}

fn url(path: &str) -> String {
    format!("http://relay.invalid{path}")
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
    let meta = std::fs::symlink_metadata(relay.path()).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(mode(relay.path()), 0o600);
    assert_eq!(relay::safe_socket(&e.home()).as_deref(), Some(relay.path()));
    // The relay answers with this build and its server; a hook of another server passes.
    assert!(relay::connect(relay.path(), &e.stub.url).await.is_some());
    assert!(
        relay::connect(relay.path(), "https://other.invalid")
            .await
            .is_none()
    );

    // A directory others may enter is not trusted, nor one in a home others may write.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
    assert_eq!(relay::safe_socket(&e.home()), None);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&e.home, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(relay::safe_socket(&e.home()), None);
    std::fs::set_permissions(&e.home, std::fs::Permissions::from_mode(0o755)).unwrap();

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
    // The relay keeps two connections warm, enough for a heartbeat and a snapshot at once.
    assert!(
        e.stub.connections() - before <= 1,
        "{} new connections",
        e.stub.connections() - before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_with_its_own_token_calls_the_server_directly() {
    let e = env();
    let _relay = e.relay().await;
    let text = e.hook("user-prompt-submit", e.prompt(), TOKEN).await;
    assert!(text.contains("alerts: x"), "{text}");
    assert_eq!(e.tokens(), vec![format!("Bearer {TOKEN}"); 2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_forwards_only_hook_requests_for_the_named_agent() {
    let e = env();
    let relay = e.relay().await;
    let http = http(relay.path());

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
        assert_eq!(resp.headers()[relay::RELAY_HEADER], "refused");
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
async fn the_relay_follows_a_new_login() {
    let e = env();
    let relay = e.relay().await;
    let http = http(relay.path());
    // `board login` rewrites the config and stores a new token (here in the token file).
    let config = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    std::fs::write(e.home.join("config.toml"), format!("{config}\n")).unwrap();
    std::fs::write(e.home.join("token"), "rotated-token\n").unwrap();
    let resp = http
        .get(url("/v1/snapshot?kind=alerts"))
        .header("x-board-agent", AGENT)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert_eq!(e.tokens(), vec!["Bearer rotated-token".to_owned()]);

    // A login to another server: the relay reports it, and the hook passes it by.
    let other = config.replace(&e.stub.url, "https://other.invalid");
    std::fs::write(e.home.join("config.toml"), other).unwrap();
    let status: relay::Status = http
        .get(url("/relay"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status.server, "https://other.invalid");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_call_the_server_directly_when_the_relay_is_gone() {
    let e = env();
    // A socket left by a daemon that died: safe to use, but it refuses connections.
    let path = e.run_dir().join("hook.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(relay::safe_socket(&e.home()).is_some());
    assert!(relay::connect(&path, &e.stub.url).await.is_none());

    let text = e.hook("user-prompt-submit", e.prompt(), TOKEN).await;
    assert!(text.contains("alerts: x"), "{text}");
    assert_eq!(
        e.tokens(),
        vec![format!("Bearer {TOKEN}"); 2],
        "called directly with the hook's own token"
    );
    assert_eq!(e.breaker(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_reports_an_unreachable_or_slow_server() {
    let e = env();
    // A slow server: the relay gives up first and says so.
    let relay = e.relay().await;
    e.stub.hang(Duration::from_secs(4));
    let resp = http(relay.path())
        .get(url("/v1/snapshot?kind=alerts"))
        .header("x-board-agent", AGENT)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 504);
    assert_eq!(resp.headers()[relay::RELAY_HEADER], "slow");
    drop(relay);

    // A server that refuses connections: the hook judges it unreachable, as directly.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let config = std::fs::read_to_string(e.home.join("config.toml")).unwrap();
    std::fs::write(
        e.home.join("config.toml"),
        config.replace(&e.stub.url, &dead),
    )
    .unwrap();
    let _relay = relay::start(&e.home(), &dead, DAEMON_TOKEN.to_owned()).unwrap();
    let text = e.hook("user-prompt-submit", e.prompt(), "").await;
    assert_eq!(text, "", "no cached snapshot to show");
    let breaker = e.breaker().expect("the breaker opened");
    assert!(breaker["until"].is_string(), "{breaker}");
}

/// Stands in for a relay that answers its status and then dies: it drops every other
/// request unanswered.
async fn dying_relay(listener: tokio::net::UnixListener, server: String) {
    let status = serde_json::to_string(&json!({
        "version": board_proto::VERSION,
        "commit": board::COMMIT,
        "server": server,
    }))
    .unwrap();
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let status = status.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            loop {
                let mut chunk = [0u8; 4096];
                let Ok(n) = stream.read(&mut chunk).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                buf.drain(..end + 4);
                if !head.starts_with("GET /relay ") {
                    // Gone mid-hook.
                    return;
                }
                let answer = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{status}",
                    status.len()
                );
                if stream.write_all(answer.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_dying_mid_hook_trips_nothing() {
    let e = env();
    let path = e.run_dir().join("hook.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    tokio::spawn(dying_relay(listener, e.stub.url.clone()));
    assert!(relay::connect(&path, &e.stub.url).await.is_some());
    // A token the hook would call the server with directly, had it passed the relay by.
    std::fs::write(e.home.join("token"), format!("{TOKEN}\n")).unwrap();

    let text = e.hook("user-prompt-submit", e.prompt(), "").await;
    assert_eq!(text, "", "no cached snapshot to show");
    assert!(e.stub.requests().is_empty());
    assert_eq!(
        e.breaker(),
        None,
        "a failing relay says nothing about the server"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_too_busy_to_accept_costs_no_hook_its_answer() {
    let e = env();
    // The relay on a runtime of its own, which the test can keep from accepting.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let relay = {
        let _on = runtime.enter();
        relay::start(&e.home(), &e.stub.url, DAEMON_TOKEN.to_owned()).unwrap()
    };
    let (stall, stalled) = tokio::sync::oneshot::channel::<()>();
    let (resume, resumed) = std::sync::mpsc::channel::<()>();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let driver = std::thread::spawn(move || {
        runtime.block_on(async move {
            stalled.await.unwrap();
            // Busy: nothing on this runtime runs, so nothing is accepted.
            resumed.recv().unwrap();
            stopped.await.unwrap();
        });
    });
    assert!(relay::connect(relay.path(), &e.stub.url).await.is_some());
    for _ in 0..200 {
        if e.stub.requests_to("GET /healthz").len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A token the hook calls the server with once it passes the relay by.
    std::fs::write(e.home.join("token"), format!("{TOKEN}\n")).unwrap();
    stall.send(()).unwrap();
    e.stub.clear();

    let text = e.hook("user-prompt-submit", e.prompt(), "").await;
    assert!(text.contains("alerts: x"), "{text}");
    assert_eq!(
        e.tokens(),
        vec![format!("Bearer {TOKEN}"); 2],
        "called directly with the hook's own token"
    );
    assert_eq!(e.breaker(), None);

    // The relay then meets the probe's closed connection: counted, and still serving.
    resume.send(()).unwrap();
    let mut serving = false;
    for _ in 0..200 {
        // macOS has no credentials for a closed connection; Linux keeps them and serves it.
        if (!cfg!(target_os = "macos") || relay.early_closes() == 1)
            && relay::connect(relay.path(), &e.stub.url).await.is_some()
        {
            serving = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        serving,
        "{} early closes; the relay must count the probe's and serve again",
        relay.early_closes()
    );
    stop.send(()).unwrap();
    driver.join().unwrap();
}
