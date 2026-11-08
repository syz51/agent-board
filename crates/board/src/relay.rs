//! The device daemon's relay for hooks: `board watch` serves the hooks of its `$BOARD_HOME`
//! on a Unix socket and forwards their requests to the server over kept-alive connections,
//! so a hook that calls the server pays for no TLS handshake and no token lookup.
//!
//! - The socket is `$BOARD_HOME/run/hook.sock`, in a directory only its user may enter
//!   (0700), with mode 0600. The relay serves only peers with its own user id.
//! - Each request names its agent in `x-board-agent`, which the relay forwards unchanged
//!   with the device token; it never adds an identity of its own. It forwards only the
//!   routes hooks call, and a route with an agent in its path only for that same agent, so a
//!   hook can do through the relay only what it could do with the token itself.
//! - A hook asks the relay first (`GET /relay`, answered by the relay itself) and uses it
//!   only when it answers at once with the hook's own build and server; otherwise the hook
//!   calls the server directly. The relay reloads the server and token when `config.toml`
//!   changes (`board login` rewrites it).
//! - When the relay gets no answer from the server, or refuses a request itself, it says so
//!   in [`RELAY_HEADER`], so the hook judges the server as it would have directly and never
//!   blames it for the relay.
//! - A hook whose probe gets no answer within [`PROBE`] closes its connection, often before
//!   the relay accepts it, and calls the server directly. The relay counts these and says
//!   how many in one line every [`EARLY_REPORT`], not one warning each.

use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use board_proto::{AGENT_HEADER, AgentId, BUILT_HEADER, COMMIT_HEADER, VERSION_HEADER};
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};

use crate::client::Client;
use crate::config::{Config, Home, load_token};

/// Set by the relay on an answer it made itself: `unreachable` (no connection to the
/// server), `slow` (no answer in time) or `refused` (not forwarded).
pub const RELAY_HEADER: &str = "x-board-relay";
/// The relay's own route, answered with its [`Status`].
pub const STATUS_PATH: &str = "/relay";
/// The relay's directory in `$BOARD_HOME`, and the socket in it.
const DIR: &str = "run";
const SOCKET: &str = "hook.sock";
/// Largest request body the relay forwards: a batch of touches is well under it.
const BODY_MAX: usize = 1 << 20;
/// Time for opening a connection to the server, TLS included; past it the server counts as
/// unreachable, as for a hook calling it directly.
const UPSTREAM_CONNECT: Duration = Duration::from_millis(500);
/// Bounds one forwarded request. Longer than any hook's budget, so the hook gives up first
/// and judges the server slow, but short enough that a connection that died silently (a
/// Mac waking from sleep) leaves the pool soon.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);
/// How long an idle connection to the server is kept for the next hook.
const UPSTREAM_IDLE: Duration = Duration::from_secs(300);
/// How often the relay asks the server for its health on two connections at once, so the
/// pool keeps the two a hook's heartbeat and snapshot use, and drops dead ones.
const KEEP_WARM: Duration = Duration::from_secs(30);
/// How long a hook waits for the relay's status; past it, the hook calls the server.
const PROBE: Duration = Duration::from_millis(100);
/// How often the relay says how many hooks closed their connection before it accepted it.
const EARLY_REPORT: Duration = Duration::from_secs(600);

/// The relay's socket for `home`.
pub fn socket_path(home: &Home) -> PathBuf {
    home.root().join(DIR).join(SOCKET)
}

/// This process's effective user id.
fn own_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// What the relay says about itself at [`STATUS_PATH`]: a hook uses it only when all three
/// match its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub commit: String,
    pub server: String,
}

impl Status {
    fn of(server: &str) -> Self {
        Self {
            version: board_proto::VERSION.to_owned(),
            commit: crate::COMMIT.to_owned(),
            server: server.to_owned(),
        }
    }
}

/// The routes hooks call. One that names an agent in its path is allowed only for the
/// agent the request names in [`AGENT_HEADER`], in the client's encoding of its id.
fn hook_route(method: &Method, path: &str, agent: &AgentId) -> bool {
    let Some(rest) = path.strip_prefix("/v1/") else {
        return false;
    };
    let own = crate::client::enc(agent.as_str());
    let segments: Vec<&str> = rest.split('/').collect();
    match segments.as_slice() {
        ["agents"] | ["touches"] => method == Method::POST,
        ["snapshot"] | ["boards"] | ["tasks"] => method == Method::GET,
        ["agents", id, "state"] => method == Method::POST && *id == own,
        ["agents", id, "handoff"] => method == Method::GET && *id == own,
        _ => false,
    }
}

