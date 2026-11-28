//! Persistent local `/mcp` HTTP client for T3 Code behind the [`T3`] and [`T3Rpc`] traits.
//!
//! Replaces repeated `t3` Electron CLI subprocess cold boots in `board watch` with:
//! 1. Discovery of the live T3 Code server's `origin` and `pid` from
//!    `<T3CODE_HOME|~/.t3>/userdata/server-runtime.json`, checking PID liveness before sending
//!    credentials.
//! 2. Issuing **one** reusable `mcp-client` Bearer session token via `t3 auth session issue`
//!    and reusing it across persistent keep-alive `/mcp` JSON-RPC calls, refreshing only on
//!    `401`, proactive expiry margin, or server `pid`/`origin` change, and revoking cleanly on
//!    [`T3Mcp::shutdown`].
//! 3. O(1) device-wide `running_threads_batch` via `t3_thread_list` (`allProjects: true`,
//!    live-only `statuses`, filtering out `relationshipToParent == "subagent"`), backed by a
//!    short injectable-clock TTL cache that invalidates immediately when runs start, receive
//!    messages, or stop.
//! 4. Routing read operations (`t3_thread_read`, `t3_thread_list`, `t3_project_list`) and
//!    full-access write operations (`t3_thread_launch`, `t3_thread_send` when the session token
//!    carries `rtc: "full-access"` and `orchestration:operate`) over `/mcp`, falling back
//!    automatically to [`T3Cli`] when runtime state or the MCP tool is unavailable, while
//!    preserving fail-closed safety when the T3 server is dead or unreachable.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use board_proto::Timestamp;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

use crate::config::ThreadModel;
use crate::t3::{
    BUSY_STATUSES, ListedThread, NewWorktree, RECENT_MESSAGES, RecentMessages, RunningBatch,
    SendMode, T3, T3Cli, T3Rpc, ThreadFacts, ThreadInfo, TimedOut, is_unknown_thread,
    parse_recent_messages, parse_running_threads, parse_thread_facts, parse_thread_id,
    parse_thread_info, parse_threads_titled,
};

/// Default TTL for the cached device-wide running-thread list.
pub const DEFAULT_RUNNING_CACHE_TTL: Duration = Duration::from_secs(3);
/// Default TTL for the cached `t3_project_list` mapping (`workspaceRoot -> projectId`).
pub const DEFAULT_PROJECTS_CACHE_TTL: Duration = Duration::from_secs(30);
/// Cooldown after a missing-runtime-state CLI probe fails with server-not-running; cleared
/// immediately if `server-runtime.json` appears.
pub const NEGATIVE_CLI_PROBE_TTL: Duration = Duration::from_secs(30);
/// Cooldown after a live-PID `/mcp` session initialization fails, preventing repeated
/// `t3 auth session issue` + `revoke` CLI storms while T3 is starting up or unhealthy.
pub const INIT_FAILURE_COOLDOWN: Duration = Duration::from_secs(10);

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MCP_WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_DELETE_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_REVOKE_TIMEOUT: Duration = Duration::from_secs(5);
const TOKEN_EXPIRY_MARGIN_SECS: i64 = 5 * 60;
const MAX_LIST_PAGES: usize = 50;
const MAX_READ_PAGES: usize = 10;
const MAX_TEXT_CHUNKS: usize = 10;
const READ_ITEM_MAX_CHARS: usize = 20_000;
const TITLE_MAX_CHARS: usize = 80;

/// Injectable UTC clock for TTL caches and token expiry checks.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

/// Injectable process liveness check for `server-runtime.json` PIDs.
pub type PidAliveCheck = Arc<dyn Fn(u32) -> bool + Send + Sync>;

/// Live server record written by T3 Code to `<baseDir>/userdata/server-runtime.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServerRuntimeState {
    pub version: u32,
    pub pid: u32,
    pub port: u16,
    pub origin: String,
    #[serde(rename = "startedAt", default)]
    pub started_at: Option<String>,
}

/// Why `server-runtime.json` did not yield a live T3 Code server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeStateError {
    /// File does not exist or is empty (T3 Code is cleanly stopped or predates runtime state).
    Missing(PathBuf),
    /// File exists, is valid JSON, and names a PID that is no longer alive.
    DeadPid {
        pid: u32,
        origin: String,
        path: PathBuf,
    },
    /// File could not be read or failed schema/URL validation.
    Invalid { path: PathBuf, detail: String },
}

impl std::fmt::Display for RuntimeStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(path) => {
                write!(
                    f,
                    "T3 Code server runtime state missing at {}",
                    path.display()
                )
            }
            Self::DeadPid { pid, origin, .. } => {
                write!(f, "T3 Code server (pid {pid}, {origin}) is not running")
            }
            Self::Invalid { path, detail } => {
                write!(
                    f,
                    "invalid T3 Code server runtime state at {}: {detail}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for RuntimeStateError {}

/// Resolves T3 Code's base directory from `T3CODE_HOME`, else `~/.t3`.
pub fn default_t3_base_dir() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("T3CODE_HOME").filter(|v| !v.is_empty()) {
        return Some(expand_tilde_path(&PathBuf::from(raw)));
    }
    dirs::home_dir().map(|h| h.join(".t3"))
}

fn expand_tilde_path(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// Returns `<base_dir>/userdata/server-runtime.json`.
pub fn runtime_state_path_for(base_dir: &Path) -> PathBuf {
    base_dir.join("userdata").join("server-runtime.json")
}

/// Checks whether a local process with `pid` is currently alive (`kill(pid, 0)` on Unix;
/// `EPERM` still counts as alive, matching T3 Code's `isProcessAlive`).
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) | Err(rustix::io::Errno::PERM) => true,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
pub fn is_process_alive(pid: u32) -> bool {
    pid > 0
}

/// Reads and validates T3 Code's `server-runtime.json`, refusing to return an origin whose
/// recorded PID is dead so credentials are never sent to a reused port.
pub fn read_live_runtime_state(
    path: &Path,
    pid_alive: &(dyn Fn(u32) -> bool + Send + Sync),
) -> std::result::Result<ServerRuntimeState, RuntimeStateError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(RuntimeStateError::Missing(path.to_path_buf()));
        }
        Err(err) => {
            return Err(RuntimeStateError::Invalid {
                path: path.to_path_buf(),
                detail: err.to_string(),
            });
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(RuntimeStateError::Missing(path.to_path_buf()));
    }
    let state: ServerRuntimeState =
        serde_json::from_str(trimmed).map_err(|err| RuntimeStateError::Invalid {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })?;
    if state.version != 1 {
        return Err(RuntimeStateError::Invalid {
            path: path.to_path_buf(),
            detail: format!("unsupported runtime state version {}", state.version),
        });
    }
    if state.pid == 0 {
        return Err(RuntimeStateError::Invalid {
            path: path.to_path_buf(),
            detail: "pid must be positive".to_owned(),
        });
    }
    let parsed_origin =
        url::Url::parse(&state.origin).map_err(|err| RuntimeStateError::Invalid {
            path: path.to_path_buf(),
            detail: format!("invalid origin {:?}: {err}", state.origin),
        })?;
    if !matches!(parsed_origin.scheme(), "http" | "https") || parsed_origin.host_str().is_none() {
        return Err(RuntimeStateError::Invalid {
            path: path.to_path_buf(),
            detail: format!("origin {:?} must be an http(s) URL with host", state.origin),
        });
    }
    if !pid_alive(state.pid) {
        return Err(RuntimeStateError::DeadPid {
            pid: state.pid,
            origin: state.origin,
            path: path.to_path_buf(),
        });
    }
    Ok(state)
}

#[derive(Debug, Clone)]
struct IssuedToken {
    session_id: String,
    token: String,
    expires_at: Option<Timestamp>,
    /// True only when the token's signed claims explicitly carry `rtc == "full-access"` and
    /// `orchestration:operate` scope. Standard `t3 auth session issue` tokens omit `rtc`,
    /// capping MCP writes at `approval-required`, so write operations use CLI for full parity.
    full_access_writes: bool,
}

#[derive(Debug)]
enum TokenIssueError {
    UnsupportedCli(String),
    Failed(String),
}

#[derive(Debug, Clone)]
struct ActiveSession {
    pid: u32,
    origin: String,
    session_id: String,
    bearer_token: String,
    expires_at: Option<Timestamp>,
    full_access_writes: bool,
    mcp_session_id: Option<String>,
    protocol_version: String,
}

impl ActiveSession {
    fn mcp_url(&self) -> String {
        format!("{}/mcp", self.origin.trim_end_matches('/'))
    }
}

#[derive(Debug, Clone)]
struct RunningCache {
    fetched_at: Timestamp,
    threads: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct NegativeProbe {
    probed_at: Timestamp,
    error: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ProjectEntry {
    id: String,
    #[serde(rename = "workspaceRoot")]
    workspace_root: String,
}

#[derive(Debug, Clone)]
struct ProjectsCache {
    fetched_at: Timestamp,
    projects: Vec<ProjectEntry>,
}

#[derive(Debug, Clone)]
struct InitFailure {
    pid: u32,
    origin: String,
    failed_at: Timestamp,
    error: String,
}

#[derive(Debug)]
enum McpError {
    /// `server-runtime.json` does not exist or is empty.
    RuntimeStateMissing(String),
    /// `server-runtime.json` exists with a dead PID or invalid contents.
    ServerDown(String),
    /// The `t3` binary does not support `auth session issue`.
    UnsupportedCli(String),
    /// Failed to issue a session token.
    TokenIssue(String),
    /// HTTP 401 from `/mcp`.
    Unauthorized,
    /// HTTP 404 or expired MCP session header.
    SessionExpired,
    /// Server reported the requested MCP tool does not exist.
    ToolMissing(String),
    /// Server rejected tool arguments (`-32602`).
    InvalidParams(String),
    /// Tool returned `isError: true`.
    ToolError {
        code: Option<String>,
        message: String,
    },
    /// JSON-RPC error other than missing tool / invalid params.
    RpcError { code: i64, message: String },
    /// Request timed out after being sent (preserves `crate::t3::TimedOut` for callers).
    Timeout(String),
    /// Transport or HTTP 5xx error talking to a live server.
    Transport(String),
}

impl McpError {
    fn should_fallback_to_cli(&self) -> bool {
        matches!(
            self,
            Self::RuntimeStateMissing(_)
                | Self::UnsupportedCli(_)
                | Self::ToolMissing(_)
                | Self::InvalidParams(_)
        )
    }

    fn is_write_capability_refusal(&self) -> bool {
        matches!(
            self,
            Self::ToolError { code: Some(code), .. }
                if matches!(
                    code.as_str(),
                    "capability_denied"
                        | "runtime_mode_escalation_denied"
                        | "interaction_mode_escalation_denied"
                )
        )
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RuntimeStateMissing(m)
            | Self::ServerDown(m)
            | Self::UnsupportedCli(m)
            | Self::TokenIssue(m)
            | Self::InvalidParams(m)
            | Self::Transport(m) => f.write_str(m),
            Self::Unauthorized => f.write_str("T3 Code MCP endpoint rejected credentials (401)"),
            Self::SessionExpired => f.write_str("T3 Code MCP session expired"),
            Self::ToolMissing(tool) => write!(f, "T3 Code server has no {tool} tool"),
            Self::ToolError { message, .. } => f.write_str(message),
            Self::RpcError { code, message } => write!(f, "MCP RPC error {code}: {message}"),
            Self::Timeout(op) => write!(f, "{op} timed out"),
        }
    }
}

impl std::error::Error for McpError {}

struct Inner {
    cli: T3Cli,
    http: reqwest::Client,
    /// Explicit base dir passed as `--base-dir` to `t3 auth session issue/revoke` when set.
    explicit_base_dir: Option<PathBuf>,
    /// Override for `server-runtime.json` path; when `None`, resolved from `T3CODE_HOME` / `~/.t3`.
    state_path_override: Option<PathBuf>,
    clock: Clock,
    pid_alive: PidAliveCheck,
    running_cache_ttl: Duration,
    projects_cache_ttl: Duration,
    read_timeout: Duration,
    write_timeout: Duration,
    next_id: AtomicU64,
    token_cli_unsupported: AtomicBool,
    shut_down: AtomicBool,
    cache_generation: AtomicU64,
    session: Mutex<Option<Arc<ActiveSession>>>,
    refresh_lock: AsyncMutex<()>,
    init_failure: Mutex<Option<InitFailure>>,
    running_cache: Mutex<Option<RunningCache>>,
    running_fetch_lock: AsyncMutex<()>,
    negative_probe: Mutex<Option<NegativeProbe>>,
    projects_cache: Mutex<Option<ProjectsCache>>,
}

/// Persistent local `/mcp` HTTP client wrapping [`T3Cli`] for token issuance and automatic
/// fallback. Cheaply [`Clone`]-able so `Daemon`, `T3Runner`, and `device_rpc` share a single
/// Bearer session, HTTP connection pool, and running-threads TTL cache.
#[derive(Clone)]
pub struct T3Mcp {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for T3Mcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("T3Mcp")
            .field("bin", &self.inner.cli.bin)
            .field("state_path_override", &self.inner.state_path_override)
            .field("running_cache_ttl", &self.inner.running_cache_ttl)
            .finish_non_exhaustive()
    }
}

impl T3Mcp {
    /// Creates a persistent local `/mcp` HTTP client backed by `cli` for token issuance and
    /// automatic fallback.
    pub fn new(cli: T3Cli) -> Self {
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(MCP_CONNECT_TIMEOUT)
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("building local T3 MCP reqwest client");
        Self {
            inner: Arc::new(Inner {
                cli,
                http,
                explicit_base_dir: None,
                state_path_override: None,
                clock: Arc::new(Utc::now),
                pid_alive: Arc::new(is_process_alive),
                running_cache_ttl: DEFAULT_RUNNING_CACHE_TTL,
                projects_cache_ttl: DEFAULT_PROJECTS_CACHE_TTL,
                read_timeout: MCP_REQUEST_TIMEOUT,
                write_timeout: MCP_WRITE_TIMEOUT,
                next_id: AtomicU64::new(1),
                token_cli_unsupported: AtomicBool::new(false),
                shut_down: AtomicBool::new(false),
                cache_generation: AtomicU64::new(0),
                session: Mutex::new(None),
                refresh_lock: AsyncMutex::new(()),
                init_failure: Mutex::new(None),
                running_cache: Mutex::new(None),
                running_fetch_lock: AsyncMutex::new(()),
                negative_probe: Mutex::new(None),
                projects_cache: Mutex::new(None),
            }),
        }
    }

    /// Underlying [`T3Cli`] used for token issuance and fallback.
    pub fn cli(&self) -> &T3Cli {
        &self.inner.cli
    }

    /// Overrides T3 Code's base directory (`--base-dir` and `<base_dir>/userdata/server-runtime.json`).
    #[must_use]
    pub fn with_base_dir(self, base_dir: PathBuf) -> Self {
        let state_path = runtime_state_path_for(&base_dir);
        self.rebuild(|inner| {
            inner.explicit_base_dir = Some(base_dir);
            inner.state_path_override = Some(state_path);
        })
    }

