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
//! - A hook uses the relay when the socket is safe and answers, and calls the server
//!   directly otherwise. When the relay gets no answer from the server, it says so in
//!   [`RELAY_HEADER`], so the hook judges the server as it would have directly.

use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use board_proto::{AGENT_HEADER, AgentId, ApiError, COMMIT_HEADER, ErrorCode, VERSION_HEADER};
use tokio::net::{UnixListener, UnixStream};

use crate::config::Home;

/// Set by the relay on an answer it made because the server gave none: `unreachable` (no
/// connection) or `slow` (no answer in time).
pub const RELAY_HEADER: &str = "x-board-relay";
/// The relay's directory in `$BOARD_HOME`, and the socket in it.
const DIR: &str = "run";
const SOCKET: &str = "hook.sock";
/// Largest request body the relay forwards: a batch of touches is well under it.
const BODY_MAX: usize = 1 << 20;
/// Time for opening a connection to the server, TLS included; past it the server counts as
/// unreachable, as for a hook calling it directly.
const UPSTREAM_CONNECT: Duration = Duration::from_millis(500);
/// Bounds one forwarded request. Longer than any hook's budget, so the hook gives up first
/// and judges the server slow.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an idle connection to the server is kept for the next hook.
const UPSTREAM_IDLE: Duration = Duration::from_secs(300);
/// How long a hook waits for the relay to accept its connection.
const PROBE: Duration = Duration::from_millis(100);

/// The relay's socket for `home`.
pub fn socket_path(home: &Home) -> PathBuf {
    home.root().join(DIR).join(SOCKET)
}

/// This process's effective user id.
fn own_uid() -> u32 {
    rustix::process::geteuid().as_raw()
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
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        // Best effort: a socket left behind refuses connections, and hooks then call the
        // server directly until the next daemon replaces it.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Starts serving hooks of `home`, forwarding to `server` with `token`. The caller must
/// hold the home's watch lock, so a socket already there is a stale one and is replaced.
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
    let upstream = Arc::new(Upstream::new(server, token)?);
    upstream.warm();
    let app = axum::Router::new().fallback(relay).with_state(upstream);
    let listener = SameUser {
        inner: listener,
        uid,
    };
    let task = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::warn!("the hook relay stopped: {err:#}");
        }
    });
    Ok(Relay { path, task })
}

/// Accepts only connections from processes of `uid`.
struct SameUser {
    inner: UnixListener,
    uid: u32,
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

/// The server the relay forwards to, with the device token and a pool of connections.
struct Upstream {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Upstream {
    fn new(server: &str, token: String) -> Result<Self> {
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
            base: server.trim_end_matches('/').to_owned(),
            token,
        })
    }

    /// Opens a connection ahead of the first hook.
    fn warm(&self) {
        let health = self.http.get(format!("{}/healthz", self.base)).send();
        tokio::spawn(async move {
            if let Err(err) = health.await {
                tracing::debug!("hook relay: warming the connection: {err}");
            }
        });
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
            ErrorCode::Invalid,
            "the hook relay needs the calling agent in x-board-agent",
        );
    };
    let path = parts.uri.path();
    if !hook_route(&parts.method, path, &agent) {
        return refuse(
            StatusCode::FORBIDDEN,
            ErrorCode::Forbidden,
            "the hook relay forwards only the requests hooks make for their own agent",
        );
    }
    let body = match axum::body::to_bytes(body, BODY_MAX).await {
        Ok(body) => body,
        Err(_) => {
            return refuse(
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::Invalid,
                "request body too large for the hook relay",
            );
        }
    };
    let mut url = format!("{}{path}", up.base);
    if let Some(query) = parts.uri.query() {
        url.push('?');
        url.push_str(query);
    }
    let mut out = up
        .http
        .request(parts.method, url)
        .bearer_auth(&up.token)
        .header(AGENT_HEADER, agent.as_str())
        .body(body);
    if let Some(kind) = parts.headers.get(header::CONTENT_TYPE) {
        out = out.header(header::CONTENT_TYPE, kind.clone());
    }
    let answer = async {
        let resp = out.send().await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await?;
        Ok::<_, reqwest::Error>((status, headers, bytes))
    };
    match answer.await {
        Ok((status, headers, bytes)) => {
            let mut resp = Response::new(Body::from(bytes));
            *resp.status_mut() = status;
            for name in [header::CONTENT_TYPE.as_str(), VERSION_HEADER, COMMIT_HEADER] {
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

fn refuse(status: StatusCode, code: ErrorCode, message: &str) -> Response {
    let error = ApiError {
        code,
        message: message.to_owned(),
        next: Some("update board on this device and restart board watch".to_owned()),
    };
    (status, axum::Json(error)).into_response()
}

/// The socket a hook of `home` may use, from file metadata alone: a socket owned by this
/// user in a directory owned by this user that no one else may enter. Cheap enough for every
/// hook; [`answers`] then checks that a relay listens.
pub fn safe_socket(home: &Home) -> Option<PathBuf> {
    let path = socket_path(home);
    let uid = own_uid();
    let dir = std::fs::symlink_metadata(path.parent()?).ok()?;
    if !dir.is_dir() || dir.uid() != uid || dir.mode() & 0o077 != 0 {
        return None;
    }
    let socket = std::fs::symlink_metadata(&path).ok()?;
    (socket.file_type().is_socket() && socket.uid() == uid).then_some(path)
}

/// Whether a relay of this user accepts connections on `path` within [`PROBE`]. A socket
/// left by a daemon that died refuses at once.
pub async fn answers(path: &Path) -> bool {
    match tokio::time::timeout(PROBE, UnixStream::connect(path)).await {
        Ok(Ok(stream)) => stream.peer_cred().is_ok_and(|c| c.uid() == own_uid()),
        Ok(Err(err)) => {
            tracing::debug!("hook relay at {}: {err}", path.display());
            false
        }
        Err(_) => {
            tracing::debug!("hook relay at {}: no answer in {PROBE:?}", path.display());
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