/// The relay serving on its socket; dropping it stops serving and removes the socket.
#[derive(Debug)]
pub struct Relay {
    path: PathBuf,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    early: Arc<AtomicU64>,
}

impl Relay {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Connections closed before the relay accepted them, not yet reported.
    pub fn early_closes(&self) -> u64 {
        self.early.load(Ordering::Relaxed)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        // A socket left behind refuses connections, and hooks then call the server directly
        // until the next daemon replaces it.
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => tracing::warn!("removing {}: {err}", self.path.display()),
        }
    }
}

/// Starts serving hooks of `home`, forwarding to `server` with `token` (as the daemon
/// loaded them from the home) until `config.toml` changes. The caller must hold the home's
/// watch lock, so a socket already there is a stale one and is replaced.
pub fn start(home: &Home, server: &str, token: String) -> Result<Relay> {
    let path = socket_path(home);
    let uid = own_uid();
    let dir = path.parent().context("the relay socket has a directory")?;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err).with_context(|| format!("creating {}", dir.display())),
    }
    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    anyhow::ensure!(
        meta.is_dir() && meta.uid() == uid,
        "{} is not a directory owned by this user; remove it and restart board watch",
        dir.display()
    );
    if meta.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("making {} private", dir.display()))?;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("removing {}", path.display())),
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))?;
    let login = Login {
        stamp: config_stamp(home),
        base: server.trim_end_matches('/').to_owned(),
        token,
    };
    let upstream = Arc::new(Upstream::new(home.clone(), login)?);
    let warm = upstream.clone();
    let keep_warm = tokio::spawn(async move {
        let mut tick = tokio::time::interval(KEEP_WARM);
        loop {
            tick.tick().await;
            warm.warm().await;
        }
    });
    let app = axum::Router::new()
        .route(STATUS_PATH, axum::routing::get(status))
        .fallback(relay)
        .with_state(upstream);
    let early = Arc::new(AtomicU64::new(0));
    let listener = SameUser {
        inner: listener,
        uid,
        early: early.clone(),
    };
    let serve = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::warn!("the hook relay stopped: {err:#}");
        }
    });
    let count = early.clone();
    let report = tokio::spawn(async move {
        let mut tick = tokio::time::interval(EARLY_REPORT);
        // The first tick is at once.
        tick.tick().await;
        loop {
            tick.tick().await;
            report_early_closes(&count);
        }
    });
    Ok(Relay {
        path,
        tasks: vec![serve, keep_warm, report],
        early,
    })
}

/// Says how many hooks closed their connection before the relay accepted it since the last
/// report, if any. Each of them called the server directly instead.
fn report_early_closes(count: &AtomicU64) {
    let closed = count.swap(0, Ordering::Relaxed);
    if closed > 0 {
        tracing::info!(
            "hook relay: {closed} hooks closed their connection before it was accepted in the last {} minutes and called the server directly: the relay did not answer within their {} ms probe, as when this device is busy",
            EARLY_REPORT.as_secs() / 60,
            PROBE.as_millis(),
        );
    }
}

/// Accepts only connections from processes of `uid`, and counts in `early` those closed
/// before they were accepted.
struct SameUser {
    inner: UnixListener,
    uid: u32,
    early: Arc<AtomicU64>,
}