    /// Overrides the `server-runtime.json` path directly (for tests).
    #[must_use]
    pub fn with_state_path(self, state_path: PathBuf) -> Self {
        self.rebuild(|inner| {
            inner.state_path_override = Some(state_path);
        })
    }

    /// Overrides the clock used for TTL caches and token expiry checks (for tests).
    #[must_use]
    pub fn with_clock(self, clock: Clock) -> Self {
        self.rebuild(|inner| {
            inner.clock = clock;
        })
    }

    /// Overrides the PID liveness check (for tests).
    #[must_use]
    pub fn with_pid_alive(self, pid_alive: PidAliveCheck) -> Self {
        self.rebuild(|inner| {
            inner.pid_alive = pid_alive;
        })
    }

    /// Overrides the TTL for the cached device-wide running-threads snapshot.
    #[must_use]
    pub fn with_running_cache_ttl(self, ttl: Duration) -> Self {
        self.rebuild(|inner| {
            inner.running_cache_ttl = ttl;
        })
    }

    /// Overrides the per-request HTTP timeouts used for `/mcp` calls (for tests).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_request_timeouts(
        self,
        read_timeout: Duration,
        write_timeout: Duration,
    ) -> Self {
        self.rebuild(|inner| {
            inner.read_timeout = read_timeout;
            inner.write_timeout = write_timeout;
        })
    }

    fn rebuild(mut self, mutate: impl FnOnce(&mut Inner)) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("T3Mcp builder methods must be called before cloning");
        mutate(inner);
        self
    }

    fn resolved_state_path(&self) -> Option<PathBuf> {
        if let Some(path) = &self.inner.state_path_override {
            return Some(path.clone());
        }
        default_t3_base_dir().map(|base| runtime_state_path_for(&base))
    }

    fn read_runtime_state(&self) -> std::result::Result<ServerRuntimeState, RuntimeStateError> {
        let Some(path) = self.resolved_state_path() else {
            return Err(RuntimeStateError::Missing(PathBuf::from(
                "~/.t3/userdata/server-runtime.json",
            )));
        };
        read_live_runtime_state(&path, self.inner.pid_alive.as_ref())
    }

    fn clear_session(&self) -> Option<Arc<ActiveSession>> {
        self.inner
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn session_usable(
        &self,
        session: &ActiveSession,
        live: &ServerRuntimeState,
        force_replace_sid: Option<&str>,
    ) -> bool {
        if session.pid != live.pid || session.origin != live.origin {
            return false;
        }
        if force_replace_sid == Some(session.session_id.as_str()) {
            return false;
        }
        if let Some(expires_at) = session.expires_at {
            let now = (self.inner.clock)();
            if now + chrono::TimeDelta::seconds(TOKEN_EXPIRY_MARGIN_SECS) >= expires_at {
                return false;
            }
        }
        true
    }

    /// Returns a live initialized MCP session, issuing a single Bearer token and completing
    /// the MCP `initialize` handshake under `refresh_lock` only when needed.
    async fn ensure_session(
        &self,
        force_replace_sid: Option<&str>,
    ) -> std::result::Result<Arc<ActiveSession>, McpError> {
        if self.inner.shut_down.load(Ordering::SeqCst) {
            return Err(McpError::ServerDown(
                "T3 MCP client is shutting down".to_owned(),
            ));
        }

        let live = match self.read_runtime_state() {
            Ok(state) => state,
            Err(RuntimeStateError::Missing(path)) => {
                self.clear_session();
                return Err(McpError::RuntimeStateMissing(format!(
                    "T3 Code server runtime state missing at {}",
                    path.display()
                )));
            }
            Err(err @ (RuntimeStateError::DeadPid { .. } | RuntimeStateError::Invalid { .. })) => {
                self.clear_session();
                return Err(McpError::ServerDown(err.to_string()));
            }
        };

        // Fast path: reuse existing session with zero subprocesses and no async lock.
        if let Some(existing) = self
            .inner
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            && self.session_usable(&existing, &live, force_replace_sid)
        {
            return Ok(existing);
        }

        let _guard = self.inner.refresh_lock.lock().await;

        if self.inner.shut_down.load(Ordering::SeqCst) {
            return Err(McpError::ServerDown(
                "T3 MCP client is shutting down".to_owned(),
            ));
        }

        // Re-read runtime state and re-check cache under lock (single-flight).
        let live = match self.read_runtime_state() {
            Ok(state) => state,
            Err(RuntimeStateError::Missing(path)) => {
                self.clear_session();
                return Err(McpError::RuntimeStateMissing(format!(
                    "T3 Code server runtime state missing at {}",
                    path.display()
                )));
            }
            Err(err @ (RuntimeStateError::DeadPid { .. } | RuntimeStateError::Invalid { .. })) => {
                self.clear_session();
                return Err(McpError::ServerDown(err.to_string()));
            }
        };

        let existing = self
            .inner
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(existing) = existing.as_ref()
            && self.session_usable(existing, &live, force_replace_sid)
        {
            return Ok(Arc::clone(existing));
        }

        if self.inner.token_cli_unsupported.load(Ordering::Relaxed) {
            return Err(McpError::UnsupportedCli(
                "t3 auth session issue is not supported by this t3 binary".to_owned(),
            ));
        }

        // Avoid repeated `auth session issue` + `revoke` CLI storms while a live-PID T3 server
        // is starting up or returning errors on `/mcp`. Cleared immediately on PID/origin change.
        if let Some(cached_err) = self.active_init_failure(&live) {
            return Err(McpError::Transport(cached_err));
        }

        let issued =
            match issue_session_via_cli(&self.inner.cli, self.inner.explicit_base_dir.as_deref())
                .await
            {
                Ok(token) => token,
                Err(TokenIssueError::UnsupportedCli(detail)) => {
                    if !self
                        .inner
                        .token_cli_unsupported
                        .swap(true, Ordering::Relaxed)
                    {
                        tracing::warn!(
                            "t3 auth session issue unsupported ({detail}); falling back to t3 CLI"
                        );
                    }
                    return Err(McpError::UnsupportedCli(detail));
                }
                Err(TokenIssueError::Failed(detail)) => {
                    self.record_init_failure(&live, &detail);
                    return Err(McpError::TokenIssue(detail));
                }
            };

        if self.inner.shut_down.load(Ordering::SeqCst) {
            revoke_session_best_effort(
                &self.inner.cli,
                self.inner.explicit_base_dir.as_deref(),
                &issued.session_id,
            )
            .await;
            return Err(McpError::ServerDown(
                "T3 MCP client is shutting down".to_owned(),
            ));
        }

        let (mcp_session_id, protocol_version) =
            match self.initialize_mcp(&live.origin, &issued.token).await {
                Ok(pair) => pair,
                Err(err) => {
                    self.record_init_failure(&live, &err.to_string());
                    // Best-effort revoke the freshly minted token if handshake failed.
                    revoke_session_best_effort(
                        &self.inner.cli,
                        self.inner.explicit_base_dir.as_deref(),
                        &issued.session_id,
                    )
                    .await;
                    return Err(err);
                }
            };

        if self.inner.shut_down.load(Ordering::SeqCst) {
            if let Some(mcp_sid) = mcp_session_id.as_deref() {
                let req = self
                    .inner
                    .http
                    .delete(format!("{}/mcp", live.origin.trim_end_matches('/')))
                    .header("accept", "application/json, text/event-stream")
                    .header("authorization", format!("Bearer {}", issued.token))
                    .header("mcp-session-id", mcp_sid)
                    .header("mcp-protocol-version", &protocol_version)
                    .send();
                let _ = tokio::time::timeout(SHUTDOWN_DELETE_TIMEOUT, req).await;
            }
            revoke_session_best_effort(
                &self.inner.cli,
                self.inner.explicit_base_dir.as_deref(),
                &issued.session_id,
            )
            .await;
            return Err(McpError::ServerDown(
                "T3 MCP client is shutting down".to_owned(),
            ));
        }

        let next = Arc::new(ActiveSession {
            pid: live.pid,
            origin: live.origin,
            session_id: issued.session_id,
            bearer_token: issued.token,
            expires_at: issued.expires_at,
            full_access_writes: issued.full_access_writes,
            mcp_session_id,
            protocol_version,
        });

        let previous = self
            .inner
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(Arc::clone(&next));

        // Clear any failure cooldowns now that we reached a live server.
        *self
            .inner
            .init_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        *self
            .inner
            .negative_probe
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;

        // Proactively rotated session (not rejected with 401): revoke old session best-effort.
        if let Some(prev) = previous
            && force_replace_sid != Some(prev.session_id.as_str())
            && prev.session_id != next.session_id
        {
            let cli = self.inner.cli.clone();
            let base_dir = self.inner.explicit_base_dir.clone();
            let old_sid = prev.session_id.clone();
            tokio::spawn(async move {
                revoke_session_best_effort(&cli, base_dir.as_deref(), &old_sid).await;
            });
        }

        Ok(next)
    }

    fn active_init_failure(&self, live: &ServerRuntimeState) -> Option<String> {
        let now = (self.inner.clock)();
        let cooldown = chrono::TimeDelta::from_std(INIT_FAILURE_COOLDOWN)
            .unwrap_or_else(|_| chrono::TimeDelta::seconds(10));
        let guard = self
            .inner
            .init_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let failure = guard.as_ref()?;
        if failure.pid != live.pid || failure.origin != live.origin {
            return None;
        }
        let age = now.signed_duration_since(failure.failed_at);
        if age >= chrono::TimeDelta::zero() && age < cooldown {
            Some(failure.error.clone())
        } else {
            None
        }
    }

    fn record_init_failure(&self, live: &ServerRuntimeState, error: &str) {
        *self
            .inner
            .init_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(InitFailure {
            pid: live.pid,
            origin: live.origin.clone(),
            failed_at: (self.inner.clock)(),
            error: error.to_owned(),
        });
    }

    /// Re-initializes the MCP HTTP session when the server dropped the in-memory `Mcp-Session-Id`
    /// while keeping the same Bearer token; refreshes the token if `initialize` returns 401.
    async fn reinitialize_mcp_session(
        &self,
        failed: &Arc<ActiveSession>,
    ) -> std::result::Result<Arc<ActiveSession>, McpError> {
        let _guard = self.inner.refresh_lock.lock().await;
        let current = self
            .inner
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(current) = current
            && (current.session_id != failed.session_id
                || current.mcp_session_id != failed.mcp_session_id)
        {
            return Ok(current);
        }

        let live = match self.read_runtime_state() {
            Ok(state) => state,
            Err(RuntimeStateError::Missing(path)) => {
                self.clear_session();
                return Err(McpError::RuntimeStateMissing(format!(
                    "T3 Code server runtime state missing at {}",
                    path.display()
                )));
            }
            Err(err @ (RuntimeStateError::DeadPid { .. } | RuntimeStateError::Invalid { .. })) => {
                self.clear_session();
                return Err(McpError::ServerDown(err.to_string()));
            }
        };

        if live.pid != failed.pid || live.origin != failed.origin {
            drop(_guard);
            return self.ensure_session(Some(&failed.session_id)).await;
        }

        match self
            .initialize_mcp(&live.origin, &failed.bearer_token)
            .await
        {
            Ok((mcp_session_id, protocol_version)) => {
                let renewed = Arc::new(ActiveSession {
                    mcp_session_id,
                    protocol_version,
                    ..(**failed).clone()
                });
                *self
                    .inner
                    .session
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&renewed));
                Ok(renewed)
            }
            Err(McpError::Unauthorized) => {
                drop(_guard);
                self.ensure_session(Some(&failed.session_id)).await
            }
            Err(err) => Err(err),
        }
    }

    fn next_request_id(&self) -> String {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        format!("board-mcp-{id}")
    }

    async fn initialize_mcp(
        &self,
        origin: &str,
        bearer_token: &str,
    ) -> std::result::Result<(Option<String>, String), McpError> {
        let url = format!("{}/mcp", origin.trim_end_matches('/'));
        let req_id = self.next_request_id();
        let body = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "agent-board",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        });
        let resp = self
            .inner
            .http
            .post(&url)
            .timeout(self.inner.read_timeout)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", format!("Bearer {bearer_token}"))
            .json(&body)
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    McpError::Timeout("t3 mcp initialize".to_owned())
                } else {
                    McpError::Transport(format!("POST {url} initialize: {err}"))
                }
            })?;

        let status = resp.status();
        let mcp_session_id = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let text = resp
            .text()
            .await
            .map_err(|err| McpError::Transport(format!("reading initialize response: {err}")))?;

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(McpError::Unauthorized);
        }
        if !status.is_success() {
            return Err(McpError::Transport(format!(
                "T3 Code MCP initialize returned HTTP {}: {}",
                status.as_u16(),
                compact_error_snippet(&text)
            )));
        }

        let envelope = parse_mcp_response_body(&content_type, &text, &req_id)
            .map_err(|err| McpError::Transport(format!("parsing initialize response: {err}")))?;
        if let Some(error) = envelope.get("error").filter(|v| !v.is_null()) {
            let code = error["code"].as_i64().unwrap_or(-32603);
            let message = error["message"]
                .as_str()
                .unwrap_or("initialize failed")
                .to_owned();
            return Err(McpError::RpcError { code, message });
        }
        let protocol_version = envelope["result"]["protocolVersion"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(MCP_PROTOCOL_VERSION)
            .to_owned();

        // Send notifications/initialized (202/200/204).
        let mut notif = self
            .inner
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", format!("Bearer {bearer_token}"))
            .header("mcp-protocol-version", &protocol_version);
        if let Some(sid) = mcp_session_id.as_deref() {
            notif = notif.header("mcp-session-id", sid);
        }
        let notif_resp = notif
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            }))
            .send()
            .await
            .map_err(|err| McpError::Transport(format!("POST {url} initialized: {err}")))?;
        if notif_resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(McpError::Unauthorized);
        }
        if !notif_resp.status().is_success() {
            let status = notif_resp.status();
            let body = notif_resp.text().await.unwrap_or_default();
            return Err(McpError::Transport(format!(
                "T3 Code MCP notifications/initialized returned HTTP {}: {}",
                status.as_u16(),
                compact_error_snippet(&body)
            )));
        }

        Ok((mcp_session_id, protocol_version))
    }

    async fn post_tool_call(
        &self,
        session: &ActiveSession,
        tool: &str,
        args: &Value,
        timeout: Duration,
    ) -> std::result::Result<Value, McpError> {
        let url = session.mcp_url();
        let req_id = self.next_request_id();
        let body = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": args,
            },
        });
        let mut req = self
            .inner
            .http
            .post(&url)
            .timeout(timeout)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", format!("Bearer {}", session.bearer_token))
            .header("mcp-protocol-version", &session.protocol_version);
        if let Some(sid) = session.mcp_session_id.as_deref() {
            req = req.header("mcp-session-id", sid);
        }
        let resp = req.json(&body).send().await.map_err(|err| {
            if err.is_timeout() {
                McpError::Timeout(format!("t3 mcp {tool}"))
            } else {
                McpError::Transport(format!("POST {url} {tool}: {err}"))
            }
        })?;

        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let text = resp.text().await.map_err(|err| {
            if err.is_timeout() {
                McpError::Timeout(format!("t3 mcp {tool}"))
            } else {
                McpError::Transport(format!("reading {tool} response: {err}"))
            }
        })?;

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(McpError::Unauthorized);
        }
        if status == reqwest::StatusCode::NOT_FOUND
            || (status == reqwest::StatusCode::BAD_REQUEST
                && text.to_ascii_lowercase().contains("session"))
        {
            return Err(McpError::SessionExpired);
        }
        if !status.is_success() {
            return Err(McpError::Transport(format!(
                "T3 Code MCP {tool} returned HTTP {}: {}",
                status.as_u16(),
                compact_error_snippet(&text)
            )));
        }

        let envelope = parse_mcp_response_body(&content_type, &text, &req_id)
            .map_err(|err| McpError::Transport(format!("parsing {tool} response: {err}")))?;
        if let Some(error) = envelope.get("error").filter(|v| !v.is_null()) {
            let code = error["code"].as_i64().unwrap_or(-32603);
            let message = error["message"]
                .as_str()
                .unwrap_or("MCP tool call failed")
                .to_owned();
            if (code == -32602 && message == format!("Tool '{tool}' not found")) || code == -32601 {
                return Err(McpError::ToolMissing(tool.to_owned()));
            }
            if code == -32602 {
                return Err(McpError::InvalidParams(message));
            }
            return Err(McpError::RpcError { code, message });
        }

        let result = envelope.get("result").ok_or_else(|| {
            McpError::Transport(format!("T3 Code MCP {tool} response missing result"))
        })?;

        if result["isError"].as_bool() == Some(true) {
            let (code, message) = extract_tool_error(result);
            return Err(McpError::ToolError { code, message });
        }

        if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
            return Ok(structured.clone());
        }
        if let Some(text) = first_content_text(result)
            && let Ok(parsed) = serde_json::from_str::<Value>(text)
        {
            return Ok(parsed);
        }
        Err(McpError::Transport(format!(
            "T3 Code MCP {tool} response missing structuredContent"
        )))
    }

    async fn call_tool(
        &self,
        tool: &str,
        args: Value,
        idempotent: bool,
    ) -> std::result::Result<Value, McpError> {
        let timeout = if idempotent {
            self.inner.read_timeout
        } else {
            self.inner.write_timeout
        };
        let session = self.ensure_session(None).await?;
        match self.post_tool_call(&session, tool, &args, timeout).await {
            Ok(val) => Ok(val),
            Err(McpError::Unauthorized) => {
                let refreshed = self.ensure_session(Some(&session.session_id)).await?;
                self.post_tool_call(&refreshed, tool, &args, timeout).await
            }
            Err(McpError::SessionExpired) => {
                let renewed = self.reinitialize_mcp_session(&session).await?;
                self.post_tool_call(&renewed, tool, &args, timeout).await
            }
            Err(McpError::Transport(first_err)) if idempotent => {
                // Server may have restarted on a new PID/port between state read and POST.
                match self.read_runtime_state() {
                    Ok(live) if live.pid != session.pid || live.origin != session.origin => {
                        let refreshed = self.ensure_session(Some(&session.session_id)).await?;
                        self.post_tool_call(&refreshed, tool, &args, timeout).await
                    }
                    Err(err @ RuntimeStateError::DeadPid { .. }) => {
                        self.clear_session();
                        Err(McpError::ServerDown(err.to_string()))
                    }
                    _ => Err(McpError::Transport(first_err)),
                }
            }
            Err(err) => Err(err),
        }
    }

    /// Cleanly terminates the MCP HTTP session (`DELETE /mcp`) and revokes the issued Bearer
    /// session via `t3 auth session revoke`. Serializes with `refresh_lock` and marks the client
    /// shut down so concurrent tasks cannot leak a newly issued session.
    pub async fn shutdown(&self) {
        self.inner.shut_down.store(true, Ordering::SeqCst);
        let _guard = tokio::time::timeout(SHUTDOWN_REVOKE_TIMEOUT, self.inner.refresh_lock.lock())
            .await
            .ok();
        let Some(session) = self.clear_session() else {
            return;
        };
        if let Some(mcp_sid) = session.mcp_session_id.as_deref() {
            let req = self
                .inner
                .http
                .delete(session.mcp_url())
                .header("accept", "application/json, text/event-stream")
                .header("authorization", format!("Bearer {}", session.bearer_token))
                .header("mcp-session-id", mcp_sid)
                .header("mcp-protocol-version", &session.protocol_version)
                .send();
            let _ = tokio::time::timeout(SHUTDOWN_DELETE_TIMEOUT, req).await;
        }
        revoke_session_best_effort(
            &self.inner.cli,
            self.inner.explicit_base_dir.as_deref(),
            &session.session_id,
        )
        .await;
    }

    // -------------------------------------------------------------------------
    // High-level MCP operations
    // -------------------------------------------------------------------------

    async fn list_running_threads_all_mcp(
        &self,
    ) -> std::result::Result<BTreeSet<String>, McpError> {
        let mut threads = BTreeSet::new();
        let mut cursor: Option<u64> = Some(0);
        for _ in 0..MAX_LIST_PAGES {
            let Some(page_cursor) = cursor else {
                break;
            };
            let page = self
                .call_tool(
                    "t3_thread_list",
                    json!({
                        "allProjects": true,
                        "statuses": BUSY_STATUSES,
                        "includeSubagents": true,
                        "limit": 100,
                        "cursor": page_cursor,
                    }),
                    true,
                )
                .await?;
            let Some(items) = page["threads"].as_array() else {
                return Err(McpError::Transport(
                    "t3_thread_list response missing threads array".to_owned(),
                ));
            };
            for item in items {
                if item["relationshipToParent"].as_str() == Some("subagent") {
                    continue;
                }
                if item["status"]
                    .as_str()
                    .is_some_and(|s| BUSY_STATUSES.contains(&s))
                    && let Some(id) = item["threadId"].as_str().filter(|s| !s.is_empty())
                {
                    threads.insert(id.to_owned());
                }
            }
            cursor = page["nextCursor"].as_u64();
        }
        Ok(threads)
    }

    async fn fetch_projects_mcp(&self) -> std::result::Result<Vec<ProjectEntry>, McpError> {
        let mut projects = Vec::new();
        let mut cursor: Option<u64> = Some(0);
        for _ in 0..MAX_LIST_PAGES {
            let Some(page_cursor) = cursor else {
                break;
            };
            let page = self
                .call_tool(
                    "t3_project_list",
                    json!({ "cursor": page_cursor, "limit": 100 }),
                    true,
                )
                .await?;
            let Some(items) = page["projects"].as_array() else {
                return Err(McpError::Transport(
                    "t3_project_list response missing projects array".to_owned(),
                ));
            };
            for item in items {
                if let Ok(entry) = serde_json::from_value::<ProjectEntry>(item.clone()) {
                    projects.push(entry);
                }
            }
            cursor = page["nextCursor"].as_u64();
        }
        let now = (self.inner.clock)();
        *self
            .inner
            .projects_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(ProjectsCache {
            fetched_at: now,
            projects: projects.clone(),
        });
        Ok(projects)
    }

    async fn cached_or_fetch_projects(
        &self,
        force_refresh: bool,
    ) -> std::result::Result<(Vec<ProjectEntry>, bool), McpError> {
        if !force_refresh {
            let now = (self.inner.clock)();
            let ttl = chrono::TimeDelta::from_std(self.inner.projects_cache_ttl)
                .unwrap_or_else(|_| chrono::TimeDelta::seconds(30));
            if let Some(cache) = self
                .inner
                .projects_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            {
                let age = now.signed_duration_since(cache.fetched_at);
                if age >= chrono::TimeDelta::zero() && age < ttl {
                    return Ok((cache.projects.clone(), true));
                }
            }
        }
        Ok((self.fetch_projects_mcp().await?, false))
    }

    async fn resolve_project_id(
        &self,
        project: &Path,
    ) -> std::result::Result<Option<String>, McpError> {
        let (projects, from_cache) = self.cached_or_fetch_projects(false).await?;
        if let Some(id) = match_project_id(&projects, project).await {
            return Ok(Some(id));
        }
        if from_cache {
            let (fresh, _) = self.cached_or_fetch_projects(true).await?;
            return Ok(match_project_id(&fresh, project).await);
        }
        Ok(None)
    }

    async fn list_project_threads_mcp(
        &self,
        project: &Path,
        statuses: Option<&[&str]>,
        title_contains: Option<&str>,
    ) -> std::result::Result<Option<Vec<Value>>, McpError> {
        let Some(project_id) = self.resolve_project_id(project).await? else {
            return Ok(None);
        };
        let mut threads = Vec::new();
        let mut cursor: Option<u64> = Some(0);
        for _ in 0..MAX_LIST_PAGES {
            let Some(page_cursor) = cursor else {
                break;
            };
            let mut args = json!({
                "projectId": project_id,
                "includeSubagents": true,
                "limit": 100,
                "cursor": page_cursor,
            });
            if let Some(statuses) = statuses {
                args["statuses"] = json!(statuses);
            }
            if let Some(title) = title_contains
                .map(str::trim)
                .filter(|t| !t.is_empty() && t.chars().count() <= 256)
            {
                args["titleContains"] = json!(title);
            }
            let page = self.call_tool("t3_thread_list", args, true).await?;
            let Some(items) = page["threads"].as_array() else {
                return Err(McpError::Transport(
                    "t3_thread_list response missing threads array".to_owned(),
                ));
            };
            for item in items {
                if item["relationshipToParent"].as_str() == Some("subagent") {
                    continue;
                }
                threads.push(item.clone());
            }
            cursor = page["nextCursor"].as_u64();
        }
        Ok(Some(threads))
    }

    async fn read_full_item_text(
        &self,
        thread_id: &str,
        item: &Value,
    ) -> std::result::Result<String, McpError> {
        let mut text = item["text"].as_str().unwrap_or_default().to_owned();
        let Some(item_id) = item["itemId"].as_str() else {
            return Ok(text);
        };
        let mut next_offset = if item["textTruncated"].as_bool() == Some(true) {
            item["nextTextOffset"].as_u64()
        } else {
            None
        };
        for _ in 0..MAX_TEXT_CHUNKS {
            let Some(offset) = next_offset else {
                break;
            };
            let page = self
                .call_tool(
                    "t3_thread_read",
                    json!({
                        "threadId": thread_id,
                        "itemId": item_id,
                        "textOffset": offset,
                        "runLimit": 1,
                        "maxCharsPerItem": READ_ITEM_MAX_CHARS,
                    }),
                    true,
                )
                .await?;
            let chunk = page["items"]
                .as_array()
                .and_then(|items| items.iter().find(|c| c["itemId"].as_str() == Some(item_id)));
            let Some(chunk) = chunk else {
                break;
            };
            if let Some(more) = chunk["text"].as_str() {
                text.push_str(more);
            }
            next_offset = if chunk["textTruncated"].as_bool() == Some(true) {
                chunk["nextTextOffset"].as_u64()
            } else {
                None
            };
        }
        Ok(text)
    }

    async fn show_thread_mcp(
        &self,
        thread: &str,
        messages: usize,
    ) -> std::result::Result<Option<Value>, McpError> {
        if messages == 0 {
            let res = self
                .call_tool(
                    "t3_thread_read",
                    json!({
                        "threadId": thread,
                        "limit": 1,
                        "runLimit": 1,
                        "include": ["thread"],
                    }),
                    true,
                )
                .await;
            let page = match res {
                Ok(page) => page,
                Err(McpError::ToolError { code, message })
                    if code.as_deref() == Some("thread_not_found")
                        || is_unknown_thread(&message, thread) =>
                {
                    return Ok(None);
                }
                Err(err) => return Err(err),
            };
            let Some(thread_obj) = page.get("thread").filter(|v| v.is_object()) else {
                return Err(McpError::Transport(
                    "t3_thread_read response missing thread object".to_owned(),
                ));
            };
            return Ok(Some(json!({
                "thread": thread_obj,
                "messages": [],
                "error": Value::Null,
            })));
        }

        let mut latest_thread: Option<Value> = None;
        let mut pages_oldest_first: Vec<Vec<Value>> = Vec::new();
        let mut collected = 0usize;
        let mut before_position: Option<u64> = None;
        let page_limit = messages.clamp(1, 100);

        for _ in 0..MAX_READ_PAGES {
            let mut args = json!({
                "threadId": thread,
                "view": "messages",
                "limit": page_limit,
                "maxCharsPerItem": READ_ITEM_MAX_CHARS,
                "runLimit": 1,
                "include": ["thread", "runs"],
            });
            if let Some(before) = before_position {
                args["beforePosition"] = json!(before);
            }
            let res = self.call_tool("t3_thread_read", args, true).await;
            let page = match res {
                Ok(page) => page,
                Err(McpError::ToolError { code, message })
                    if code.as_deref() == Some("thread_not_found")
                        || is_unknown_thread(&message, thread) =>
                {
                    return Ok(None);
                }
                Err(err) => return Err(err),
            };
            if latest_thread.is_none() {
                latest_thread = page.get("thread").filter(|v| v.is_object()).cloned();
            }
            let Some(items) = page["items"].as_array() else {
                return Err(McpError::Transport(
                    "t3_thread_read response missing items array".to_owned(),
                ));
            };
            let mut page_msgs = Vec::new();
            for item in items {
                let Some(kind) = item["type"].as_str() else {
                    continue;
                };
                if kind == "user_message" || kind == "assistant_message" {
                    page_msgs.push(item.clone());
                }
            }
            collected += page_msgs.len();
            pages_oldest_first.push(page_msgs);
            let earlier = page["earlierPosition"].as_u64();
            if collected >= messages || earlier.is_none() || earlier == before_position {
                break;
            }
            before_position = earlier;
        }

        let Some(thread_obj) = latest_thread else {
            return Err(McpError::Transport(
                "t3_thread_read response missing thread object".to_owned(),
            ));
        };

        // Reassemble oldest-first and keep only the newest `messages`.
        pages_oldest_first.reverse();
        let all_items: Vec<Value> = pages_oldest_first.into_iter().flatten().collect();
        let skip = all_items.len().saturating_sub(messages);
        let mut out_messages = Vec::with_capacity(all_items.len().saturating_sub(skip));
        for item in all_items.into_iter().skip(skip) {
            let role = if item["type"].as_str() == Some("assistant_message") {
                "assistant"
            } else if item["createdBy"].as_str() == Some("system") {
                "system"
            } else {
                "user"
            };
            let text = self.read_full_item_text(thread, &item).await?;
            let run_id = item
                .get("runId")
                .and_then(Value::as_str)
                .map_or(Value::Null, |s| json!(s));
            out_messages.push(json!({
                "role": role,
                "runId": run_id,
                "text": text,
            }));
        }

        Ok(Some(json!({
            "thread": thread_obj,
            "messages": out_messages,
            "error": Value::Null,
        })))
    }

    /// Reads a thread's parent, title, and link over `/mcp` with CLI fallback.
    pub async fn thread_facts(&self, thread: &str) -> Result<Option<ThreadFacts>> {
        Ok(self
            .show_thread_raw(thread, 0)
            .await?
            .map(|value| parse_thread_facts(&value)))
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_start_thread_mcp(
        &self,
        project: &Path,
        title: Option<&str>,
        message: &str,
        worktree_base: Option<&str>,
        branch: Option<&str>,
        local: bool,
        model: Option<&ThreadModel>,
    ) -> std::result::Result<String, McpError> {
        let session = self.ensure_session(None).await?;
        if !session.full_access_writes {
            return Err(McpError::ToolError {
                code: Some("capability_denied".to_owned()),
                message: "MCP session does not carry full-access write ceiling".to_owned(),
            });
        }
        let Some(project_id) = self.resolve_project_id(project).await? else {
            return Err(McpError::ToolError {
                code: Some("project_not_found".to_owned()),
                message: format!(
                    "No T3 Code project matches '{}'. Pass a project id or a path inside a project.",
                    project.display()
                ),
            });
        };
        let resolved_title = title
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| title_from_message(message));
        let mut args = json!({
            "projectId": project_id,
            "title": resolved_title,
            "message": message,
        });
        if local {
            args["workspaceStrategy"] = json!({ "type": "root" });
        } else if let Some(base) = worktree_base {
            let mut strategy = json!({ "type": "worktree", "baseRef": base });
            if let Some(branch) = branch {
                strategy["branch"] = json!(branch);
            }
            args["workspaceStrategy"] = strategy;
        }
        if let Some(ThreadModel {
            provider,
            model,
            options,
        }) = model
        {
            let mut selection = json!({
                "instanceId": provider,
                "model": model,
            });
            if !options.is_empty() {
                let opts: Vec<Value> = options
                    .iter()
                    .map(|(k, v)| {
                        let val = match v.as_str() {
                            "true" => Value::Bool(true),
                            "false" => Value::Bool(false),
                            other => Value::String(other.to_owned()),
                        };
                        json!({ "id": k, "value": val })
                    })
                    .collect();
                selection["options"] = Value::Array(opts);
            }
            args["modelSelection"] = selection;
        }
        let launched = self.call_tool("t3_thread_launch", args, false).await?;
        let text = launched.to_string();
        parse_thread_id(&text).ok_or_else(|| {
            McpError::Transport("t3_thread_launch response missing threadId".to_owned())
        })
    }

    async fn try_send_with_mode_mcp(
        &self,
        thread: &str,
        message: &str,
        mode: SendMode,
    ) -> std::result::Result<(), McpError> {
        let session = self.ensure_session(None).await?;
        if !session.full_access_writes {
            return Err(McpError::ToolError {
                code: Some("capability_denied".to_owned()),
                message: "MCP session does not carry full-access write ceiling".to_owned(),
            });
        }
        self.call_tool(
            "t3_thread_send",
            json!({
                "threadId": thread,
                "message": message,
                "mode": mode.as_cli_arg(),
            }),
            false,
        )
        .await?;
        Ok(())
    }

    async fn running_threads_cli_fallback(
        &self,
        projects: &[&Path],
        short_circuit_missing_state: bool,
        generation: u64,
    ) -> RunningBatch {
        if projects.is_empty() {
            return RunningBatch::default();
        }
        if short_circuit_missing_state {
            // Check negative probe cooldown first (avoids repeated CLI cold boots while T3 is off).
            let now = (self.inner.clock)();
            let cooldown = chrono::TimeDelta::from_std(NEGATIVE_CLI_PROBE_TTL)
                .unwrap_or_else(|_| chrono::TimeDelta::seconds(30));
            if let Some(neg) = self
                .inner
                .negative_probe
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            {
                let age = now.signed_duration_since(neg.probed_at);
                if age >= chrono::TimeDelta::zero() && age < cooldown {
                    return RunningBatch {
                        threads: BTreeSet::new(),
                        unread: vec![neg.error.clone()],
                    };
                }
            }
            // Probe only the first project before fanning out across all N projects.
            match self.inner.cli.running_threads(projects[0]).await {
                Err(err) => {
                    let msg = format!("{} ({err:#})", projects[0].display());
                    *self
                        .inner
                        .negative_probe
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Some(NegativeProbe {
                        probed_at: (self.inner.clock)(),
                        error: msg.clone(),
                    });
                    return RunningBatch {
                        threads: BTreeSet::new(),
                        unread: vec![msg],
                    };
                }
                Ok(first_threads) => {
                    *self
                        .inner
                        .negative_probe
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = None;
                    let mut batch = RunningBatch {
                        threads: first_threads.into_iter().collect(),
                        unread: Vec::new(),
                    };
                    if projects.len() > 1 {
                        let rest = projects[1..]
                            .iter()
                            .map(|project| self.inner.cli.running_threads(project));
                        let rest = futures_util::future::join_all(rest).await;
                        for (project, list) in projects[1..].iter().zip(rest) {
                            match list {
                                Ok(threads) => batch.threads.extend(threads),
                                Err(err) => batch
                                    .unread
                                    .push(format!("{} ({err:#})", project.display())),
                            }
                        }
                    }
                    if batch.unread.is_empty() {
                        self.store_running_cache_if_current(generation, &batch.threads);
                    }
                    return batch;
                }
            }
        }

        let lists = projects
            .iter()
            .map(|project| self.inner.cli.running_threads(project));
        let lists = futures_util::future::join_all(lists).await;
        let mut batch = RunningBatch::default();
        for (project, list) in projects.iter().zip(lists) {
            match list {
                Ok(threads) => batch.threads.extend(threads),
                Err(err) => batch
                    .unread
                    .push(format!("{} ({err:#})", project.display())),
            }
        }
        if batch.unread.is_empty() {
            self.store_running_cache_if_current(generation, &batch.threads);
        }
        batch
    }

    fn store_running_cache_if_current(&self, expected_generation: u64, threads: &BTreeSet<String>) {
        if self.inner.cache_generation.load(Ordering::SeqCst) != expected_generation {
            return;
        }
        let mut guard = self
            .inner
            .running_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.inner.cache_generation.load(Ordering::SeqCst) == expected_generation {
            *guard = Some(RunningCache {
                fetched_at: (self.inner.clock)(),
                threads: threads.clone(),
            });
        }
    }
}

