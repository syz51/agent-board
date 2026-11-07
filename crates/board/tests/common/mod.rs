//! A stub board server that records requests and answers from canned routes, plus
//! fixtures built from the wire types so their shape always matches `board-proto`.

#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use board_proto::{
    Agent, AgentRole, AgentState, Board, BoardStatus, Priority, Runtime, Snapshot, SnapshotKind,
    StatusCounts, Task, TaskStatus,
};
use chrono::{TimeZone as _, Utc};
use serde::Serialize;
use serde_json::{Value, json};

pub const TOKEN: &str = "test-device-token-0123456789";
pub const DEVICE: &str = "mac-test";

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: String,
    pub agent: Option<String>,
    pub auth: Option<String>,
    pub accept: Option<String>,
    pub body: Value,
}

impl Recorded {
    /// The query as decoded pairs.
    pub fn query_pairs(&self) -> BTreeMap<String, String> {
        self.query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (k.to_owned(), v.replace("%3A", ":").replace("%2C", ","))
            })
            .collect()
    }
}

#[derive(Clone)]
struct Canned {
    status: u16,
    body: Value,
    delay: Duration,
}

#[derive(Default)]
struct StubState {
    routes: BTreeMap<String, Canned>,
    /// One-off answers per route, used before `routes`.
    once: BTreeMap<String, VecDeque<Canned>>,
    requests: Vec<Recorded>,
    /// Delay applied to every request (an overloaded or hanging server).
    hang: Option<Duration>,
}

#[derive(Clone)]
pub struct Stub {
    pub url: String,
    state: Arc<Mutex<StubState>>,
}

impl Stub {
    /// Starts the stub on a free loopback port, on its own thread and runtime.
    pub fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(StubState::default()));
        let shared = state.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let app = axum::Router::new().fallback(handle).with_state(shared);
                axum::serve(listener, app).await.unwrap();
            });
        });
        Self { url, state }
    }

    /// Answers `"<METHOD> <path>"` (path as sent, percent-encoded) with `body`.
    pub fn on(&self, route: &str, body: impl Serialize) -> &Self {
        self.on_status(route, 200, body)
    }

    pub fn on_status(&self, route: &str, status: u16, body: impl Serialize) -> &Self {
        self.state.lock().unwrap().routes.insert(
            route.to_owned(),
            Canned {
                status,
                body: serde_json::to_value(body).unwrap(),
                delay: Duration::ZERO,
            },
        );
        self
    }

    /// Answers `route` with an `ApiError`.
    pub fn on_error(
        &self,
        route: &str,
        status: u16,
        code: &str,
        message: &str,
        next: &str,
    ) -> &Self {
        self.on_status(
            route,
            status,
            json!({"code": code, "message": message, "next": next}),
        )
    }

    /// Answers the next request to `route` with an `ApiError`, then falls back to the route's
    /// usual answer.
    pub fn once_error(&self, route: &str, status: u16, code: &str, message: &str) -> &Self {
        self.state
            .lock()
            .unwrap()
            .once
            .entry(route.to_owned())
            .or_default()
            .push_back(Canned {
                status,
                body: json!({"code": code, "message": message, "next": null}),
                delay: Duration::ZERO,
            });
        self
    }

    /// Delays every answer, as a hanging server would.
    pub fn hang(&self, delay: Duration) {
        self.state.lock().unwrap().hang = Some(delay);
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Requests matching `"<METHOD> <path>"`.
    pub fn requests_to(&self, route: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| format!("{} {}", r.method, r.path) == route)
            .collect()
    }

    pub fn clear(&self) {
        self.state.lock().unwrap().requests.clear();
    }
}

async fn handle(State(state): State<Arc<Mutex<StubState>>>, req: Request) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or_default().to_owned();
    let (agent, auth, accept) = {
        let header = |name: &str| {
            req.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        (
            header("x-board-agent"),
            header("authorization"),
            header("accept"),
        )
    };
    let bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let route = format!("{method} {path}");
    let (canned, hang) = {
        let mut st = state.lock().unwrap();
        st.requests.push(Recorded {
            method,
            path,
            query,
            agent,
            auth,
            accept,
            body,
        });
        let once = st.once.get_mut(&route).and_then(VecDeque::pop_front);
        (once.or_else(|| st.routes.get(&route).cloned()), st.hang)
    };
    if let Some(delay) = hang {
        tokio::time::sleep(delay).await;
    }
    let canned = canned.unwrap_or(Canned {
        status: 404,
        body: json!({"code": "not_found", "message": format!("no stub for {route}"), "next": null}),
        delay: Duration::ZERO,
    });
    tokio::time::sleep(canned.delay).await;
    let mut resp = Response::new(Body::from(canned.body.to_string()));
    *resp.status_mut() = StatusCode::from_u16(canned.status).unwrap();
    resp.headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    resp
}