impl axum::serve::Listener for SameUser {
    type Io = UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, addr)) => match stream.peer_cred() {
                    Ok(cred) if cred.uid() == self.uid => return (stream, addr),
                    Ok(cred) => tracing::warn!(
                        "hook relay: refused a connection from user id {}",
                        cred.uid()
                    ),
                    // macOS has no credentials for a connection its client already closed: a
                    // hook whose probe gave up and called the server directly.
                    Err(err) if err.kind() == ErrorKind::NotConnected => {
                        tracing::debug!("hook relay: closed before it was accepted: {err}");
                        self.early.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(err) => tracing::warn!("hook relay: no peer credentials: {err}"),
                },
                Err(err) => {
                    // Out of file descriptors, say: wait instead of spinning.
                    tracing::debug!("hook relay: accept failed: {err}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// When `config.toml` was last written, to notice a `board login`.
type Stamp = Option<(SystemTime, u64)>;

fn config_stamp(home: &Home) -> Stamp {
    let meta = std::fs::metadata(home.config_path()).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The server and device token the relay forwards with, and the config they came from.
#[derive(Clone)]
struct Login {
    stamp: Stamp,
    base: String,
    token: String,
}

impl Login {
    /// Reads the home's server and token, as `board login` left them.
    fn load(home: &Home) -> Result<Self> {
        // First, so a change while loading is noticed next time.
        let stamp = config_stamp(home);
        let config = Config::load(home)?;
        let (server, _) = config.require_login()?;
        let token = load_token(home)?.context("no device token")?;
        Ok(Self {
            stamp,
            base: server.trim_end_matches('/').to_owned(),
            token,
        })
    }
}

/// The server the relay forwards to, with the device token and a pool of connections.
struct Upstream {
    http: reqwest::Client,
    home: Home,
    login: Mutex<Arc<Login>>,
}

impl Upstream {
    fn new(home: Home, login: Login) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(UPSTREAM_CONNECT)
            .timeout(UPSTREAM_TIMEOUT)
            .pool_idle_timeout(UPSTREAM_IDLE)
            .tcp_keepalive(Duration::from_secs(30))
            .user_agent(concat!("agent-board/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building the relay's HTTP client")?;
        Ok(Self {
            http,
            home,
            login: Mutex::new(Arc::new(login)),
        })
    }

    /// The current login, reloaded when `config.toml` changed since it was read.
    async fn login(&self) -> Result<Arc<Login>> {
        let current = self
            .login
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if current.stamp == config_stamp(&self.home) {
            return Ok(current);
        }
        let home = self.home.clone();
        // The Keychain lookup runs a process.
        let fresh = Arc::new(tokio::task::spawn_blocking(move || Login::load(&home)).await??);
        tracing::info!("hook relay: reloaded the server and token after a config change");
        *self.login.lock().unwrap_or_else(PoisonError::into_inner) = fresh.clone();
        Ok(fresh)
    }

    /// Asks for the server's health on two connections at once, opening or keeping them.
    async fn warm(&self) {
        let Ok(login) = self.login().await else {
            return;
        };
        let health = || self.http.get(format!("{}/healthz", login.base)).send();
        let (a, b) = tokio::join!(health(), health());
        for result in [a, b] {
            if let Err(err) = result {
                tracing::debug!("hook relay: warming connections: {err}");
            }
        }
    }
}

/// `GET /relay`: the relay's build and server, after reloading a changed config. A relay
/// that cannot load its login answers 503, and hooks call the server directly.
async fn status(State(up): State<Arc<Upstream>>) -> Response {
    match up.login().await {
        Ok(login) => axum::Json(Status::of(&login.base)).into_response(),
        Err(err) => {
            tracing::warn!("hook relay: {err:#}");
            refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "the hook relay cannot load this device's login; hooks call the server directly until board watch is restarted",
            )
        }
    }
}

/// Forwards one hook request: the route and query, the caller's agent and the JSON body,
/// with the device token; nothing else the caller sent.
async fn relay(State(up): State<Arc<Upstream>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let agent = parts
        .headers
        .get(AGENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<AgentId>().ok());
    let Some(agent) = agent else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "send the calling agent's id in x-board-agent",
        );
    };
    let path = parts.uri.path();
    if !hook_route(&parts.method, path, &agent) {
        return refuse(
            StatusCode::FORBIDDEN,
            "the hook relay forwards only the requests hooks make for their own agent; call the server directly",
        );
    }
    let Ok(body) = axum::body::to_bytes(body, BODY_MAX).await else {
        return refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            "send at most 1 MiB through the hook relay, or call the server directly",
        );
    };
    let login = match up.login().await {
        Ok(login) => login,
        Err(err) => {
            tracing::warn!("hook relay: {err:#}");
            return refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "the hook relay cannot load this device's login; call the server directly",
            );
        }
    };
    let mut url = format!("{}{path}", login.base);
    if let Some(query) = parts.uri.query() {
        url.push('?');
        url.push_str(query);
    }
    let send = || {
        let mut out = up
            .http
            .request(parts.method.clone(), &url)
            .bearer_auth(&login.token)
            .header(AGENT_HEADER, agent.as_str())
            .body(body.clone());
        if let Some(kind) = parts.headers.get(header::CONTENT_TYPE) {
            out = out.header(header::CONTENT_TYPE, kind.clone());
        }
        forward(out)
    };
    // A kept-alive connection the server closed as it was reused fails without a timeout or
    // a connect error: try once more, on another connection.
    let answer = match send().await {
        Err(err) if !err.is_timeout() && !err.is_connect() => {
            tracing::debug!("hook relay: retrying after {err}");
            send().await
        }
        answer => answer,
    };
    match answer {
        Ok((status, headers, bytes)) => {
            let mut resp = Response::new(Body::from(bytes));
            *resp.status_mut() = status;
            for name in [
                header::CONTENT_TYPE.as_str(),
                VERSION_HEADER,
                COMMIT_HEADER,
                BUILT_HEADER,
            ] {
                if let Some(value) = headers.get(name) {
                    resp.headers_mut().insert(name, value.clone());
                }
            }
            resp
        }
        Err(err) => {
            let slow = err.is_timeout() && !err.is_connect();
            tracing::debug!("hook relay: no answer from the server: {err}");
            let (status, kind) = if slow {
                (StatusCode::GATEWAY_TIMEOUT, "slow")
            } else {
                (StatusCode::BAD_GATEWAY, "unreachable")
            };
            let mut resp = (status, format!("board server {kind}")).into_response();
            resp.headers_mut()
                .insert(RELAY_HEADER, HeaderValue::from_static(kind));
            resp
        }
    }
}