impl T3 for T3Mcp {
    async fn start_thread(
        &self,
        project: &Path,
        title: &str,
        message: &str,
        worktree: Option<&NewWorktree>,
        model: Option<&ThreadModel>,
    ) -> Result<String> {
        self.start_thread_with_options(
            project,
            Some(title),
            message,
            worktree.map(|w| w.base.as_str()),
            worktree.map(|w| w.branch.as_str()),
            false,
            model,
        )
        .await
    }

    async fn send(&self, thread: &str, message: &str) -> Result<()> {
        self.send_with_mode(thread, message, SendMode::Queue).await
    }

    async fn send_with_mode(&self, thread: &str, message: &str, mode: SendMode) -> Result<()> {
        self.invalidate_running_cache();
        let res = match self.try_send_with_mode_mcp(thread, message, mode).await {
            Ok(()) => Ok(()),
            Err(McpError::Timeout(op)) => Err(anyhow::Error::new(TimedOut(op))),
            Err(err) if err.should_fallback_to_cli() || err.is_write_capability_refusal() => {
                self.inner.cli.send_with_mode(thread, message, mode).await
            }
            Err(err) => Err(anyhow::anyhow!("t3 thread send failed: {err}")),
        };
        self.invalidate_running_cache();
        res
    }

    async fn thread_info(&self, thread: &str) -> Result<Option<ThreadInfo>> {
        // `parse_thread_info` reads only the `thread` record, so `messages = 0` avoids
        // fetching timeline messages.
        Ok(self
            .show_thread_raw(thread, 0)
            .await?
            .map(|value| parse_thread_info(&value)))
    }