/// Percent-encodes a path segment the way the client does.
pub fn enc(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A `$BOARD_HOME` logged in to `server` as [`DEVICE`].
pub fn home_for(dir: &Path, server: &str) -> PathBuf {
    let home = dir.join("board-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!("server = \"{server}\"\ndevice = \"{DEVICE}\"\n"),
    )
    .unwrap();
    home
}

/// Runs the `board` binary with a clean agent environment.
pub fn board(home: &Path, cwd: &Path, env: &[(&str, &str)], args: &[&str], stdin: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_board"));
    command
        .args(args)
        .current_dir(cwd)
        .env("BOARD_HOME", home)
        .env("BOARD_TOKEN", TOKEN)
        .env("BOARD_KEYCHAIN", "0")
        .env_remove("T3CODE_THREAD_ID")
        .env_remove("T3CODE_CLI_PATH")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("BOARD_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        command.env(k, v);
    }
    // The T3 identity applies only outside a nested session, which depends on the
    // processes above this test; detached, it depends on nothing else.
    #[cfg(unix)]
    if env.iter().any(|(k, _)| *k == "T3CODE_THREAD_ID") {
        return detached(&command, stdin);
    }
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// Runs `command` with no `claude` or `codex` above it, whatever runs the tests: a shell
/// starts it in the background and exits, and it starts only once init (or a subreaper)
/// has adopted it. Its streams go through files, and `stdin` is its input.
#[cfg(unix)]
pub fn detached(command: &Command, stdin: &str) -> Output {
    use std::os::unix::process::ExitStatusExt as _;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("in"), stdin).unwrap();
    let script = r#"outer=$$
(
    while kill -0 "$outer" 2>/dev/null; do sleep 0.01; done
    "$@" <"$DETACHED/in" >"$DETACHED/out" 2>"$DETACHED/err"
    echo $? >"$DETACHED/code.tmp" && mv "$DETACHED/code.tmp" "$DETACHED/code"
) </dev/null >/dev/null 2>&1 &"#;
    let mut sh = Command::new("/bin/sh");
    sh.arg("-c")
        .arg(script)
        .arg("sh")
        .arg(command.get_program())
        .args(command.get_args())
        .env("DETACHED", dir.path());
    if let Some(cwd) = command.get_current_dir() {
        sh.current_dir(cwd);
    }
    for (k, v) in command.get_envs() {
        match v {
            Some(v) => sh.env(k, v),
            None => sh.env_remove(k),
        };
    }
    // Waiting reaps the shell, so the loop above sees it gone.
    assert!(sh.status().unwrap().success());
    let code = dir.path().join("code");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !code.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the detached command did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let code: i32 = std::fs::read_to_string(&code)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: std::fs::read(dir.path().join("out")).unwrap(),
        stderr: std::fs::read(dir.path().join("err")).unwrap(),
    }
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A git repo with `origin` set, for repo keys and relative paths.
pub fn git_repo(dir: &Path, origin: &str) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "feature/x"]);
    git(&["remote", "add", "origin", origin]);
    // Resolve symlinks (macOS /var → /private/var) so hook cwd and git agree.
    repo.canonicalize().unwrap()
}

pub fn at() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 6, 12, 0, 0).unwrap()
}

pub fn board_fixture(slug: &str, repos: &[&str]) -> Board {
    Board {
        slug: slug.parse().unwrap(),
        title: format!("{slug} board"),
        goal: String::new(),
        status: BoardStatus::Active,
        temporary: false,
        repos: repos.iter().map(|r| (*r).to_owned()).collect(),
        autopilot: false,
        coordinator: None,
        home_devices: Vec::new(),
        counts: StatusCounts::default(),
        open_asks: 0,
        created_at: at(),
        updated_at: at(),
    }
}

pub fn task_fixture(id: &str, title: &str) -> Task {
    let id: board_proto::TaskId = id.parse().unwrap();
    Task {
        board: id.board(),
        id,
        parent: None,
        title: title.to_owned(),
        body: String::new(),
        acceptance: String::new(),
        status: TaskStatus::Open,
        priority: Priority::P1,
        blocked_reason: None,
        owner: None,
        owner_stale: false,
        blockers: Vec::new(),
        ready: true,
        not_before: None,
        auto: true,
        created_by: "t3:creator".parse().unwrap(),
        version: 3,
        feedback: None,
        created_at: at(),
        updated_at: at(),
    }
}

pub fn agent_fixture(id: &str, device: &str, state: AgentState) -> Agent {
    Agent {
        id: id.parse().unwrap(),
        handle: id.chars().take(7).collect(),
        runtime: Runtime::Claude,
        device: device.parse().unwrap(),
        role: AgentRole::Worker,
        title: None,
        repo: None,
        branch: None,
        cwd: None,
        native_ids: BTreeMap::new(),
        forked_from: None,
        state,
        paused_until: None,
        boards: Vec::new(),
        last_seen: at(),
        created_at: at(),
    }
}

pub fn snapshot_fixture(kind: SnapshotKind, text: &str, seq: i64) -> Snapshot {
    Snapshot {
        kind,
        text: text.to_owned(),
        seq,
        at: Utc::now(),
    }
}
