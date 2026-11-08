//! A stub board server that records requests and answers from canned routes, plus
//! fixtures built from the wire types so their shape always matches `board-proto`.

#![allow(dead_code)]

pub mod script;

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
use futures_util::StreamExt as _;
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
    /// Routes answered as a server-sent event stream: the events after the request's
    /// `since`, then nothing more while the connection stays open.
    streams: BTreeMap<String, Stream>,
    /// Version, commit and order stamp sent in the build headers, as a server does since it
    /// reports them; servers before the stamp send none.
    build: Option<(String, String, Option<String>)>,
}

struct Stream {
    events: Vec<board_proto::Event>,
    /// Each connection carries the first event after `since` and then ends.
    flapping: bool,
}

#[derive(Clone)]
pub struct Stub {
    pub url: String,
    state: Arc<Mutex<StubState>>,
    /// Connections accepted so far.
    connections: Arc<std::sync::atomic::AtomicUsize>,
}

impl Stub {
    /// Starts the stub on a free loopback port, on its own thread and runtime.
    pub fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(StubState::default()));
        let shared = state.clone();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = connections.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                use axum::serve::ListenerExt as _;
                let listener =
                    tokio::net::TcpListener::from_std(listener)
                        .unwrap()
                        .tap_io(move |_| {
                            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        });
                let app = axum::Router::new().fallback(handle).with_state(shared);
                axum::serve(listener, app).await.unwrap();
            });
        });
        Self {
            url,
            state,
            connections,
        }
    }

    /// How many connections clients opened to the stub so far.
    pub fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
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

    /// Names a server build in the headers of every answer.
    /// Its order stamp is `unknown`, as from a server built without one.
    pub fn build(&self, version: &str, commit: &str) {
        self.build_stamped(version, commit, Some("unknown"));
    }

    /// Names a server build with its order stamp in the headers of every answer; `None`
    /// sends no stamp header, as a server from before stamps.
    pub fn build_stamped(&self, version: &str, commit: &str, built: Option<&str>) {
        self.state.lock().unwrap().build = Some((
            version.to_owned(),
            commit.to_owned(),
            built.map(str::to_owned),
        ));
    }

    /// Delays the answers to `route` by `delay`, as a server stalled on it would.
    pub fn delay(&self, route: &str, delay: Duration) -> &Self {
        if let Some(canned) = self.state.lock().unwrap().routes.get_mut(route) {
            canned.delay = delay;
        }
        self
    }

    /// Answers `route` as an event stream carrying the `events` after the request's
    /// `since`, then staying open and silent: no event and no keep-alive.
    pub fn on_stream(&self, route: &str, events: Vec<board_proto::Event>) -> &Self {
        let stream = Stream {
            events,
            flapping: false,
        };
        self.state
            .lock()
            .unwrap()
            .streams
            .insert(route.to_owned(), stream);
        self
    }

    /// Answers `route` as an event stream that carries the first of `events` after the
    /// request's `since` and then ends, as a flapping connection would.
    pub fn on_flapping_stream(&self, route: &str, events: Vec<board_proto::Event>) -> &Self {
        let stream = Stream {
            events,
            flapping: true,
        };
        self.state
            .lock()
            .unwrap()
            .streams
            .insert(route.to_owned(), stream);
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
    let since: i64 = query
        .split('&')
        .find_map(|p| p.strip_prefix("since="))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let (canned, hang, stream, build) = {
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
        let stream = st.streams.get(&route).map(|stream| {
            let after = stream.events.iter().filter(|e| e.seq > since);
            let take = if stream.flapping { 1 } else { usize::MAX };
            let text = after
                .take(take)
                .map(|e| {
                    format!(
                        "id: {}\ndata: {}\n\n",
                        e.seq,
                        serde_json::to_string(e).unwrap()
                    )
                })
                .collect::<String>();
            (text, stream.flapping)
        });
        (
            once.or_else(|| st.routes.get(&route).cloned()),
            st.hang,
            stream,
            st.build.clone(),
        )
    };
    if let Some((text, flapping)) = stream {
        let sent = futures_util::stream::iter([Ok::<_, std::io::Error>(text)]);
        let body = if flapping {
            Body::from_stream(sent)
        } else {
            Body::from_stream(sent.chain(futures_util::stream::pending()))
        };
        let mut resp = Response::new(body);
        resp.headers_mut().insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        return resp;
    }
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
    if let Some((version, commit, built)) = build {
        let headers = resp.headers_mut();
        headers.insert(
            board_proto::VERSION_HEADER,
            HeaderValue::from_str(&version).unwrap(),
        );
        headers.insert(
            board_proto::COMMIT_HEADER,
            HeaderValue::from_str(&commit).unwrap(),
        );
        if let Some(built) = built {
            headers.insert(
                board_proto::BUILT_HEADER,
                HeaderValue::from_str(&built).unwrap(),
            );
        }
    }
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
        parallel_tasks: 1,
        usage_stop_at: board_proto::USAGE_STOP_AT_DEFAULT,
        coordinator: None,
        dispatch: Default::default(),
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
        proposal: false,
        blockers: Vec::new(),
        ready: true,
        not_before: None,
        auto: true,
        created_by: "t3:creator".parse().unwrap(),
        version: 3,
        feedback: None,
        summary: None,
        child_count: 0,
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
        held_by: Vec::new(),
    }
}

/// A parent's detail with four children in three statuses, as a current server sends it.
pub fn parent_detail() -> board_proto::TaskDetail {
    let mut task = task_fixture("factory-1", "Umbrella");
    task.child_count = 4;
    let child = |id: &str, status: TaskStatus| board_proto::ChildTask {
        id: id.parse().unwrap(),
        title: format!("Child {id}"),
        status,
        ready: status == TaskStatus::Open,
        owner: None,
        owner_stale: false,
    };
    let child_tasks = vec![
        child("factory-2", TaskStatus::Done),
        child("factory-3", TaskStatus::Open),
        child("factory-4", TaskStatus::Done),
        child("factory-5", TaskStatus::Blocked),
    ];
    board_proto::TaskDetail {
        task,
        notes: vec![],
        evidence: vec![],
        blocks: vec![],
        children: child_tasks.iter().map(|c| c.id.clone()).collect(),
        child_tasks,
        history: vec![],
        handles: Default::default(),
    }
}

/// What `show` and `task_show` print for [`parent_detail`]'s children.
pub const PARENT_CHILDREN_LINES: &str = "children 4: done 2 · blocked 1 · open 1\n  done factory-2,factory-4\n  blocked factory-5\n  open factory-3";