    async fn recent_messages(&self, thread: &str) -> Result<Option<RecentMessages>> {
        self.show_thread_raw(thread, RECENT_MESSAGES)
            .await?
            .map(|value| parse_recent_messages(&value))
            .transpose()
    }

    async fn thread_link(&self, thread: &str) -> Result<Option<String>> {
        Ok(self
            .thread_facts(thread)
            .await?
            .and_then(|facts| facts.link))
    }

    async fn threads_titled(&self, project: &Path, title: &str) -> Result<Vec<ListedThread>> {
        match self
            .list_project_threads_mcp(project, None, Some(title))
            .await
        {
            Ok(Some(items)) => Ok(parse_threads_titled(&Value::Array(items), title)),
            Ok(None) => bail!(
                "t3 thread list failed: T3 has no project for {}",
                project.display()
            ),
            Err(McpError::Timeout(op)) => Err(anyhow::Error::new(TimedOut(op))),
            Err(err) if err.should_fallback_to_cli() => {
                self.inner.cli.threads_titled(project, title).await
            }
            Err(err) => bail!("t3 thread list failed: {err}"),
        }
    }

    async fn running_threads(&self, project: &Path) -> Result<Vec<String>> {
        match self
            .list_project_threads_mcp(project, Some(&BUSY_STATUSES), None)
            .await
        {
            Ok(Some(items)) => Ok(parse_running_threads(&Value::Array(items))),
            Ok(None) => Ok(Vec::new()),
            Err(McpError::Timeout(op)) => Err(anyhow::Error::new(TimedOut(op))),
            Err(err) if err.should_fallback_to_cli() => {
                self.inner.cli.running_threads(project).await
            }
            Err(err) => bail!("t3 thread list failed: {err}"),
        }
    }

    async fn running_threads_batch(&self, projects: &[&Path]) -> Option<RunningBatch> {
        if projects.is_empty() {
            return Some(RunningBatch::default());
        }

        // Fast path: serve from short injectable-clock TTL cache when warm.
        if let Some(cached) = self.cached_running_threads() {
            return Some(RunningBatch {
                threads: cached,
                unread: Vec::new(),
            });
        }

        // Coalesce concurrent cold-cache fetches so rapid SSE/heartbeat bursts make 1 call.
        let _guard = self.inner.running_fetch_lock.lock().await;
        if let Some(cached) = self.cached_running_threads() {
            return Some(RunningBatch {
                threads: cached,
                unread: Vec::new(),
            });
        }

        let generation = self.inner.cache_generation.load(Ordering::SeqCst);
        match self.list_running_threads_all_mcp().await {
            Ok(threads) => {
                self.store_running_cache_if_current(generation, &threads);
                Some(RunningBatch {
                    threads,
                    unread: Vec::new(),
                })
            }
            Err(McpError::RuntimeStateMissing(_)) => Some(
                self.running_threads_cli_fallback(projects, true, generation)
                    .await,
            ),
            Err(
                McpError::ToolMissing(_) | McpError::InvalidParams(_) | McpError::UnsupportedCli(_),
            ) => Some(
                self.running_threads_cli_fallback(projects, false, generation)
                    .await,
            ),
            // Fail-closed: dead PID, unreachable live server, timeout, 5xx, 401-after-refresh.
            Err(err) => Some(RunningBatch {
                threads: BTreeSet::new(),
                unread: vec![format!("T3 server ({err})")],
            }),
        }
    }

    fn invalidate_running_cache(&self) {
        self.inner.cache_generation.fetch_add(1, Ordering::SeqCst);
        *self
            .inner
            .running_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        *self
            .inner
            .negative_probe
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }
}

impl T3Mcp {
    fn cached_running_threads(&self) -> Option<BTreeSet<String>> {
        if self.inner.running_cache_ttl.is_zero() {
            return None;
        }
        let now = (self.inner.clock)();
        let ttl = chrono::TimeDelta::from_std(self.inner.running_cache_ttl)
            .unwrap_or_else(|_| chrono::TimeDelta::seconds(3));
        let guard = self
            .inner
            .running_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let cache = guard.as_ref()?;
        let age = now.signed_duration_since(cache.fetched_at);
        if age >= chrono::TimeDelta::zero() && age < ttl {
            Some(cache.threads.clone())
        } else {
            None
        }
    }
}

impl T3Rpc for T3Mcp {
    async fn show_thread_raw(&self, thread: &str, messages: usize) -> Result<Option<Value>> {
        match self.show_thread_mcp(thread, messages).await {
            Ok(val) => Ok(val),
            Err(McpError::Timeout(op)) => Err(anyhow::Error::new(TimedOut(op))),
            Err(err) if err.should_fallback_to_cli() => {
                self.inner.cli.show_thread_raw(thread, messages).await
            }
            Err(err) => bail!("t3 thread show failed: {err}"),
        }
    }

    async fn list_project_threads_raw(&self, project: &Path) -> Result<Option<Value>> {
        match self.list_project_threads_mcp(project, None, None).await {
            Ok(Some(items)) => Ok(Some(Value::Array(items))),
            Ok(None) => Ok(None),
            Err(McpError::Timeout(op)) => Err(anyhow::Error::new(TimedOut(op))),
            Err(err) if err.should_fallback_to_cli() => {
                self.inner.cli.list_project_threads_raw(project).await
            }
            Err(err) => bail!("t3 thread list failed: {err}"),
        }
    }

    async fn start_thread_with_options(
        &self,
        project: &Path,
        title: Option<&str>,
        message: &str,
        worktree_base: Option<&str>,
        branch: Option<&str>,
        local: bool,
        model: Option<&ThreadModel>,
    ) -> Result<String> {
        self.invalidate_running_cache();
        let res = match self
            .try_start_thread_mcp(project, title, message, worktree_base, branch, local, model)
            .await
        {
            Ok(id) => Ok(id),
            Err(McpError::Timeout(op)) => Err(anyhow::Error::new(TimedOut(op))),
            Err(err) if err.should_fallback_to_cli() || err.is_write_capability_refusal() => {
                self.inner
                    .cli
                    .start_thread_with_options(
                        project,
                        title,
                        message,
                        worktree_base,
                        branch,
                        local,
                        model,
                    )
                    .await
            }
            Err(err) => bail!("t3 thread start failed: {err}"),
        };
        self.invalidate_running_cache();
        res
    }
}

// -----------------------------------------------------------------------------
// Helper functions: token issuance/claims, response parsing, project resolution
// -----------------------------------------------------------------------------