async fn forward(
    req: reqwest::RequestBuilder,
) -> reqwest::Result<(StatusCode, reqwest::header::HeaderMap, Bytes)> {
    let resp = req.send().await?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await?;
    Ok((status, headers, bytes))
}

/// An answer the relay makes without forwarding: plain text, marked `refused`.
fn refuse(status: StatusCode, message: &'static str) -> Response {
    let mut resp = (status, message).into_response();
    resp.headers_mut()
        .insert(RELAY_HEADER, HeaderValue::from_static("refused"));
    resp
}

/// The relay's socket for a hook of `home`, from file metadata alone: a socket owned by
/// this user, in a directory owned by this user that no one else may enter, in a home that
/// belongs to this user and that no one else may write. Cheap enough for every hook.
pub fn safe_socket(home: &Home) -> Option<PathBuf> {
    let path = socket_path(home);
    let uid = own_uid();
    let root = std::fs::metadata(home.root()).ok()?;
    if root.uid() != uid || root.mode() & 0o022 != 0 {
        return None;
    }
    let dir = std::fs::symlink_metadata(path.parent()?).ok()?;
    if !dir.is_dir() || dir.uid() != uid || dir.mode() & 0o077 != 0 {
        return None;
    }
    let socket = std::fs::symlink_metadata(&path).ok()?;
    (socket.file_type().is_socket() && socket.uid() == uid).then_some(path)
}

/// A client through the relay on `socket`, when the relay answers within [`PROBE`] with
/// this build and `server`: a socket left by a daemon that died refuses at once, and a
/// daemon of another build (the binary was upgraded under it) or one still on an earlier
/// login is passed by. The probe's connection stays open for the hook's requests.
pub async fn connect(socket: &Path, server: &str) -> Option<Client> {
    let client = match Client::relayed(socket, PROBE) {
        Ok(client) => client,
        Err(err) => {
            tracing::debug!("hook relay: {err:#}");
            return None;
        }
    };
    match client.relay_status().await {
        Ok(status) if status == Status::of(server.trim_end_matches('/')) => Some(client),
        Ok(status) => {
            tracing::debug!("hook relay: passed by: {status:?}");
            None
        }
        Err(err) => {
            tracing::debug!("hook relay at {}: {err:#}", socket.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_closes_are_reported_once() {
        let count = AtomicU64::new(3);
        report_early_closes(&count);
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "each report starts a new count"
        );
    }

    #[test]
    fn only_hook_routes_for_the_calling_agent_pass() {
        let me: AgentId = "t3:thread:a%3Ab".parse().unwrap();
        let mine = crate::client::enc(me.as_str());
        let other = crate::client::enc("claude:someone-else");
        let ok = [
            (Method::POST, "/v1/agents".to_owned()),
            (Method::POST, format!("/v1/agents/{mine}/state")),
            (Method::GET, format!("/v1/agents/{mine}/handoff")),
            (Method::GET, "/v1/snapshot".to_owned()),
            (Method::GET, "/v1/boards".to_owned()),
            (Method::GET, "/v1/tasks".to_owned()),
            (Method::POST, "/v1/touches".to_owned()),
        ];
        for (method, path) in &ok {
            assert!(hook_route(method, path, &me), "{method} {path}");
        }
        let refused = [
            (Method::POST, format!("/v1/agents/{other}/state")),
            (Method::GET, format!("/v1/agents/{other}/handoff")),
            (Method::PUT, format!("/v1/agents/{mine}/handoff")),
            (
                Method::POST,
                format!("/v1/agents/{mine}/../../tasks/x-1/done"),
            ),
            (Method::POST, "/v1/agents/../tasks".to_owned()),
            (Method::POST, "/v1/tasks".to_owned()),
            (Method::POST, "/v1/tasks/factory-1/claim".to_owned()),
            (Method::POST, "/v1/asks".to_owned()),
            (Method::GET, "/v1/admin/devices".to_owned()),
            (Method::GET, "/v1/events".to_owned()),
            (Method::GET, "/healthz".to_owned()),
            (Method::GET, "/v1/snapshot/".to_owned()),
        ];
        for (method, path) in &refused {
            assert!(!hook_route(method, path, &me), "{method} {path}");
        }
    }
}