/// Decodes the unsigned payload segment of a `<payload>.<sig>` T3 session token to inspect
/// whether it carries `rtc == "full-access"` and `orchestration:operate` scope.
pub(crate) fn token_has_full_access_writes(token: &str) -> bool {
    let Some((payload_b64, _sig)) = token.split_once('.') else {
        return false;
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64) else {
        return false;
    };
    let Ok(claims) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    let rtc_full = claims["rtc"].as_str() == Some("full-access");
    let has_operate = claims["scopes"].as_array().is_some_and(|scopes| {
        scopes
            .iter()
            .any(|s| s.as_str() == Some("orchestration:operate"))
    });
    rtc_full && has_operate
}

async fn issue_session_via_cli(
    cli: &T3Cli,
    base_dir: Option<&Path>,
) -> std::result::Result<IssuedToken, TokenIssueError> {
    let mut owned_args: Vec<String> = vec![
        "auth".to_owned(),
        "session".to_owned(),
        "issue".to_owned(),
        "--subject".to_owned(),
        "mcp-client".to_owned(),
        "--scope".to_owned(),
        "orchestration:read".to_owned(),
        "--ttl".to_owned(),
        "24h".to_owned(),
        "--label".to_owned(),
        "board-watch".to_owned(),
        "--json".to_owned(),
    ];
    if let Some(dir) = base_dir {
        owned_args.push("--base-dir".to_owned());
        owned_args.push(dir.to_string_lossy().into_owned());
    }
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let (ok, stdout, stderr) = cli.run(&arg_refs, None).await.map_err(|err| {
        TokenIssueError::Failed(format!("running t3 auth session issue: {err:#}"))
    })?;
    if !ok {
        let combined = format!("{stdout}\n{stderr}");
        if is_unsupported_auth_issue(&combined) {
            return Err(TokenIssueError::UnsupportedCli(compact_error_snippet(
                &combined,
            )));
        }
        return Err(TokenIssueError::Failed(format!(
            "t3 auth session issue failed: {}",
            compact_error_snippet(&combined)
        )));
    }
    let parsed = parse_json_object(&stdout).ok_or_else(|| {
        TokenIssueError::UnsupportedCli(format!(
            "t3 auth session issue did not print session JSON: {}",
            compact_error_snippet(&stdout)
        ))
    })?;
    let Some(session_id) = parsed["sessionId"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err(TokenIssueError::UnsupportedCli(
            "t3 auth session issue output missing sessionId".to_owned(),
        ));
    };
    let Some(token) = parsed["token"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err(TokenIssueError::UnsupportedCli(
            "t3 auth session issue output missing token".to_owned(),
        ));
    };
    let expires_at = parsed["expiresAt"]
        .as_str()
        .and_then(|s| s.parse::<Timestamp>().ok());
    let full_access_writes = token_has_full_access_writes(token);
    Ok(IssuedToken {
        session_id: session_id.to_owned(),
        token: token.to_owned(),
        expires_at,
        full_access_writes,
    })
}

async fn revoke_session_best_effort(cli: &T3Cli, base_dir: Option<&Path>, session_id: &str) {
    if session_id.trim().is_empty() {
        return;
    }
    let mut owned_args = vec!["auth".to_owned(), "session".to_owned(), "revoke".to_owned()];
    if let Some(dir) = base_dir {
        owned_args.push("--base-dir".to_owned());
        owned_args.push(dir.to_string_lossy().into_owned());
    }
    owned_args.push(session_id.to_owned());
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    match tokio::time::timeout(SHUTDOWN_REVOKE_TIMEOUT, cli.run(&arg_refs, None)).await {
        Ok(Ok((true, _, _))) => {
            tracing::debug!("revoked T3 MCP session {session_id}");
        }
        Ok(Ok((false, _, stderr))) => {
            tracing::debug!(
                "t3 auth session revoke {session_id} returned non-zero: {}",
                stderr.trim()
            );
        }
        Ok(Err(err)) => {
            tracing::debug!("revoking T3 MCP session {session_id}: {err:#}");
        }
        Err(_) => {
            tracing::debug!("timed out revoking T3 MCP session {session_id}");
        }
    }
}

fn is_unsupported_auth_issue(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("unrecognized flag")
        || lower.contains("unknown flag")
        || lower.contains("unknown option")
        || lower.contains("unknown command")
        || lower.contains("unrecognized command")
        || lower.contains("unexpected argument")
        || lower.contains("invalid subcommand")
}

fn parse_json_object(stdout: &str) -> Option<Value> {
    let trimmed = stdout.trim();
    if let Ok(val) = serde_json::from_str::<Value>(trimmed)
        && val.is_object()
    {
        return Some(val);
    }
    // Tolerate any leading log lines before the JSON object.
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end < start {
        return None;
    }
    let val = serde_json::from_str::<Value>(&trimmed[start..=end]).ok()?;
    val.is_object().then_some(val)
}

/// Parses an MCP Streamable HTTP response body (`application/json` or `text/event-stream`)
/// and returns the JSON-RPC envelope matching `expected_id`.
pub(crate) fn parse_mcp_response_body(
    content_type: &str,
    body: &str,
    expected_id: &str,
) -> Result<Value> {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mut payloads = Vec::new();
    if media_type == "text/event-stream" {
        for raw_event in split_sse_events(body) {
            let data: Vec<&str> = raw_event
                .lines()
                .filter_map(|line| {
                    let line = line.strip_suffix('\r').unwrap_or(line);
                    line.strip_prefix("data:")
                        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
                })
                .collect();
            if data.is_empty() {
                continue;
            }
            let joined = data.join("\n");
            let trimmed = joined.trim();
            if trimmed.is_empty() {
                continue;
            }
            let val: Value = serde_json::from_str(trimmed).with_context(|| {
                format!(
                    "invalid SSE JSON payload: {}",
                    compact_error_snippet(trimmed)
                )
            })?;
            if let Some(arr) = val.as_array() {
                payloads.extend(arr.iter().cloned());
            } else {
                payloads.push(val);
            }
        }
    } else {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            bail!("empty MCP response body");
        }
        let val: Value = serde_json::from_str(trimmed).with_context(|| {
            format!("invalid JSON response: {}", compact_error_snippet(trimmed))
        })?;
        if let Some(arr) = val.as_array() {
            payloads.extend(arr.iter().cloned());
        } else {
            payloads.push(val);
        }
    }

    payloads
        .into_iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(expected_id))
        .with_context(|| format!("no JSON-RPC response envelope matched id {expected_id:?}"))
}

fn split_sse_events(body: &str) -> Vec<String> {
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .split("\n\n")
        .filter(|chunk| !chunk.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn first_content_text(result: &Value) -> Option<&str> {
    result["content"]
        .as_array()?
        .iter()
        .find_map(|part| part["text"].as_str())
}

fn extract_tool_error(result: &Value) -> (Option<String>, String) {
    if let Some(sc_err) = result["structuredContent"]["error"].as_object() {
        let code = sc_err
            .get("code")
            .or_else(|| sc_err.get("_tag"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(msg) = sc_err.get("message").and_then(Value::as_str) {
            return (code, msg.to_owned());
        }
    }
    let text = first_content_text(result).unwrap_or("");
    if let Ok(parsed) = serde_json::from_str::<Value>(text)
        && let Some(msg) = parsed["message"].as_str()
    {
        let code = parsed["code"].as_str().map(str::to_owned);
        return (code, msg.to_owned());
    }
    if text.is_empty() {
        (None, "The tool call failed.".to_owned())
    } else {
        (None, text.to_owned())
    }
}

fn compact_error_snippet(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 200 {
        collapsed
    } else {
        let mut cut: String = collapsed.chars().take(199).collect();
        cut.push('…');
        cut
    }
}

fn title_from_message(message: &str) -> String {
    let first_line = message
        .trim()
        .lines()
        .next()
        .map(str::trim)
        .unwrap_or("New thread");
    if first_line.is_empty() {
        return "New thread".to_owned();
    }
    if first_line.chars().count() <= TITLE_MAX_CHARS {
        return first_line.to_owned();
    }
    let mut cut: String = first_line.chars().take(TITLE_MAX_CHARS - 1).collect();
    while cut.ends_with(char::is_whitespace) {
        cut.pop();
    }
    cut.push('…');
    cut
}

fn path_contains(root: &Path, candidate: &Path) -> bool {
    candidate == root || candidate.starts_with(root)
}

/// Canonicalizes `path` when it exists; for a non-existent path, canonicalizes its nearest
/// existing ancestor and re-appends the remaining components so symlinked roots (e.g. macOS
/// `/tmp` -> `/private/tmp`) still match registered project roots and git worktrees.
fn resolve_canonical_path(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    let mut missing_tail = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        let Some(name) = cursor.file_name() else {
            return path.to_path_buf();
        };
        missing_tail.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            return path.to_path_buf();
        };
        cursor = parent;
    }
    let mut resolved = std::fs::canonicalize(cursor).unwrap_or_else(|_| cursor.to_path_buf());
    for part in missing_tail.into_iter().rev() {
        resolved.push(part);
    }
    resolved
}

async fn match_project_id(projects: &[ProjectEntry], requested: &Path) -> Option<String> {
    if let Some(raw) = requested.to_str()
        && let Some(found) = projects.iter().find(|p| p.id == raw)
    {
        return Some(found.id.clone());
    }

    let target = resolve_canonical_path(requested);
    let roots: Vec<(&ProjectEntry, PathBuf)> = projects
        .iter()
        .map(|p| (p, resolve_canonical_path(Path::new(&p.workspace_root))))
        .collect();

    let best_containing = |candidate: &Path, original: Option<&Path>| -> Option<String> {
        roots
            .iter()
            .filter(|(_, root)| path_contains(root, candidate))
            .max_by_key(|(entry, root)| {
                let raw_match = original
                    .filter(|orig| path_contains(Path::new(&entry.workspace_root), orig))
                    .map_or(0, |_| entry.workspace_root.len());
                (root.as_os_str().len(), raw_match)
            })
            .map(|(entry, _)| entry.id.clone())
    };

    if let Some(id) = best_containing(&target, Some(requested)) {
        return Some(id);
    }

    // Sibling git worktree fallback (matching `cli/thread.ts:resolveProject`).
    let mut existing = target.as_path();
    while !existing.exists() {
        existing = existing.parent()?;
    }
    let git_dir = if existing.is_file() {
        existing.parent()?
    } else {
        existing
    };
    let checkouts = list_git_checkouts(git_dir).await;
    if checkouts.is_empty() {
        return None;
    }
    let canonical_checkouts: Vec<PathBuf> = checkouts
        .iter()
        .map(|p| resolve_canonical_path(p))
        .collect();
    let own_checkout = canonical_checkouts
        .iter()
        .filter(|checkout| path_contains(checkout, &target))
        .max_by_key(|checkout| checkout.as_os_str().len())?;
    let rel = target.strip_prefix(own_checkout).ok()?;
    for checkout in &canonical_checkouts {
        if checkout == own_checkout {
            continue;
        }
        let candidate = checkout.join(rel);
        if let Some(id) = best_containing(&candidate, None) {
            return Some(id);
        }
    }
    None
}

async fn list_git_checkouts(directory: &Path) -> Vec<PathBuf> {
    let out = tokio::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await;
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut checkouts = Vec::new();
    for block in stdout.split("\n\n") {
        let mut path: Option<PathBuf> = None;
        let mut bare_or_prunable = false;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(rest.trim()));
            } else if line == "bare" || line.starts_with("prunable") {
                bare_or_prunable = true;
            }
        }
        if !bare_or_prunable && let Some(path) = path {
            checkouts.push(path);
        }
    }
    checkouts
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use board_proto::{DeviceRpcCall, Runtime};
    use tokio::net::TcpListener;

    use super::*;
    use crate::config::Config;
    use crate::daemon::{device, device_rpc};
    use crate::t3::UserMessage;

    fn make_signed_token(claims: &Value) -> String {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        format!("{payload}.fakesig")
    }

    fn read_only_token() -> String {
        make_signed_token(&json!({
            "v": 2,
            "kind": "session",
            "sid": "sid-1",
            "sub": "mcp-client",
            "scopes": ["orchestration:read"],
            "method": "bearer-access-token"
        }))
    }

    fn full_access_token() -> String {
        make_signed_token(&json!({
            "v": 2,
            "kind": "session",
            "sid": "sid-full",
            "sub": "mcp-client",
            "scopes": ["orchestration:read", "orchestration:operate"],
            "rtc": "full-access",
            "method": "bearer-access-token"
        }))
    }

    fn write_runtime_state(path: &Path, pid: u32, port: u16, origin: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            path,
            json!({
                "version": 1,
                "pid": pid,
                "port": port,
                "origin": origin,
                "startedAt": "2026-11-28T00:00:00.000Z"
            })
            .to_string(),
        )
        .unwrap();
    }

    /// Writes a fake `t3` CLI script that logs every invocation to `log_path`, responds to
    /// `auth session issue` with incrementing tokens, responds to `auth session revoke`, and
    /// handles `thread list`/`show`/`start`/`send` fallbacks.
    #[cfg(unix)]
    fn write_mock_t3_cli(
        dir: &Path,
        token_template: &str,
        expires_at: Option<&str>,
        cli_thread_list_json: &str,
        cli_thread_list_fails: bool,
    ) -> (T3Cli, PathBuf) {
        let bin = dir.join("t3");
        let log = dir.join("cli.log");
        let counter = dir.join("tok.count");
        let exp_field = expires_at
            .map(|e| format!(r#","expiresAt":"{e}""#))
            .unwrap_or_default();
        let list_body = if cli_thread_list_fails {
            r#"echo "No running T3 Code server" >&2; exit 1"#
        } else {
            cli_thread_list_json
        };
        let list_cmd = if cli_thread_list_fails {
            list_body.to_owned()
        } else {
            format!("printf '%s\\n' '{list_body}'")
        };
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
if [ "$1" = "auth" ] && [ "$2" = "session" ] && [ "$3" = "issue" ]; then
  n=0
  if [ -f '{counter}' ]; then n=$(cat '{counter}'); fi
  n=$((n + 1))
  printf '%s' "$n" > '{counter}'
  printf '{{"sessionId":"sid-%s","token":"{tok}-%s"{exp}}}\n' "$n" "$n"
  exit 0
fi
if [ "$1" = "auth" ] && [ "$2" = "session" ] && [ "$3" = "revoke" ]; then
  echo "Revoked."
  exit 0
fi
if [ "$1" = "thread" ] && [ "$2" = "list" ]; then
  {list_cmd}
  exit 0
fi
if [ "$1" = "thread" ] && [ "$2" = "start" ]; then
  cat > /dev/null
  echo '{{"threadId":"cli-started-1"}}'
  exit 0
fi
if [ "$1" = "thread" ] && [ "$2" = "send" ]; then
  cat > /dev/null
  echo 'Sent.'
  exit 0
fi
if [ "$1" = "thread" ] && [ "$2" = "show" ]; then
  echo '{{"thread":{{"threadId":"t-cli","status":"idle","providerInstanceId":"codex","archived":false,"activeRunId":null}},"messages":[]}}'
  exit 0
fi
echo "unexpected args: $*" >&2
exit 1
"#,
            log = log.display(),
            counter = counter.display(),
            tok = token_template,
            exp = exp_field,
            list_cmd = list_cmd,
        );
        crate::test_script::write_script(&bin, &script);
        (T3Cli::locate(Some(&bin)), log)
    }

    type MockToolHandler = Box<dyn Fn(&str, &Value, usize) -> Value + Send + Sync>;

    #[derive(Clone)]
    struct MockMcpState {
        inner: Arc<Mutex<MockMcpInner>>,
    }

    struct MockMcpInner {
        valid_tokens: BTreeSet<String>,
        allow_any_bearer: bool,
        expected_mcp_session_id: String,
        require_mcp_session: bool,
        use_sse: bool,
        tool_calls: Vec<(String, Value)>,
        init_count: usize,
        delete_count: usize,
        auth_headers: Vec<String>,
        queued_statuses: VecDeque<StatusCode>,
        handler: MockToolHandler,
    }

    impl MockMcpState {
        fn new(handler: impl Fn(&str, &Value, usize) -> Value + Send + Sync + 'static) -> Self {
            Self {
                inner: Arc::new(Mutex::new(MockMcpInner {
                    valid_tokens: BTreeSet::new(),
                    allow_any_bearer: true,
                    expected_mcp_session_id: "mcp-sess-1".to_owned(),
                    require_mcp_session: true,
                    use_sse: false,
                    tool_calls: Vec::new(),
                    init_count: 0,
                    delete_count: 0,
                    auth_headers: Vec::new(),
                    queued_statuses: VecDeque::new(),
                    handler: Box::new(handler),
                })),
            }
        }

        fn calls(&self) -> Vec<(String, Value)> {
            self.inner.lock().unwrap().tool_calls.clone()
        }

        fn init_count(&self) -> usize {
            self.inner.lock().unwrap().init_count
        }

        fn delete_count(&self) -> usize {
            self.inner.lock().unwrap().delete_count
        }
    }

    async fn mock_mcp_post(
        State(state): State<MockMcpState>,
        headers: HeaderMap,
        body: String,
    ) -> Response {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let req_mcp_sid = headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let method = parsed["method"].as_str().unwrap_or("");
        let id = parsed.get("id").cloned().unwrap_or(Value::Null);

        let mut inner = state.inner.lock().unwrap();
        inner.auth_headers.push(auth.clone());

        let token = auth.strip_prefix("Bearer ").unwrap_or("");
        if token.is_empty() || (!inner.allow_any_bearer && !inner.valid_tokens.contains(token)) {
            return (
                StatusCode::UNAUTHORIZED,
                [("content-type", "application/json")],
                r#"{"error":"invalid_mcp_credential"}"#,
            )
                .into_response();
        }

        if let Some(status) = inner.queued_statuses.pop_front() {
            return (
                status,
                [("content-type", "application/json")],
                format!(r#"{{"error":"forced_{}"}}"#, status.as_u16()),
            )
                .into_response();
        }

        if method == "initialize" {
            inner.init_count += 1;
            let sid = format!("mcp-sess-{}", inner.init_count);
            inner.expected_mcp_session_id = sid.clone();
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "Mock T3", "version": "0.1.14" }
                }
            });
            return (
                StatusCode::OK,
                [
                    ("content-type", "application/json".to_owned()),
                    ("mcp-session-id", sid),
                    ("mcp-protocol-version", "2025-06-18".to_owned()),
                ],
                resp.to_string(),
            )
                .into_response();
        }

        if method == "notifications/initialized" {
            return StatusCode::ACCEPTED.into_response();
        }

        if inner.require_mcp_session
            && req_mcp_sid.as_deref() != Some(inner.expected_mcp_session_id.as_str())
        {
            return (
                StatusCode::NOT_FOUND,
                [("content-type", "application/json")],
                r#"{"error":"unknown session"}"#,
            )
                .into_response();
        }

        if method == "tools/call" {
            let tool = parsed["params"]["name"].as_str().unwrap_or("").to_owned();
            let args = parsed["params"]["arguments"].clone();
            let call_idx = inner.tool_calls.len();
            inner.tool_calls.push((tool.clone(), args.clone()));
            let tool_resp = (inner.handler)(&tool, &args, call_idx);
            let envelope = if tool_resp.get("jsonrpc").is_some() {
                let mut env = tool_resp;
                env["id"] = id;
                env
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": tool_resp
                })
            };
            if inner.use_sse {
                let sse = format!(
                    "event: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
                    json!({"jsonrpc": "2.0", "method": "notifications/progress"}),
                    envelope
                );
                return (StatusCode::OK, [("content-type", "text/event-stream")], sse)
                    .into_response();
            }
            return (
                StatusCode::OK,
                [("content-type", "application/json")],
                envelope.to_string(),
            )
                .into_response();
        }

        (
            StatusCode::OK,
            [("content-type", "application/json")],
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
                .to_string(),
        )
            .into_response()
    }

    async fn mock_mcp_delete(State(state): State<MockMcpState>) -> StatusCode {
        state.inner.lock().unwrap().delete_count += 1;
        StatusCode::NO_CONTENT
    }

    async fn start_mock_mcp_server(
        state: MockMcpState,
    ) -> (String, u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let app = Router::new()
            .route("/mcp", post(mock_mcp_post).delete(mock_mcp_delete))
            .with_state(state);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (origin, port, handle)
    }

    #[test]
    fn runtime_state_discovery_validates_schema_origin_and_pid_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = runtime_state_path_for(dir.path());

        // Missing file -> Missing.
        assert!(matches!(
            read_live_runtime_state(&state_path, &|_| true),
            Err(RuntimeStateError::Missing(_))
        ));

        // Empty file -> Missing.
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        std::fs::write(&state_path, "   \n").unwrap();
        assert!(matches!(
            read_live_runtime_state(&state_path, &|_| true),
            Err(RuntimeStateError::Missing(_))
        ));

        // Unsupported version -> Invalid.
        std::fs::write(
            &state_path,
            r#"{"version":2,"pid":123,"port":3773,"origin":"http://127.0.0.1:3773"}"#,
        )
        .unwrap();
        assert!(matches!(
            read_live_runtime_state(&state_path, &|_| true),
            Err(RuntimeStateError::Invalid { .. })
        ));

        // Non-http origin -> Invalid.
        std::fs::write(
            &state_path,
            r#"{"version":1,"pid":123,"port":3773,"origin":"file:///etc/passwd"}"#,
        )
        .unwrap();
        assert!(matches!(
            read_live_runtime_state(&state_path, &|_| true),
            Err(RuntimeStateError::Invalid { .. })
        ));

        // Dead PID -> DeadPid (never sends credentials).
        write_runtime_state(&state_path, 4242, 3773, "http://127.0.0.1:3773");
        assert!(matches!(
            read_live_runtime_state(&state_path, &|pid| pid != 4242),
            Err(RuntimeStateError::DeadPid { pid: 4242, .. })
        ));

        // Live PID -> Ok.
        let live = read_live_runtime_state(&state_path, &|pid| pid == 4242).unwrap();
        assert_eq!(live.pid, 4242);
        assert_eq!(live.port, 3773);
        assert_eq!(live.origin, "http://127.0.0.1:3773");
    }

    #[test]
    fn token_claim_inspection_distinguishes_read_only_from_full_access() {
        assert!(!token_has_full_access_writes(&read_only_token()));
        assert!(!token_has_full_access_writes("opaque-token"));
        assert!(!token_has_full_access_writes(&make_signed_token(&json!({
            "scopes": ["orchestration:read", "orchestration:operate"],
            "rtc": "approval-required"
        }))));
        assert!(token_has_full_access_writes(&full_access_token()));
    }

    #[test]
    fn sse_and_json_mcp_response_parsing_matches_expected_request_id() {
        let json_body =
            r#"{"jsonrpc":"2.0","id":"req-1","result":{"structuredContent":{"ok":true}}}"#;
        let parsed =
            parse_mcp_response_body("application/json; charset=utf-8", json_body, "req-1").unwrap();
        assert_eq!(parsed["result"]["structuredContent"]["ok"], true);

        let sse_body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/log\"}\n\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":\"req-2\",\"result\":{\"structuredContent\":{\"n\":7}}}\n\n",
        );
        let parsed = parse_mcp_response_body("text/event-stream", sse_body, "req-2").unwrap();
        assert_eq!(parsed["result"]["structuredContent"]["n"], 7);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn running_threads_is_o1_filters_subagents_reuses_one_token_and_revokes_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = runtime_state_path_for(dir.path());
        let (cli, cli_log) = write_mock_t3_cli(dir.path(), &read_only_token(), None, "[]", false);

        let mock = MockMcpState::new(|tool, args, _| {
            assert_eq!(tool, "t3_thread_list");
            assert_eq!(args["allProjects"], json!(true));
            assert_eq!(args["statuses"], json!(BUSY_STATUSES));
            let cursor = args["cursor"].as_u64().unwrap_or(0);
            if cursor == 0 {
                json!({
                    "structuredContent": {
                        "projectId": null,
                        "currentThreadId": null,
                        "threads": [
                            {"threadId": "t-live-1", "status": "running", "relationshipToParent": null},
                            {"threadId": "t-sub-1", "status": "running", "relationshipToParent": "subagent"},
                            {"threadId": "t-fork-1", "status": "waiting", "relationshipToParent": "fork"},
                            {"threadId": "t-idle", "status": "completed", "relationshipToParent": null}
                        ],
                        "nextCursor": 4,
                        "total": 5
                    }
                })
            } else {
                json!({
                    "structuredContent": {
                        "projectId": null,
                        "currentThreadId": null,
                        "threads": [
                            {"threadId": "t-queued-2", "status": "queued", "relationshipToParent": null}
                        ],
                        "nextCursor": null,
                        "total": 5
                    }
                })
            }
        });
        mock.inner.lock().unwrap().use_sse = true;
        let (origin, port, server) = start_mock_mcp_server(mock.clone()).await;
        write_runtime_state(&state_path, 999, port, &origin);

        let now = Arc::new(Mutex::new(
            "2026-11-28T10:00:00Z".parse::<Timestamp>().unwrap(),
        ));
        let clock: Clock = {
            let now = Arc::clone(&now);
            Arc::new(move || *now.lock().unwrap())
        };
        let t3 = T3Mcp::new(cli)
            .with_base_dir(dir.path().to_path_buf())
            .with_clock(clock)
            .with_pid_alive(Arc::new(|pid| pid == 999))
            .with_running_cache_ttl(Duration::from_secs(3));

        // 18 configured projects -> still O(1) across all projects (2 paged MCP calls, 0 `t3 thread list` spawns).
        let mut config = Config::default();
        for i in 0..18 {
            config.projects.insert(
                format!("github.com/org/repo-{i}"),
                dir.path().join(format!("repo-{i}")),
            );
        }

        let r1 = device::running_threads(&t3, &config).await;
        assert!(r1.unread.is_empty(), "{r1:?}");
        assert_eq!(
            r1.threads.into_iter().collect::<Vec<_>>(),
            vec!["t-fork-1", "t-live-1", "t-queued-2"]
        );
        assert_eq!(mock.calls().len(), 2); // cursor 0 + cursor 4
        assert_eq!(mock.init_count(), 1);

        // Within 3s TTL -> served purely from cache (0 additional HTTP calls).
        *now.lock().unwrap() += chrono::TimeDelta::seconds(2);
        let r2 = device::running_threads(&t3, &config).await;
        assert_eq!(r2.threads.len(), 3);
        assert_eq!(mock.calls().len(), 2);

        // Past 3s TTL -> refreshes over same persistent MCP session & same Bearer token.
        *now.lock().unwrap() += chrono::TimeDelta::seconds(2);
        let r3 = device::running_threads(&t3, &config).await;
        assert_eq!(r3.threads.len(), 3);
        assert_eq!(mock.calls().len(), 4);
        assert_eq!(mock.init_count(), 1);

        // Explicit invalidation forces an immediate refresh even within TTL.
        t3.invalidate_running_cache();
        let _ = device::running_threads(&t3, &config).await;
        assert_eq!(mock.calls().len(), 6);
        assert_eq!(mock.init_count(), 1);

        // Clean shutdown sends DELETE /mcp and revokes the issued session.
        t3.shutdown().await;
        assert_eq!(mock.delete_count(), 1);

        let cli_invocations: Vec<String> = std::fs::read_to_string(&cli_log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(cli_invocations.len(), 2, "{cli_invocations:?}");
        assert!(cli_invocations[0].starts_with("auth session issue "));
        assert!(cli_invocations[0].contains("--subject mcp-client"));
        assert!(cli_invocations[0].contains("--scope orchestration:read"));
        assert!(cli_invocations[1].starts_with("auth session revoke "));
        assert!(cli_invocations[1].ends_with("sid-1"));

        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refreshes_token_on_401_reinitializes_on_404_and_reconnects_on_server_restart() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = runtime_state_path_for(dir.path());
        let base_tok = read_only_token();
        let (cli, cli_log) = write_mock_t3_cli(dir.path(), &base_tok, None, "[]", false);

        // Server 1: starts with token-1 valid, then we force 401 -> refreshes to token-2;
        // then we force 404 (dropped MCP session) -> re-initializes WITHOUT re-issuing token!
        let mock1 = MockMcpState::new(|_, _, _| {
            json!({
                "structuredContent": {
                    "projectId": null,
                    "currentThreadId": null,
                    "threads": [{"threadId": "from-server-1", "status": "running", "relationshipToParent": null}],
                    "nextCursor": null,
                    "total": 1
                }
            })
        });
        {
            let mut m = mock1.inner.lock().unwrap();
            m.allow_any_bearer = false;
            m.valid_tokens.insert(format!("{base_tok}-1"));
            m.valid_tokens.insert(format!("{base_tok}-2"));
        }
        let (origin1, port1, srv1) = start_mock_mcp_server(mock1.clone()).await;
        write_runtime_state(&state_path, 1001, port1, &origin1);

        let t3 = T3Mcp::new(cli)
            .with_base_dir(dir.path().to_path_buf())
            .with_pid_alive(Arc::new(|pid| pid == 1001 || pid == 2002))
            .with_running_cache_ttl(Duration::ZERO);

        let mut config = Config::default();
        config
            .projects
            .insert("github.com/org/a".into(), dir.path().join("a"));

        let r1 = device::running_threads(&t3, &config).await;
        assert_eq!(r1.threads, BTreeSet::from(["from-server-1".to_owned()]));
        assert_eq!(mock1.init_count(), 1);

        // Revoke token-1 on server 1 -> 401 triggers single token refresh to token-2.
        mock1
            .inner
            .lock()
            .unwrap()
            .valid_tokens
            .remove(&format!("{base_tok}-1"));
        let r2 = device::running_threads(&t3, &config).await;
        assert_eq!(r2.threads, BTreeSet::from(["from-server-1".to_owned()]));
        assert_eq!(mock1.init_count(), 2);

        // Drop MCP session (404) while keeping token-2 valid -> re-initializes without issuing token-3.
        mock1
            .inner
            .lock()
            .unwrap()
            .queued_statuses
            .push_back(StatusCode::NOT_FOUND);
        let r3 = device::running_threads(&t3, &config).await;
        assert_eq!(r3.threads, BTreeSet::from(["from-server-1".to_owned()]));
        assert_eq!(mock1.init_count(), 3);

        // Server restarts on a new PID & port (server 2) -> reconnects and issues token-3.
        let mock2 = MockMcpState::new(|_, _, _| {
            json!({
                "structuredContent": {
                    "projectId": null,
                    "currentThreadId": null,
                    "threads": [{"threadId": "from-server-2", "status": "running", "relationshipToParent": null}],
                    "nextCursor": null,
                    "total": 1
                }
            })
        });
        let (origin2, port2, srv2) = start_mock_mcp_server(mock2.clone()).await;
        write_runtime_state(&state_path, 2002, port2, &origin2);

        let r4 = device::running_threads(&t3, &config).await;
        assert_eq!(r4.threads, BTreeSet::from(["from-server-2".to_owned()]));
        assert_eq!(mock2.init_count(), 1);

        let issues = std::fs::read_to_string(&cli_log)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("auth session issue "))
            .count();
        assert_eq!(
            issues, 3,
            "expected exactly 3 token issues (init, 401, restart)"
        );

        srv1.abort();
        srv2.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fails_closed_on_dead_pid_and_unreachable_server_with_zero_cli_spawns() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = runtime_state_path_for(dir.path());
        let (cli, cli_log) = write_mock_t3_cli(dir.path(), &read_only_token(), None, "[]", false);

        // Dead PID in runtime state -> fail-closed (`unread` non-empty), 0 CLI spawns.
        write_runtime_state(&state_path, 7777, 1, "http://127.0.0.1:1");
        let t3 = T3Mcp::new(cli.clone())
            .with_base_dir(dir.path().to_path_buf())
            .with_pid_alive(Arc::new(|_| false));

        let mut config = Config::default();
        config
            .projects
            .insert("github.com/org/a".into(), dir.path().join("a"));
        config
            .projects
            .insert("github.com/org/b".into(), dir.path().join("b"));

        let r = device::running_threads(&t3, &config).await;
        assert!(r.threads.is_empty());
        assert!(r.unknown().is_some(), "dead PID must fail closed");
        assert!(!cli_log.exists(), "dead PID must spawn 0 CLI processes");

        // Live PID but unreachable/500 server -> fail-closed (`unread` non-empty).
        let mock = MockMcpState::new(|_, _, _| json!({}));
        mock.inner
            .lock()
            .unwrap()
            .queued_statuses
            .push_back(StatusCode::INTERNAL_SERVER_ERROR);
        let (origin, port, srv) = start_mock_mcp_server(mock.clone()).await;
        write_runtime_state(&state_path, 8888, port, &origin);
        let t3_live = T3Mcp::new(cli)
            .with_base_dir(dir.path().to_path_buf())
            .with_pid_alive(Arc::new(|pid| pid == 8888));

        let r_err = device::running_threads(&t3_live, &config).await;
        assert!(r_err.threads.is_empty());
        assert!(r_err.unknown().is_some(), "HTTP 500 must fail closed");
        // Only `auth session issue` ran (never `thread list` fallback on a 500!).
        let log_text = std::fs::read_to_string(&cli_log).unwrap();
        assert!(!log_text.contains("thread list"), "{log_text}");

        srv.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn falls_back_to_cli_when_runtime_state_missing_or_tool_missing_and_short_circuits_stopped_server()
     {
        let dir = tempfile::tempdir().unwrap();
        let state_path = runtime_state_path_for(dir.path());

        // Case 1: Missing runtime state + CLI fails (T3 cleanly stopped) -> 1 probe (not 18!),
        // fail-closed, and cached for NEGATIVE_CLI_PROBE_TTL so subsequent heartbeats spawn 0 CLIs.
        let (cli_fail, log_fail) =
            write_mock_t3_cli(dir.path(), &read_only_token(), None, "[]", true);
        let now = Arc::new(Mutex::new(
            "2026-11-28T10:00:00Z".parse::<Timestamp>().unwrap(),
        ));
        let clock: Clock = {
            let now = Arc::clone(&now);
            Arc::new(move || *now.lock().unwrap())
        };
        let t3_stopped = T3Mcp::new(cli_fail)
            .with_base_dir(dir.path().to_path_buf())
            .with_clock(clock)
            .with_pid_alive(Arc::new(|_| true));
        let mut config = Config::default();
        for i in 0..10 {
            config.projects.insert(
                format!("github.com/org/p-{i}"),
                dir.path().join(format!("p-{i}")),
            );
        }

        let r1 = device::running_threads(&t3_stopped, &config).await;
        assert!(r1.unknown().is_some());
        let r2 = device::running_threads(&t3_stopped, &config).await;
        assert!(r2.unknown().is_some());
        let calls = std::fs::read_to_string(&log_fail).unwrap().lines().count();
        assert_eq!(
            calls, 1,
            "stopped server must probe 1 project once, not 10x2"
        );

        // Case 2: MCP server answers ToolMissing -> falls back automatically to CLI.
        let dir2 = tempfile::tempdir().unwrap();
        let state_path2 = runtime_state_path_for(dir2.path());
        let (cli_ok, log_ok) = write_mock_t3_cli(
            dir2.path(),
            &read_only_token(),
            None,
            r#"[{"threadId":"cli-fallback-1","status":"running"}]"#,
            false,
        );
        let mock_missing = MockMcpState::new(|tool, _, _| {
            json!({
                "jsonrpc": "2.0",
                "error": { "code": -32602, "message": format!("Tool '{tool}' not found") }
            })
        });
        let (origin, port, srv) = start_mock_mcp_server(mock_missing).await;
        write_runtime_state(&state_path2, 3333, port, &origin);
        let t3_fb = T3Mcp::new(cli_ok)
            .with_base_dir(dir2.path().to_path_buf())
            .with_pid_alive(Arc::new(|pid| pid == 3333));
        let mut cfg2 = Config::default();
        cfg2.projects
            .insert("github.com/org/a".into(), dir2.path().join("a"));
        let r_fb = device::running_threads(&t3_fb, &cfg2).await;
        assert!(r_fb.unread.is_empty(), "{r_fb:?}");
        assert_eq!(r_fb.threads, BTreeSet::from(["cli-fallback-1".to_owned()]));
        assert!(
            std::fs::read_to_string(&log_ok)
                .unwrap()
                .contains("thread list --json --project")
        );

        // Case 3: Missing runtime state + CLI succeeds -> automatic CLI fallback works.
        let _ = std::fs::remove_file(&state_path);
        let dir3 = tempfile::tempdir().unwrap();
        let (cli_ok3, _) = write_mock_t3_cli(
            dir3.path(),
            &read_only_token(),
            None,
            r#"[{"threadId":"cli-only-1","status":"running"}]"#,
            false,
        );
        let t3_missing = T3Mcp::new(cli_ok3).with_base_dir(dir3.path().to_path_buf());
        let r_m = device::running_threads(&t3_missing, &cfg2).await;
        assert!(r_m.unread.is_empty());
        assert_eq!(r_m.threads, BTreeSet::from(["cli-only-1".to_owned()]));

        srv.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn routes_thread_read_list_device_rpc_and_gated_writes_with_parity_and_cache_invalidation()
     {
        let dir = tempfile::tempdir().unwrap();
        let state_path = runtime_state_path_for(dir.path());
        let repo_a = dir.path().join("repo-a");
        std::fs::create_dir_all(&repo_a).unwrap();
        let repo_a_str = repo_a.display().to_string();

        let running_version = Arc::new(AtomicUsize::new(1));
        let rv = Arc::clone(&running_version);
        let repo_a_clone = repo_a_str.clone();

        let mock = MockMcpState::new(move |tool, args, _| match tool {
            "t3_project_list" => json!({
                "structuredContent": {
                    "projects": [
                        {"id": "proj-a", "title": "repo-a", "workspaceRoot": repo_a_clone}
                    ],
                    "nextCursor": null
                }
            }),
            "t3_thread_list" => {
                if args["allProjects"].as_bool() == Some(true) {
                    let v = rv.load(Ordering::Relaxed);
                    json!({
                        "structuredContent": {
                            "projectId": null,
                            "currentThreadId": null,
                            "threads": [
                                {"threadId": format!("live-v{v}"), "status": "running", "relationshipToParent": null}
                            ],
                            "nextCursor": null,
                            "total": 1
                        }
                    })
                } else {
                    json!({
                        "structuredContent": {
                            "projectId": "proj-a",
                            "currentThreadId": null,
                            "threads": [
                                {
                                    "threadId": "t-1",
                                    "title": "worker alpha",
                                    "status": "running",
                                    "createdAt": "2026-11-28T01:00:00Z",
                                    "updatedAt": "2026-11-28T02:00:00Z",
                                    "latestRunId": "run-1",
                                    "itemCount": 2,
                                    "relationshipToParent": null
                                },
                                {
                                    "threadId": "t-sub",
                                    "title": "worker alpha",
                                    "status": "running",
                                    "createdAt": "2026-11-28T01:00:00Z",
                                    "updatedAt": "2026-11-28T02:00:00Z",
                                    "latestRunId": "run-2",
                                    "itemCount": 1,
                                    "relationshipToParent": "subagent"
                                }
                            ],
                            "nextCursor": null,
                            "total": 2
                        }
                    })
                }
            }
            "t3_thread_read" => {
                if args["threadId"].as_str() == Some("missing-thread") {
                    return json!({
                        "isError": true,
                        "content": [{
                            "type": "text",
                            "text": "{\"code\":\"thread_not_found\",\"message\":\"Thread missing-thread is no longer available.\"}"
                        }]
                    });
                }
                if args.get("itemId").and_then(Value::as_str) == Some("item-2") {
                    return json!({
                        "structuredContent": {
                            "items": [{
                                "itemId": "item-2",
                                "type": "assistant_message",
                                "text": " world",
                                "textTruncated": false,
                                "nextTextOffset": null
                            }]
                        }
                    });
                }
                json!({
                    "structuredContent": {
                        "thread": {
                            "threadId": "t-1",
                            "title": "Worker Alpha",
                            "link": "https://app.example.test/threads/env-1/t-1",
                            "status": "running",
                            "activeRunId": "run-1",
                            "providerInstanceId": "antigravity",
                            "worktreePath": "/tmp/wt-1",
                            "runCount": 4,
                            "archived": false,
                            "parentThreadId": "parent-9",
                            "relationshipToParent": "subagent"
                        },
                        "items": [
                            {
                                "position": 0,
                                "itemId": "item-1",
                                "type": "user_message",
                                "createdBy": "user",
                                "runId": "run-1",
                                "text": "please fix it",
                                "textTruncated": false
                            },
                            {
                                "position": 1,
                                "itemId": "item-2",
                                "type": "assistant_message",
                                "createdBy": "agent",
                                "runId": "run-1",
                                "text": "hello",
                                "textTruncated": true,
                                "nextTextOffset": 5
                            }
                        ],
                        "earlierPosition": null,
                        "nextPosition": 1,
                        "hasMore": false
                    }
                })
            }
            "t3_thread_launch" => json!({
                "structuredContent": {
                    "threadId": "mcp-launched-1",
                    "projectId": "proj-a",
                    "runId": "run-10"
                }
            }),
            "t3_thread_send" => json!({
                "structuredContent": {
                    "runId": "run-11",
                    "delivery": "steered"
                }
            }),
            other => panic!("unexpected MCP tool call: {other}"),
        });

        let (origin, port, srv) = start_mock_mcp_server(mock.clone()).await;
        write_runtime_state(&state_path, 5555, port, &origin);

        // 1. Read-only token (default from `t3 auth session issue`): reads go via /mcp, writes
        //    automatically use CLI (no silent `approval-required` downgrade) and invalidate cache!
        let (cli_ro, log_ro) = write_mock_t3_cli(dir.path(), &read_only_token(), None, "[]", false);
        let t3_ro = T3Mcp::new(cli_ro)
            .with_base_dir(dir.path().to_path_buf())
            .with_pid_alive(Arc::new(|pid| pid == 5555))
            .with_running_cache_ttl(Duration::from_secs(60));

        let info = t3_ro.thread_info("t-1").await.unwrap().unwrap();
        assert_eq!(info.runtime, Runtime::Antigravity);
        assert_eq!(info.runs, Some(4));
        assert!(info.busy);
        assert_eq!(info.worktree, Some(PathBuf::from("/tmp/wt-1")));

        assert_eq!(t3_ro.thread_info("missing-thread").await.unwrap(), None);

        let recent = t3_ro.recent_messages("t-1").await.unwrap().unwrap();
        assert!(recent.busy);
        assert_eq!(
            recent.user,
            vec![UserMessage {
                text: "please fix it".to_owned(),
                run: Some("run-1".to_owned())
            }]
        );

        let link = t3_ro.thread_link("t-1").await.unwrap();
        assert_eq!(
            link.as_deref(),
            Some("https://app.example.test/threads/env-1/t-1")
        );

        let titled = t3_ro.threads_titled(&repo_a, "worker alpha").await.unwrap();
        assert_eq!(titled.len(), 1, "subagent must be filtered out");
        assert_eq!(titled[0].id, "t-1");
        assert!(titled[0].ran);

        // device_rpc `t3.thread.read` receives full stitched text ("hello world") over /mcp!
        let mut cfg = Config::default();
        cfg.projects
            .insert("github.com/org/repo-a".into(), repo_a.clone());
        let rpc_read = device_rpc::handle_call(
            &t3_ro,
            &cfg,
            &DeviceRpcCall {
                id: "rpc-1".into(),
                method: "t3.thread.read".into(),
                params: json!({"thread_id": "t-1", "messages": 6}),
                timeout_secs: 15,
            },
        )
        .await;
        assert!(rpc_read.ok, "{rpc_read:?}");
        let rpc_msgs = rpc_read.result.unwrap()["messages"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(rpc_msgs.len(), 2);
        assert_eq!(rpc_msgs[1]["role"], "assistant");
        assert_eq!(rpc_msgs[1]["text"], "hello world");

        // Warm running cache -> send invalidates it -> next call observes v2!
        let r_before = device::running_threads(&t3_ro, &cfg).await;
        assert_eq!(r_before.threads, BTreeSet::from(["live-v1".to_owned()]));
        running_version.store(2, Ordering::Relaxed);
        t3_ro
            .send_with_mode("t-1", "wake", SendMode::Auto)
            .await
            .unwrap();
        let r_after = device::running_threads(&t3_ro, &cfg).await;
        assert_eq!(r_after.threads, BTreeSet::from(["live-v2".to_owned()]));

        // Read-only token fell back to CLI for start/send (never sent `t3_thread_launch`/`t3_thread_send`).
        let launched_ro = t3_ro
            .start_thread(&repo_a, "title", "prompt", None, None)
            .await
            .unwrap();
        assert_eq!(launched_ro, "cli-started-1");
        let ro_log = std::fs::read_to_string(&log_ro).unwrap();
        assert!(ro_log.contains("thread send --mode auto t-1 -"), "{ro_log}");
        assert!(ro_log.contains("thread start --json"), "{ro_log}");
        assert!(
            !mock
                .calls()
                .iter()
                .any(|(name, _)| name == "t3_thread_launch" || name == "t3_thread_send")
        );

        // 2. Full-access token (`rtc: "full-access"` + `orchestration:operate`): routes
        //    `t3_thread_launch` and `t3_thread_send` directly over `/mcp`!
        let dir_fa = tempfile::tempdir().unwrap();
        let (cli_fa, log_fa) =
            write_mock_t3_cli(dir_fa.path(), &full_access_token(), None, "[]", false);
        let t3_fa = T3Mcp::new(cli_fa)
            .with_state_path(state_path.clone())
            .with_pid_alive(Arc::new(|pid| pid == 5555));
        let wt = NewWorktree {
            base: "origin/main".into(),
            branch: "task/test".into(),
        };
        let model = ThreadModel {
            provider: "antigravity".into(),
            model: "gemini-4-neon".into(),
            options: BTreeMap::from([("fastMode".into(), "true".into())]),
        };
        let launched_mcp = t3_fa
            .start_thread(&repo_a, "MCP Worker", "do work", Some(&wt), Some(&model))
            .await
            .unwrap();
        assert_eq!(launched_mcp, "mcp-launched-1");
        t3_fa
            .send_with_mode("mcp-launched-1", "steer now", SendMode::Steer)
            .await
            .unwrap();

        let fa_calls = mock.calls();
        let launch_call = fa_calls
            .iter()
            .find(|(name, _)| name == "t3_thread_launch")
            .unwrap();
        assert_eq!(launch_call.1["projectId"], "proj-a");
        assert_eq!(launch_call.1["workspaceStrategy"]["type"], "worktree");
        assert_eq!(launch_call.1["workspaceStrategy"]["baseRef"], "origin/main");
        assert_eq!(launch_call.1["workspaceStrategy"]["branch"], "task/test");
        assert_eq!(launch_call.1["modelSelection"]["options"][0]["value"], true);

        let send_call = fa_calls
            .iter()
            .find(|(name, _)| name == "t3_thread_send")
            .unwrap();
        assert_eq!(send_call.1["threadId"], "mcp-launched-1");
        assert_eq!(send_call.1["mode"], "steer");

        let fa_log = std::fs::read_to_string(&log_fa).unwrap();
        assert!(!fa_log.contains("thread start"), "{fa_log}");
        assert!(!fa_log.contains("thread send"), "{fa_log}");

        srv.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proactively_refreshes_near_expiry_and_paginates_thread_read_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = runtime_state_path_for(dir.path());
        let (cli, cli_log) = write_mock_t3_cli(
            dir.path(),
            &read_only_token(),
            Some("2026-11-28T11:00:00Z"),
            "[]",
            false,
        );

        let mock = MockMcpState::new(|tool, args, _| match tool {
            "t3_thread_read" => {
                if args.get("beforePosition").and_then(Value::as_u64) == Some(2) {
                    json!({
                        "structuredContent": {
                            "thread": {"threadId": "t-paged", "status": "idle", "activeRunId": null},
                            "items": [
                                {"position": 0, "itemId": "i0", "type": "user_message", "createdBy": "user", "runId": "r0", "text": "msg-0", "textTruncated": false},
                                {"position": 1, "itemId": "i1", "type": "user_message", "createdBy": "system", "runId": "r1", "text": "msg-1", "textTruncated": false}
                            ],
                            "earlierPosition": null,
                            "nextPosition": 1,
                            "hasMore": false
                        }
                    })
                } else {
                    json!({
                        "structuredContent": {
                            "thread": {"threadId": "t-paged", "status": "idle", "activeRunId": null},
                            "items": [
                                {"position": 2, "itemId": "i2", "type": "assistant_message", "createdBy": "agent", "runId": "r1", "text": "msg-2", "textTruncated": false}
                            ],
                            "earlierPosition": 2,
                            "nextPosition": 2,
                            "hasMore": false
                        }
                    })
                }
            }
            other => panic!("unexpected tool {other}"),
        });
        let (origin, port, srv) = start_mock_mcp_server(mock.clone()).await;
        write_runtime_state(&state_path, 6666, port, &origin);

        let now = Arc::new(Mutex::new(
            "2026-11-28T10:00:00Z".parse::<Timestamp>().unwrap(),
        ));
        let clock: Clock = {
            let now = Arc::clone(&now);
            Arc::new(move || *now.lock().unwrap())
        };
        let t3 = T3Mcp::new(cli)
            .with_base_dir(dir.path().to_path_buf())
            .with_clock(clock)
            .with_pid_alive(Arc::new(|pid| pid == 6666));

        let raw = t3.show_thread_raw("t-paged", 3).await.unwrap().unwrap();
        let msgs = raw["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["text"], "msg-0");
        assert_eq!(msgs[1]["role"], "system");
        assert_eq!(msgs[1]["text"], "msg-1");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["text"], "msg-2");
        assert_eq!(mock.init_count(), 1);

        // Advance clock to within 5m of `expiresAt` (10:56Z) -> proactive token refresh.
        *now.lock().unwrap() = "2026-11-28T10:56:00Z".parse::<Timestamp>().unwrap();
        let _ = t3.thread_info("t-paged").await.unwrap();
        assert_eq!(mock.init_count(), 2);

        let issues = std::fs::read_to_string(&cli_log)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("auth session issue "))
            .count();
        assert_eq!(issues, 2);

        srv.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mcp_write_timeout_preserves_timed_out_error_type() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = runtime_state_path_for(dir.path());
        let (cli, _) = write_mock_t3_cli(dir.path(), &full_access_token(), None, "[]", false);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let origin = format!("http://127.0.0.1:{port}");
        let app = Router::new().route(
            "/mcp",
            post(|body: String| async move {
                let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let method = parsed["method"].as_str().unwrap_or("");
                let id = parsed.get("id").cloned().unwrap_or(Value::Null);
                if method == "initialize" {
                    return (
                        StatusCode::OK,
                        [
                            ("content-type", "application/json"),
                            ("mcp-session-id", "mcp-slow-1"),
                        ],
                        json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18"}})
                            .to_string(),
                    )
                        .into_response();
                }
                if method == "notifications/initialized" {
                    return StatusCode::ACCEPTED.into_response();
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    json!({"jsonrpc":"2.0","id":id,"result":{"structuredContent":{"delivery":"queued"}}})
                        .to_string(),
                )
                    .into_response()
            }),
        );
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        write_runtime_state(&state_path, 4444, port, &origin);

        let t3 = T3Mcp::new(cli)
            .with_base_dir(dir.path().to_path_buf())
            .with_pid_alive(Arc::new(|pid| pid == 4444))
            .with_request_timeouts(Duration::from_secs(2), Duration::from_millis(60));

        let err = t3
            .send_with_mode("t-slow", "hello", SendMode::Auto)
            .await
            .unwrap_err();
        assert!(
            err.downcast_ref::<TimedOut>().is_some(),
            "expected TimedOut error, got: {err:#}"
        );
        srv.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalidation_during_inflight_fetch_prevents_stale_cache_write_and_init_failure_cools_down()
     {
        let dir = tempfile::tempdir().unwrap();
        let state_path = runtime_state_path_for(dir.path());
        let (cli, cli_log) = write_mock_t3_cli(dir.path(), &read_only_token(), None, "[]", false);

        // 1. Live PID with failing `/mcp` initialize -> init failure cooldown prevents repeated
        //    `auth session issue` + `revoke` CLI spawns within INIT_FAILURE_COOLDOWN.
        let mock_fail = MockMcpState::new(|_, _, _| json!({}));
        mock_fail
            .inner
            .lock()
            .unwrap()
            .queued_statuses
            .push_back(StatusCode::INTERNAL_SERVER_ERROR);
        mock_fail
            .inner
            .lock()
            .unwrap()
            .queued_statuses
            .push_back(StatusCode::INTERNAL_SERVER_ERROR);
        let (origin_fail, port_fail, srv_fail) = start_mock_mcp_server(mock_fail).await;
        write_runtime_state(&state_path, 7001, port_fail, &origin_fail);

        let now = Arc::new(Mutex::new(
            "2026-11-28T10:00:00Z".parse::<Timestamp>().unwrap(),
        ));
        let clock: Clock = {
            let now = Arc::clone(&now);
            Arc::new(move || *now.lock().unwrap())
        };
        let t3_fail = T3Mcp::new(cli)
            .with_base_dir(dir.path().to_path_buf())
            .with_clock(clock)
            .with_pid_alive(Arc::new(|pid| pid == 7001 || pid == 7002));
        let mut cfg = Config::default();
        cfg.projects
            .insert("github.com/org/a".into(), dir.path().join("a"));

        assert!(
            device::running_threads(&t3_fail, &cfg)
                .await
                .unknown()
                .is_some()
        );
        assert!(
            device::running_threads(&t3_fail, &cfg)
                .await
                .unknown()
                .is_some()
        );
        assert!(
            device::running_threads(&t3_fail, &cfg)
                .await
                .unknown()
                .is_some()
        );
        let issues = std::fs::read_to_string(&cli_log)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("auth session issue "))
            .count();
        assert_eq!(issues, 1, "init failure cooldown must issue only 1 token");
        srv_fail.abort();

        // 2. Concurrent invalidation during an in-flight running_threads_batch fetch prevents
        //    the stale fetch from populating running_cache.
        let dir2 = tempfile::tempdir().unwrap();
        let state_path2 = runtime_state_path_for(dir2.path());
        let (cli2, _) = write_mock_t3_cli(dir2.path(), &read_only_token(), None, "[]", false);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let call_num = Arc::new(AtomicUsize::new(0));

        let entered_srv = Arc::clone(&entered);
        let release_srv = Arc::clone(&release);
        let call_num_srv = Arc::clone(&call_num);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port2 = listener.local_addr().unwrap().port();
        let origin2 = format!("http://127.0.0.1:{port2}");
        let app = Router::new().route(
            "/mcp",
            post(move |body: String| {
                let entered = Arc::clone(&entered_srv);
                let release = Arc::clone(&release_srv);
                let call_num = Arc::clone(&call_num_srv);
                async move {
                    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let method = parsed["method"].as_str().unwrap_or("");
                    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
                    if method == "initialize" {
                        return (
                            StatusCode::OK,
                            [
                                ("content-type", "application/json"),
                                ("mcp-session-id", "mcp-race-1"),
                            ],
                            json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18"}})
                                .to_string(),
                        )
                            .into_response();
                    }
                    if method == "notifications/initialized" {
                        return StatusCode::ACCEPTED.into_response();
                    }
                    let n = call_num.fetch_add(1, Ordering::SeqCst) + 1;
                    if n == 1 {
                        entered.notify_one();
                        release.notified().await;
                        return (
                            StatusCode::OK,
                            [("content-type", "application/json")],
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "structuredContent": {
                                        "projectId": null,
                                        "currentThreadId": null,
                                        "threads": [{"threadId": "stale-t1", "status": "running", "relationshipToParent": null}],
                                        "nextCursor": null,
                                        "total": 1
                                    }
                                }
                            })
                            .to_string(),
                        )
                            .into_response();
                    }
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "structuredContent": {
                                    "projectId": null,
                                    "currentThreadId": null,
                                    "threads": [
                                        {"threadId": "stale-t1", "status": "running", "relationshipToParent": null},
                                        {"threadId": "fresh-t2", "status": "running", "relationshipToParent": null}
                                    ],
                                    "nextCursor": null,
                                    "total": 2
                                }
                            }
                        })
                        .to_string(),
                    )
                        .into_response()
                }
            }),
        );
        let srv2 = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        write_runtime_state(&state_path2, 7002, port2, &origin2);

        let t3 = T3Mcp::new(cli2)
            .with_base_dir(dir2.path().to_path_buf())
            .with_pid_alive(Arc::new(|pid| pid == 7002))
            .with_running_cache_ttl(Duration::from_secs(60));

        let t3_bg = t3.clone();
        let cfg_bg = cfg.clone();
        let first_task =
            tokio::spawn(async move { device::running_threads(&t3_bg, &cfg_bg).await });
        entered.notified().await;
        // Invalidate while fetch #1 is in flight, then let fetch #1 complete.
        t3.invalidate_running_cache();
        release.notify_one();
        let r1 = first_task.await.unwrap();
        assert_eq!(r1.threads, BTreeSet::from(["stale-t1".to_owned()]));

        // Because invalidation happened mid-flight, stale-t1 must NOT be cached; fetch #2 runs!
        let r2 = device::running_threads(&t3, &cfg).await;
        assert_eq!(
            r2.threads,
            BTreeSet::from(["fresh-t2".to_owned(), "stale-t1".to_owned()])
        );
        assert_eq!(call_num.load(Ordering::SeqCst), 2);
        srv2.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn match_project_id_resolves_symlinked_nonexistent_subpaths_and_sibling_git_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real");
        let main_repo = real_root.join("my-repo");
        let worktree_dir = real_root.join("my-repo-wt");
        let symlink_root = dir.path().join("link");
        std::fs::create_dir_all(&main_repo).unwrap();
        std::os::unix::fs::symlink(&real_root, &symlink_root).unwrap();

        // Initialize a real git repo + sibling worktree.
        let run_git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&main_repo, &["init", "-b", "main"]);
        run_git(&main_repo, &["config", "user.email", "test@example.com"]);
        run_git(&main_repo, &["config", "user.name", "Test"]);
        std::fs::write(main_repo.join("README.md"), "hi\n").unwrap();
        run_git(&main_repo, &["add", "README.md"]);
        run_git(&main_repo, &["commit", "-m", "init"]);
        run_git(
            &main_repo,
            &[
                "worktree",
                "add",
                "-b",
                "feat/wt",
                worktree_dir.to_str().unwrap(),
            ],
        );

        let projects = vec![ProjectEntry {
            id: "proj-main".to_owned(),
            workspace_root: main_repo.display().to_string(),
        }];

        // 1. Non-existent subpath accessed through a symlink resolves to `proj-main`.
        let via_symlink = symlink_root.join("my-repo/nonexistent/deep");
        assert_eq!(
            match_project_id(&projects, &via_symlink).await.as_deref(),
            Some("proj-main")
        );

        // 2. Sibling git worktree (and a non-existent subpath inside it via symlink) resolves to `proj-main`.
        assert_eq!(
            match_project_id(&projects, &worktree_dir).await.as_deref(),
            Some("proj-main")
        );
        let wt_subpath = symlink_root.join("my-repo-wt/new-subdir");
        assert_eq!(
            match_project_id(&projects, &wt_subpath).await.as_deref(),
            Some("proj-main")
        );
    }
}
