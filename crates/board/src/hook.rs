//! `board hook <event> --runtime claude|codex`: reads the runtime's hook JSON from stdin,
//! answers within 1.5 seconds and never fails the turn.
//!
//! Output shapes (Claude Code: code.claude.com/docs/en/hooks; Codex:
//! learn.chatgpt.com/docs/hooks.md):
//! - SessionStart, UserPromptSubmit, PostToolUse, PostToolUseFailure (Claude Code):
//!   `{"hookSpecificOutput": {"hookEventName":
//!   <event>, "additionalContext": <text>}}` on both runtimes.
//! - Stop: `{"decision": "block", "reason": <text>}` continues the turn with the reason on
//!   both runtimes; Codex requires JSON on stdout, so it gets `{}` otherwise.
//! - PreCompact (Claude Code): plain stdout becomes compaction instructions.
//! - PreToolUse (shell calls only): prints nothing, which lets the call run.
//! - StopFailure and SessionEnd: output is ignored.

use std::collections::HashSet;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use board_proto::{
    AgentId, AgentState, AgentStateUpdate, PATH_CHARS_MAX, Runtime, Snapshot, SnapshotFormat,
    SnapshotKind, TOUCH_PATHS_MAX, TaskId, TaskStatus, Timestamp, Touches,
};
use chrono::{DateTime, Local, NaiveTime, SubsecRound as _, TimeZone, Utc};
use clap::ValueEnum;
use serde_json::{Value, json};

use crate::client::{Client, RelayFailure, SnapshotQuery, TaskQuery, relay_failure};
use crate::config::{Config, Home, load_token};
use crate::identity::{RepoInfo, T3_THREAD_ENV, nested_here, resolve_agent, t3_identity_applies};
use crate::render;
use crate::state::{self, AgentLocal, DoneMark, ToolStart};
use crate::t3::T3Cli;

/// T3 thread ids of threads T3 started for a parent thread's `delegate_task`.
const DELEGATED_THREAD: &str = "thread:delegated-task:";
/// Where T3 tells its threads its CLI is.
const T3_CLI_ENV: &str = "T3CODE_CLI_PATH";
/// Time for asking T3 for a delegated thread's parent (about 0.2 seconds measured);
/// past it the agent registers without one.
const PARENT_BUDGET: Duration = Duration::from_millis(400);

/// Hard limit for the whole hook, start to output.
const BUDGET: Duration = Duration::from_millis(1500);
/// Kept free after the network work for writing state and output.
const MARGIN: Duration = Duration::from_millis(200);
/// Time for opening a connection to the server, TLS included (well under 0.1 s on a
/// tailnet). Past it the server counts as unreachable, not slow: a connect that hangs (a
/// peer asleep or dropping packets) never answers however long the hook waits.
const CONNECT_BUDGET: Duration = Duration::from_millis(500);
/// A failure to get an answer within a deadline shorter than this, because the hook's local
/// steps took the rest of the budget, leaves the breaker as it was.
const SERVER_SHARE_FAIR: Duration = Duration::from_millis(500);
/// After a failed connection (refused, DNS, TLS), hooks on this device use the cache for
/// this long instead of trying the server again.
const OFFLINE_BACKOFF: Duration = Duration::from_secs(60);
/// Replies that missed the hook's budget, in a row, before hooks on this device stop asking
/// for [`SLOW_BACKOFF`]. One slow reply (a busy Mac, a long query) opens nothing.
const SLOW_STRIKES: u32 = 3;
/// How long hooks on this device use the cache after [`SLOW_STRIKES`] slow replies.
const SLOW_BACKOFF: Duration = Duration::from_secs(10);
/// The device's breaker file in `$BOARD_HOME/state`, shared by every hook on the device.
const BREAKER: &str = "offline";
/// Heartbeats and alert checks from `PostToolUse` happen at most this often.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookEvent {
    #[value(alias = "SessionStart")]
    SessionStart,
    #[value(alias = "UserPromptSubmit")]
    UserPromptSubmit,
    #[value(alias = "PreToolUse")]
    PreToolUse,
    #[value(alias = "PostToolUse")]
    PostToolUse,
    /// Claude Code: a tool call failed, such as a shell command exiting non-zero.
    #[value(alias = "PostToolUseFailure")]
    PostToolUseFailure,
    #[value(alias = "Stop")]
    Stop,
    #[value(alias = "StopFailure")]
    StopFailure,
    #[value(alias = "SessionEnd")]
    SessionEnd,
    #[value(alias = "PreCompact")]
    PreCompact,
}

impl HookEvent {
    /// The runtime's name for the event.
    pub fn name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
            Self::SessionEnd => "SessionEnd",
            Self::PreCompact => "PreCompact",
        }
    }

    /// The `board hook` argument for the event.
    pub fn arg(self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::UserPromptSubmit => "user-prompt-submit",
            Self::PreToolUse => "pre-tool-use",
            Self::PostToolUse => "post-tool-use",
            Self::PostToolUseFailure => "post-tool-use-failure",
            Self::Stop => "stop",
            Self::StopFailure => "stop-failure",
            Self::SessionEnd => "session-end",
            Self::PreCompact => "pre-compact",
        }
    }
}

/// What a handler decided to say.
#[derive(Debug, Default, PartialEq)]
enum Reply {
    #[default]
    Nothing,
    /// Context for the model.
    Context(String),
    /// Continue the turn with this instruction (Stop).
    Block(String),
    /// Plain text (PreCompact instructions).
    Plain(String),
}

/// Entry point: reads stdin, handles the event, prints the reply. Never fails.
pub async fn run(event: HookEvent, runtime: Runtime, explicit_agent: Option<&str>) {
    let started = Instant::now();
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    // A guard around the whole hook; `handle` keeps its own, earlier deadline for the
    // network so it can still fall back to the cache.
    let guard = BUDGET.saturating_sub(started.elapsed() + MARGIN / 2);
    let reply = match tokio::time::timeout(
        guard,
        handle(event, runtime, explicit_agent, &input, started),
    )
    .await
    {
        Ok(Ok(reply)) => reply,
        Ok(Err(err)) => {
            tracing::debug!("hook {}: {err:#}", event.name());
            Reply::Nothing
        }
        Err(_) => {
            tracing::debug!("hook {}: over budget", event.name());
            Reply::Nothing
        }
    };
    if let Some(text) = render_reply(event, runtime, reply) {
        print!("{text}");
    }
}

fn render_reply(event: HookEvent, runtime: Runtime, reply: Reply) -> Option<String> {
    match reply {
        Reply::Context(text) => Some(
            json!({
                "hookSpecificOutput": {
                    "hookEventName": event.name(),
                    "additionalContext": text,
                }
            })
            .to_string(),
        ),
        Reply::Block(reason) => Some(json!({"decision": "block", "reason": reason}).to_string()),
        Reply::Plain(text) => Some(text),
        // Codex rejects plain or empty output from Stop.
        Reply::Nothing if event == HookEvent::Stop && runtime == Runtime::Codex => {
            Some("{}".into())
        }
        Reply::Nothing => None,
    }
}

struct Hook<'a> {
    event: HookEvent,
    /// The hook reaches the server through the device daemon's relay.
    relayed: bool,
    runtime: Runtime,
    /// The session runs inside another agent session (`identity::is_nested`).
    nested: bool,
    home: Home,
    device: board_proto::DeviceName,
    client: Client,
    agent: AgentId,
    input: &'a Value,
    local: AgentLocal,
    /// At `PostToolUse`, the files to report, in batches the server takes.
    touches: Vec<Touches>,
    /// At `SessionStart`, the T3 thread that delegated this agent's thread.
    t3_parent: Option<String>,
    /// At `Stop`, the done reminder once worked out, kept for when the server then fails.
    done_reminder: Option<String>,
    now: Timestamp,
}

async fn handle(
    event: HookEvent,
    runtime: Runtime,
    explicit_agent: Option<&str>,
    input: &Value,
    started: Instant,
) -> Result<Reply> {
    let mut steps = Steps::new(event, started);
    let home = Home::from_env()?;
    let shell_call = (input["tool_name"] == SHELL_TOOL)
        .then(|| input["tool_use_id"].as_str().filter(|s| !s.is_empty()))
        .flatten();
    if event == HookEvent::PreToolUse {
        // Runs before every shell call: local only, no token or server. Logged out, no
        // PostToolUse could report the call, so it leaves no mark.
        if let Some(tool_use) = shell_call
            && Config::load(&home)?.require_login().is_ok()
        {
            let (git_dir, head) =
                match tokio::time::timeout(HEAD_BUDGET, git_head(&input_cwd(input))).await {
                    Ok(Ok(found)) => (Some(found.git_dir), found.head),
                    _ => (None, None),
                };
            // After HEAD is read, so a HEAD that moves in between is seen after the call.
            let at = Utc::now();
            let start = ToolStart { at, git_dir, head };
            state::save_tool_start(&home, runtime, tool_use, &start)?;
        }
        return Ok(Reply::Nothing);
    }
    // Taken before any of the returns below (logged out, no agent, no token), so a shell
    // call's start mark never outlives it.
    let shell_start = match shell_call {
        Some(tool_use)
            if matches!(
                event,
                HookEvent::PostToolUse | HookEvent::PostToolUseFailure
            ) =>
        {
            state::take_tool_start(&home, runtime, tool_use)?
        }
        _ => None,
    };
    if event == HookEvent::SessionStart {
        // In every session, logged in or not: marks of shell calls that never reached
        // PostToolUse (failed to start, denied, interrupted, or hooks offline).
        if let Err(err) = state::prune_tool_starts(&home, TOOL_START_AGE) {
            tracing::debug!("pruning tool start marks: {err:#}");
        }
        // Done marks of turns whose Stop hook never ran (interrupted, or the session ended).
        if let Err(err) = state::prune_done(&home, DONE_MARK_AGE) {
            tracing::debug!("pruning done marks: {err:#}");
        }
        // Hook state and cached snapshots of agents gone for a week.
        if let Err(err) = state::prune_agents(&home, AGENT_FILE_AGE) {
            tracing::debug!("pruning agent files: {err:#}");
        }
    }
    let config = Config::load(&home)?;
    let Ok((server, device)) = config.require_login() else {
        return Ok(Reply::Nothing);
    };
    steps.mark("start");
    let session = input["session_id"].as_str().filter(|s| !s.is_empty());
    let env = |name: &str| std::env::var(name).ok();
    let post = matches!(
        event,
        HookEvent::PostToolUse | HookEvent::PostToolUseFailure
    );
    let edited = if post {
        let tool = input["tool_name"].as_str().unwrap_or_default();
        edited_paths(tool, &input["tool_input"])
    } else {
        Vec::new()
    };
    // A PostToolUse hook other than an edit tool's often has nothing to send, so it asks
    // for the relay or the token only once it knows it has.
    let early = !post || !edited.is_empty();
    // The device daemon's relay holds the device token, so a hook using it needs none. A
    // session given its own token in `BOARD_TOKEN` keeps it, and so calls directly.
    let socket = std::env::var_os("BOARD_TOKEN")
        .is_none_or(|t| t.is_empty())
        .then(|| crate::relay::safe_socket(&home))
        .flatten();
    let mut relay = match &socket {
        Some(socket) if early => crate::relay::connect(socket, server).await,
        _ => None,
    };
    // The Keychain lookup runs a process, while the process table and the work tree are
    // read below.
    let mut token = (early && relay.is_none()).then(|| spawn_token(&home));
    let nested = nested_session(&home, event, runtime, session, explicit_agent).await?;
    let Some(agent) = resolve_agent(explicit_agent, env, session.map(|s| (runtime, s)), nested)?
    else {
        return Ok(Reply::Nothing);
    };
    steps.mark("agent");
    let now = Utc::now();
    let breaker: Breaker = state::load_named(&home, BREAKER);
    if post && breaker.open_at(now).is_some() {
        // Mid-turn hooks stay quiet while the server is skipped: no scan, token or call.
        return Ok(Reply::Nothing);
    }
    let local = state::load_agent(&home, &agent);
    // Local work comes before the server's share of the budget is set, so a slow work tree
    // or T3 never looks like a slow server.
    let repo = if event == HookEvent::SessionStart || !edited.is_empty() || shell_start.is_some() {
        Some(detect_repo(input_cwd(input)).await?)
    } else {
        None
    };
    let touches = match &repo {
        Some(repo) if post => local_touches(&agent, repo, edited, shell_start.as_ref()).await,
        _ => Vec::new(),
    };
    let heartbeat = post && heartbeat_due(local.last_heartbeat, now);
    steps.mark("local");
    if post && touches.is_empty() && !heartbeat {
        // Nothing to send: no server call and no Keychain lookup.
        return Ok(Reply::Nothing);
    }
    if !early && let Some(socket) = &socket {
        relay = crate::relay::connect(socket, server).await;
    }
    let route = match relay {
        Some(client) => {
            steps.mark("relay");
            Route::Relay(client)
        }
        None => {
            let token = match token.take() {
                Some(token) => token,
                None => spawn_token(&home),
            };
            let Some(token) = token.await?? else {
                return Ok(Reply::Nothing);
            };
            steps.mark("token");
            Route::Direct(token)
        }
    };
    let t3_parent = match event {
        HookEvent::SessionStart if !nested && t3_identity_applies(explicit_agent, env) => {
            delegated_parent(&home, runtime, session).await
        }
        _ => None,
    };
    steps.mark("t3");
    let remaining = BUDGET.saturating_sub(started.elapsed() + MARGIN);
    let relayed = matches!(route, Route::Relay(_));
    let client = match route {
        Route::Relay(client) => client
            .with_agent(Some(agent.clone()))
            .with_timeout(remaining),
        Route::Direct(token) => {
            let connect = CONNECT_BUDGET.min(remaining);
            Client::with_connect_timeout(server, token, Some(agent.clone()), remaining, connect)?
        }
    };
    let mut hook = Hook {
        relayed,
        event,
        runtime,
        nested,
        device: device.clone(),
        client,
        local,
        home,
        agent,
        input,
        touches,
        t3_parent,
        done_reminder: None,
        now,
    };
    // Turn boundaries for the done reminder hold whether or not the server answers.
    match event {
        HookEvent::UserPromptSubmit => hook.local.turn_started = Some(hook.now),
        // A new or resumed session: done marks left over were from a turn whose Stop never
        // ran. Compaction happens within a turn, so it keeps them and the turn's start.
        HookEvent::SessionStart if input["source"] != "compact" => {
            hook.local.turn_started = None;
            if let Err(err) = state::take_done(&hook.home, &hook.agent) {
                tracing::debug!("dropping done marks: {err:#}");
            }
        }
        _ => {}
    }
    if let Some(failure) = breaker.open_at(hook.now) {
        let reply = hook.offline(failure);
        state::save_agent(&hook.home, &hook.agent, &hook.local)?;
        return Ok(reply);
    }
    // `judged`: whether a failure says anything about the server.
    let (reply, failure, judged) = match tokio::time::timeout(remaining, hook.online(repo)).await {
        Ok(Ok(reply)) => {
            hook.local.offline_notified = false;
            (reply, None, true)
        }
        // The server answered with an error: it is reachable, so the cache is no better.
        Ok(Err(err)) if crate::client::error_code(&err).is_some() => {
            tracing::debug!("hook {}: {err:#}", event.name());
            (hook.local_stop(), None, true)
        }
        // The relay did not forward the request: nothing is known about the server.
        Ok(Err(err)) if relay_failure(&err) == Some(RelayFailure::Refused) => {
            tracing::debug!("hook {}: {err:#}", event.name());
            (hook.local_stop(), None, false)
        }
        Ok(Err(err)) => {
            tracing::debug!("hook {}: {err:#}", event.name());
            let failure = Failure::of(&err);
            // The relay's own connection failing (a daemon stopping mid-hook) says nothing
            // about the server. The relay reports the server's failures in an answer, and a
            // timeout counts as the server's.
            let relay_broke = hook.relayed
                && err
                    .chain()
                    .filter_map(|e| e.downcast_ref::<reqwest::Error>())
                    .any(|e| !e.is_timeout());
            (hook.offline(failure), Some(failure), !relay_broke)
        }
        Err(_) => {
            tracing::debug!("hook {}: no answer within {remaining:?}", event.name());
            (hook.offline(Failure::Slow), Some(Failure::Slow), true)
        }
    };
    steps.mark("server");
    let next = match failure {
        // The relay failed, not the server.
        _ if !judged => breaker.clone(),
        // This device left the server too little time to judge it.
        Some(_) if remaining < SERVER_SHARE_FAIR => breaker.clone(),
        failure => breaker.after(failure, hook.now)?,
    };
    if next != breaker {
        state::save_named(&hook.home, BREAKER, &next)?;
    }
    state::save_agent(&hook.home, &hook.agent, &hook.local)?;
    Ok(reply)
}

/// How long each step of a hook took, logged at debug level when the hook returns
/// (`BOARD_LOG=board=debug`): each step's time since the one before, and the total.
struct Steps {
    event: HookEvent,
    started: Instant,
    last: Duration,
    done: Vec<String>,
}

impl Steps {
    fn new(event: HookEvent, started: Instant) -> Self {
        Self {
            event,
            started,
            last: Duration::ZERO,
            done: Vec::new(),
        }
    }

    fn mark(&mut self, step: &str) {
        let now = self.started.elapsed();
        self.done
            .push(format!("{step} {}ms", (now - self.last).as_millis()));
        self.last = now;
    }
}

impl Drop for Steps {
    fn drop(&mut self) {
        tracing::debug!(
            "hook {} steps: {} (total {}ms)",
            self.event.name(),
            self.done.join(", "),
            self.started.elapsed().as_millis()
        );
    }
}

/// How a hook reaches the server.
enum Route {
    /// Through the device daemon's relay, with this client.
    Relay(Client),
    /// Directly, with this token.
    Direct(String),
}

/// Why the hook got no answer from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// The server did not answer within the hook's budget. It may be busy, or this device
    /// may be; board tools and the CLI, which wait longer, still work.
    Slow,
    /// The connection failed: refused, DNS, TLS, or not open within [`CONNECT_BUDGET`].
    /// The server cannot take writes now.
    Unreachable,
}

impl Failure {
    /// A request that timed out after it connected is slow; any other failure to get an
    /// answer is unreachable, a connection not opened within [`CONNECT_BUDGET`] included.
    fn of(err: &anyhow::Error) -> Self {
        match relay_failure(err) {
            Some(RelayFailure::Slow) => return Self::Slow,
            Some(RelayFailure::Unreachable) => return Self::Unreachable,
            Some(RelayFailure::Refused) | None => {}
        }
        let timed_out = err
            .chain()
            .filter_map(|e| e.downcast_ref::<reqwest::Error>())
            .any(|e| e.is_timeout() && !e.is_connect());
        if timed_out {
            Self::Slow
        } else {
            Self::Unreachable
        }
    }
}

/// The device's breaker (`state/offline.json`): while it is open, hooks use the cache
/// without trying the server. A failed connection opens it for [`OFFLINE_BACKOFF`];
/// [`SLOW_STRIKES`] slow replies in a row open it for [`SLOW_BACKOFF`]. Any answer closes it.
/// Hooks running at once may each count the same strike; the count is a rough guide.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct Breaker {
    /// Until when hooks skip the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    until: Option<Timestamp>,
    /// The breaker opened on slow replies, not on a failed connection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    slow: bool,
    /// Slow replies in a row since the server last answered in time.
    #[serde(default, skip_serializing_if = "is_zero")]
    strikes: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Breaker {
    /// Why hooks skip the server at `now`; `None` when they should try it.
    fn open_at(&self, now: Timestamp) -> Option<Failure> {
        let open = self.until.is_some_and(|until| until > now);
        open.then_some(if self.slow {
            Failure::Slow
        } else {
            Failure::Unreachable
        })
    }

    /// The breaker after a hook that tried the server and got `failure`, or an answer.
    fn after(&self, failure: Option<Failure>, now: Timestamp) -> Result<Self> {
        Ok(match failure {
            None => Self::default(),
            Some(Failure::Unreachable) => Self {
                until: Some(now + chrono::Duration::from_std(OFFLINE_BACKOFF)?),
                ..Self::default()
            },
            Some(Failure::Slow) if self.strikes + 1 >= SLOW_STRIKES => Self {
                until: Some(now + chrono::Duration::from_std(SLOW_BACKOFF)?),
                slow: true,
                strikes: 0,
            },
            Some(Failure::Slow) => Self {
                strikes: self.strikes + 1,
                ..Self::default()
            },
        })
    }
}

/// How long a session's cached decisions outlive its last SessionStart.
const SESSION_CACHE_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);
/// How long the start mark of a shell call that never reached PostToolUse is kept.
const TOOL_START_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
/// How long an agent's hook state and cached snapshot outlive its last write.
const AGENT_FILE_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);
/// How long a done mark no `Stop` hook read (an interrupted turn, an ended session) is kept.
const DONE_MARK_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Whether this session runs inside another agent session, which matters only when the
/// T3 thread id would name it. Decided from the process table at `SessionStart` (or the
/// first hook of a session without one) and cached for the session's later hooks;
/// `SessionEnd` drops the cache.
async fn nested_session(
    home: &Home,
    event: HookEvent,
    runtime: Runtime,
    session: Option<&str>,
    explicit_agent: Option<&str>,
) -> Result<bool> {
    if !t3_identity_applies(explicit_agent, |name| std::env::var(name).ok()) {
        return Ok(false);
    }
    if event != HookEvent::SessionStart
        && let Some(session) = session
        && let Some(cached) = state::load_session(home, runtime, session)
    {
        if event == HookEvent::SessionEnd {
            state::remove_session(home, runtime, session)?;
        }
        return Ok(cached.nested);
    }
    let prune = (event == HookEvent::SessionStart).then(|| home.clone());
    let nested = tokio::task::spawn_blocking(move || {
        // Before this session's file is written, so it is never the one removed.
        if let Some(home) = prune
            && let Err(err) = state::prune_sessions(&home, SESSION_CACHE_AGE)
        {
            tracing::debug!("pruning cached sessions: {err:#}");
        }
        nested_here()
    })
    .await?;
    // Nothing to remember at the end of a session.
    if let Some(session) = session.filter(|_| event != HookEvent::SessionEnd) {
        let t3_parent = state::load_session(home, runtime, session).and_then(|s| s.t3_parent);
        state::save_session(
            home,
            runtime,
            session,
            &state::SessionLocal { nested, t3_parent },
        )?;
    }
    Ok(nested)
}

/// The directory a hook's tool call runs in: the payload's `cwd`, else this process's.
fn input_cwd(input: &Value) -> PathBuf {
    input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

/// The T3 thread that delegated this session's T3 thread, for a thread T3 started for a
/// parent's `delegate_task`: from the session's cache, else asked of T3 (the `t3` this
/// thread's T3 names, else the configured one) within [`PARENT_BUDGET`] and cached once
/// known. `None` for other threads, or when T3 does not answer in time; a later
/// `SessionStart` asks again.
async fn delegated_parent(home: &Home, runtime: Runtime, session: Option<&str>) -> Option<String> {
    let thread = std::env::var(T3_THREAD_ENV).ok()?;
    let thread = thread.trim();
    if !thread.starts_with(DELEGATED_THREAD) {
        return None;
    }
    let cached = session.and_then(|s| state::load_session(home, runtime, s));
    if let Some(parent) = cached.as_ref().and_then(|c| c.t3_parent.clone()) {
        return Some(parent);
    }
    let bin = std::env::var_os(T3_CLI_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| Config::load(home).ok()?.t3_bin);
    let t3 = T3Cli::locate(bin.as_deref());
    let parent = match tokio::time::timeout(PARENT_BUDGET, t3.thread_parent(thread)).await {
        Ok(Ok(parent)) => parent?,
        Ok(Err(err)) => {
            tracing::debug!("parent of thread {thread}: {err:#}");
            return None;
        }
        Err(_) => {
            tracing::debug!("parent of thread {thread}: over budget");
            return None;
        }
    };
    if let (Some(session), Some(mut local)) = (session, cached) {
        local.t3_parent = Some(parent.clone());
        if let Err(err) = state::save_session(home, runtime, session, &local) {
            tracing::debug!("caching the parent thread: {err:#}");
        }
    }
    Some(parent)
}

impl Hook<'_> {
    fn cwd(&self) -> PathBuf {
        input_cwd(self.input)
    }

    /// `repo`: the work tree, when the local steps detected it.
    async fn online(&mut self, repo: Option<RepoInfo>) -> Result<Reply> {
        match self.event {
            HookEvent::SessionStart => self.session_start(repo).await,
            HookEvent::UserPromptSubmit => self.user_prompt_submit().await,
            // Handled before login (`handle`).
            HookEvent::PreToolUse => Ok(Reply::Nothing),
            // A failed shell command may still have changed files.
            HookEvent::PostToolUse | HookEvent::PostToolUseFailure => self.post_tool_use().await,
            HookEvent::Stop => self.stop().await,
            HookEvent::StopFailure => self.stop_failure().await,
            HookEvent::SessionEnd => {
                self.set_state(AgentState::Ended, None).await?;
                Ok(Reply::Nothing)
            }
            HookEvent::PreCompact => self.pre_compact().await,
        }
    }

    /// The server is unreachable or too slow: fall back to the cache.
    fn offline(&mut self, failure: Failure) -> Reply {
        match self.event {
            HookEvent::SessionStart => self.cached_snapshot(failure),
            HookEvent::UserPromptSubmit if !self.local.offline_notified => {
                self.cached_snapshot(failure)
            }
            HookEvent::PreCompact => self.compact_instructions(None),
            _ => self.local_stop(),
        }
    }

    /// At `Stop` without the server's answer: the done reminder, which needs no server.
    fn local_stop(&mut self) -> Reply {
        if self.event != HookEvent::Stop {
            return Reply::Nothing;
        }
        if self.done_reminder.is_none() {
            self.prepare_done_reminder();
        }
        self.done_reminder
            .take()
            .map_or(Reply::Nothing, Reply::Block)
    }

    /// Sets `done_reminder`; a state file that cannot be read costs only the reminder, never
    /// the handoff check or the offline backoff.
    fn prepare_done_reminder(&mut self) {
        match self.take_done_reminder() {
            Ok(text) => self.done_reminder = text,
            Err(err) => tracing::debug!("reading done marks: {err:#}"),
        }
    }

    /// Reads the tasks this agent marked done since the last `Stop` and returns the reminder
    /// to file what remains of them, once per task.
    ///
    /// The signal is the mark `ops::task_done` writes in this device's state after the server
    /// accepted the done, rather than server events since the turn began. Every done this
    /// agent makes goes through it (the `task_done` tool and `board done`), on this device,
    /// under the same agent id the hooks resolve, and it needs no server call, so it holds
    /// within the hook's budget, offline, and on a busy board whose events would take pages to
    /// read. Each `Stop` takes the marks, so the next one sees only later dones; marks older
    /// than the turn (`UserPromptSubmit`) are from a turn whose `Stop` never ran.
    fn take_done_reminder(&mut self) -> Result<Option<String>> {
        let marks = state::take_done(&self.home, &self.agent)?;
        if self.input["stop_hook_active"].as_bool().unwrap_or(false) {
            return Ok(None);
        }
        Ok(done_reminder(
            marks,
            self.local.turn_started,
            &mut self.local.done_reminded,
        ))
    }

    fn cached_snapshot(&mut self, failure: Failure) -> Reply {
        let Some(snap) = state::load_cached_snapshot(&self.home, &self.agent) else {
            return Reply::Nothing;
        };
        if snap.text.is_empty() {
            return Reply::Nothing;
        }
        self.local.offline_notified = true;
        let (ago, seq) = (render::ago(snap.at), snap.seq);
        let head = match failure {
            Failure::Slow => format!(
                "board server slow (no answer within the hook's {:.1} s): cached snapshot from {ago} (seq {seq}); board tools and the CLI wait longer and still work.",
                BUDGET.as_secs_f32()
            ),
            Failure::Unreachable => format!(
                "board server unreachable: cached snapshot from {ago} (seq {seq}); claims and updates fail until it is back."
            ),
        };
        Reply::Context(format!("{head}\n{}", snap.text))
    }

    async fn set_state(&self, state: AgentState, paused_until: Option<Timestamp>) -> Result<()> {
        self.client
            .set_state(
                &self.agent,
                &AgentStateUpdate {
                    state,
                    paused_until,
                    said: None,
                },
            )
            .await?;
        Ok(())
    }

    async fn fetch(&self, kind: SnapshotKind) -> Result<Snapshot> {
        let since = if kind == SnapshotKind::Full {
            None
        } else {
            self.local.seq
        };
        self.client
            .snapshot(&SnapshotQuery {
                agent: self.agent.clone(),
                kind,
                since,
                format: SnapshotFormat::Lines,
            })
            .await
    }

    /// Marks the agent running and fetches a snapshot of `kind`. Directly, one request after
    /// the other, so both use one connection: two at once would open two, each with its own
    /// TLS handshake (the client speaks HTTP/1.1). Through the relay, at once: its
    /// connections to the server are already open.
    async fn beat_and_fetch(&self, kind: SnapshotKind) -> Result<Snapshot> {
        let beat = self.set_state(AgentState::Running, None);
        if self.relayed {
            let (beat, snap) = tokio::join!(beat, self.fetch(kind));
            beat?;
            snap
        } else {
            beat.await?;
            self.fetch(kind).await
        }
    }

    /// Records the snapshot's sequence (and caches a full one) and returns its text.
    fn keep(&mut self, snap: Snapshot) -> Result<String> {
        self.local.seq = Some(snap.seq);
        if snap.kind == SnapshotKind::Full {
            state::save_cached_snapshot(&self.home, &self.agent, &snap)?;
        }
        Ok(snap.text)
    }

    async fn snapshot(&mut self, kind: SnapshotKind) -> Result<String> {
        let snap = self.fetch(kind).await?;
        self.keep(snap)
    }

    async fn session_start(&mut self, repo: Option<RepoInfo>) -> Result<Reply> {
        let thread = std::env::var(T3_THREAD_ENV).ok().filter(|_| !self.nested);
        let repo = match repo {
            Some(repo) => repo,
            None => detect_repo(self.cwd()).await?,
        };
        let mut native = std::collections::BTreeMap::new();
        if let Some(session) = self.input["session_id"].as_str() {
            let key = match self.runtime {
                Runtime::Claude => "claude_session",
                Runtime::Codex => "codex_session",
            };
            native.insert(key.to_owned(), session.to_owned());
        }
        if let Some(thread) = thread {
            native.insert("t3_thread".to_owned(), thread);
        }
        // The server keeps a delegated child's edits and its parent's out of each other's
        // overlaps and alerts.
        if let Some(parent) = self.t3_parent.clone() {
            native.insert("t3_parent".to_owned(), parent);
        }
        self.client
            .register(&board_proto::RegisterAgent {
                id: self.agent.clone(),
                runtime: self.runtime,
                device: self.device.clone(),
                role: board_proto::AgentRole::Worker,
                title: None,
                repo: repo.key.clone(),
                branch: repo.branch.clone(),
                cwd: Some(repo.cwd.display().to_string()),
                native_ids: native,
                forked_from: None,
            })
            .await?;
        self.local.reminded_for = None;
        let text = self.snapshot(SnapshotKind::Full).await?;
        if !text.is_empty() {
            return Ok(Reply::Context(text));
        }
        // No board for this agent: hint at a board covering the repo, if any.
        let Some(key) = repo.key else {
            return Ok(Reply::Nothing);
        };
        let boards = self.client.boards().await?;
        // The repo's active board first; closed boards are not worth a hint.
        let mut covering: Vec<&board_proto::Board> = boards
            .iter()
            .filter(|b| b.repos.contains(&key) && b.status != board_proto::BoardStatus::Closed)
            .collect();
        covering.sort_by_key(|b| b.status != board_proto::BoardStatus::Active);
        Ok(match covering.first() {
            Some(board) => Reply::Context(format!(
                "board: this repo has board {} ({}); board_join {} to see and claim its tasks.",
                board.slug,
                serde_json::to_value(board.status)?
                    .as_str()
                    .unwrap_or_default(),
                board.slug
            )),
            None => Reply::Nothing,
        })
    }

    async fn user_prompt_submit(&mut self) -> Result<Reply> {
        // After an outage, catch up with a full snapshot instead of a delta.
        let kind = if self.local.offline_notified {
            SnapshotKind::Full
        } else {
            SnapshotKind::Delta
        };
        let snap = self.beat_and_fetch(kind).await?;
        let text = self.keep(snap)?;
        Ok(if text.is_empty() {
            Reply::Nothing
        } else {
            Reply::Context(text)
        })
    }

    async fn post_tool_use(&mut self) -> Result<Reply> {
        let mut lines = Vec::new();
        for touches in &self.touches {
            let mut result = self.client.touches(touches).await?;
            // Every call reports every overlap; each is shown once per peer change.
            result
                .overlaps
                .retain(|o| self.local.first_sight(o, self.now));
            lines.extend(render::overlaps(&result));
        }
        if heartbeat_due(self.local.last_heartbeat, self.now) {
            self.local.last_heartbeat = Some(self.now);
            let snap = self.beat_and_fetch(SnapshotKind::Alerts).await?;
            let alerts = self.keep(snap)?;
            if !alerts.is_empty() {
                lines.push(alerts);
            }
        }
        Ok(if lines.is_empty() {
            Reply::Nothing
        } else {
            Reply::Context(lines.join("\n"))
        })
    }

    /// Marks the agent idle with the turn's last message, which the server keeps for the
    /// user while the agent coordinates a board. A turn the reminders continue has not
    /// ended, so its message is not sent; the next `Stop` sends the final one.
    ///
    /// Both reminders go in one block: the turn continues once, and the `Stop` that follows
    /// has `stop_hook_active` and stays quiet, so a second block would never be seen.
    async fn stop(&mut self) -> Result<Reply> {
        // Before any server call, so it survives a server that fails (`local_stop`).
        self.prepare_done_reminder();
        // A Stop that follows our own block ends the turn that answered a reminder; the
        // message worth keeping was the one the reminder interrupted.
        let continued = self.input["stop_hook_active"].as_bool().unwrap_or(false);
        let said = if continued {
            None
        } else {
            self.input["last_assistant_message"]
                .as_str()
                .and_then(board_proto::turn_ending)
        };
        // The state first: it matters most within the hook's budget.
        self.client
            .set_state(
                &self.agent,
                &AgentStateUpdate {
                    state: AgentState::Idle,
                    paused_until: None,
                    said,
                },
            )
            .await?;
        if continued {
            return Ok(Reply::Nothing);
        }
        let handoff = self.handoff_reminder().await?;
        let text: Vec<String> = self
            .done_reminder
            .take()
            .into_iter()
            .chain(handoff)
            .collect();
        Ok(if text.is_empty() {
            Reply::Nothing
        } else {
            Reply::Block(text.join("\n"))
        })
    }

    /// The handoff reminder: when a task is in `doing` and the handoff is missing or stale.
    async fn handoff_reminder(&mut self) -> Result<Option<String>> {
        let doing = self
            .client
            .tasks(&TaskQuery {
                mine: Some(true),
                status: Some(TaskStatus::Doing),
                ..TaskQuery::default()
            })
            .await?;
        let Some(task) = doing.first() else {
            return Ok(None);
        };
        let handoff = self.client.handoff(&self.agent).await?;
        let turn = self.local.turn_started;
        // The server dates the agent's last board change or file touch on the handoff's own
        // clock, so a turn that only read or replied leaves the handoff current. A server
        // without that date falls back to: a handoff older than the turn is stale.
        let (stale_since, why) = match &handoff {
            None => (Some(turn.unwrap_or(self.now)), "you have no handoff"),
            Some(h) => match h.last_change_at {
                Some(at) => (
                    (at > h.updated_at).then_some(at),
                    "your handoff predates your last board change or file edit",
                ),
                None => (
                    turn.filter(|&t| h.updated_at < t),
                    "your handoff is older than this turn",
                ),
            },
        };
        let Some(since) = stale_since else {
            return Ok(None);
        };
        if self.local.reminded_for == Some(since) {
            return Ok(None);
        }
        self.local.reminded_for = Some(since);
        Ok(Some(format!(
            "board: {} is in doing and {why}. Call handoff_set (now, next, blockers) so the next session can resume, then stop.",
            task.id
        )))
    }

    async fn stop_failure(&mut self) -> Result<Reply> {
        let texts = [
            self.input["error"].as_str(),
            self.input["error_details"].as_str(),
            self.input["last_assistant_message"].as_str(),
        ];
        if !is_usage_limit(&texts) {
            return Ok(Reply::Nothing);
        }
        let until = texts
            .iter()
            .flatten()
            .find_map(|t| parse_reset(t, self.now.with_timezone(&Local)));
        self.set_state(AgentState::Paused, until).await?;
        Ok(Reply::Nothing)
    }

    async fn pre_compact(&mut self) -> Result<Reply> {
        if self.runtime == Runtime::Codex {
            // Codex documents no way to add compaction instructions.
            return Ok(Reply::Nothing);
        }
        let doing = self
            .client
            .tasks(&TaskQuery {
                mine: Some(true),
                status: Some(TaskStatus::Doing),
                ..TaskQuery::default()
            })
            .await?;
        Ok(self.compact_instructions(doing.first().map(|t| t.id.to_string())))
    }

    fn compact_instructions(&self, task: Option<String>) -> Reply {
        if self.runtime == Runtime::Codex {
            return Reply::Nothing;
        }
        let task = match task {
            Some(id) => format!(" and the current board task id {id} (status doing)"),
            None => String::new(),
        };
        Reply::Plain(format!(
            "Keep in the summary, verbatim: the board agent id {}{task}. After compaction, call board_snapshot before continuing.",
            self.agent
        ))
    }
}

/// How many reminded tasks the hook state keeps; a task done again after it fell out is
/// reminded about again.
const DONE_REMINDED_KEPT: usize = 100;

/// The reminder for tasks marked done this turn (marks at or after `turn_started`, any when
/// the turn's start is unknown) not reminded about before, which it records in `reminded`.
fn done_reminder(
    marks: Vec<DoneMark>,
    turn_started: Option<Timestamp>,
    reminded: &mut Vec<TaskId>,
) -> Option<String> {
    let mut ids: Vec<TaskId> = Vec::new();
    for mark in marks {
        if turn_started.is_none_or(|t| mark.at >= t)
            && !reminded.contains(&mark.task)
            && !ids.contains(&mark.task)
        {
            ids.push(mark.task);
        }
    }
    if ids.is_empty() {
        return None;
    }
    reminded.extend(ids.iter().cloned());
    let excess = reminded.len().saturating_sub(DONE_REMINDED_KEPT);
    reminded.drain(..excess);
    let ids: Vec<&str> = ids.iter().map(TaskId::as_str).collect();
    Some(format!(
        "board: you marked {} done this turn. Before you stop, file anything still to do: sub-tasks for its scope, Proposal: tasks for found work, a blocked task with an ask for the user's part; name them in your reply.",
        ids.join(", ")
    ))
}

/// Looks up the device token off the async threads: the Keychain lookup runs `security`.
fn spawn_token(home: &Home) -> tokio::task::JoinHandle<Result<Option<String>>> {
    let home = home.clone();
    tokio::task::spawn_blocking(move || load_token(&home))
}

/// What a `PostToolUse` hook reports for `agent`, in batches the server takes: the files an
/// edit tool names, or those a shell call that started at `shell_start` changed in the work
/// tree. Empty outside a repo with an `origin`.
async fn local_touches(
    agent: &AgentId,
    repo: &RepoInfo,
    edited: Vec<PathBuf>,
    shell_start: Option<&ToolStart>,
) -> Vec<Touches> {
    let Some(key) = repo.key.clone() else {
        return Vec::new();
    };
    // An edit tool names its files; a shell call's are found by scanning the work tree, so
    // another agent in the same checkout may have written them. Edit tools never have a
    // start mark, and a shell call names no files.
    let (paths, scanned) = match (shell_start, repo.root.as_deref()) {
        (Some(start), Some(root)) => {
            // Bounded on its own, so a slow work tree never eats the server's budget.
            let found =
                match tokio::time::timeout(SHELL_SCAN_BUDGET, shell_changes(root, start)).await {
                    Ok(Ok(changed)) => changed,
                    Ok(Err(err)) => {
                        tracing::debug!("files changed by the shell call: {err:#}");
                        Vec::new()
                    }
                    Err(_) => {
                        tracing::debug!("files changed by the shell call: over budget");
                        Vec::new()
                    }
                };
            let found = found.into_iter().map(|(p, at)| (p, Some(at))).collect();
            (found, true)
        }
        (Some(_), None) => (Vec::new(), true),
        (None, _) => {
            let named = edited
                .into_iter()
                .map(|p| {
                    let at = modified_at(&repo.cwd.join(&p));
                    (p, at)
                })
                .collect();
            (named, false)
        }
    };
    // A path the server would refuse would fail the whole request.
    let rel: Vec<(String, Option<Timestamp>)> = paths
        .iter()
        .filter_map(|(p, at): &(PathBuf, Option<Timestamp>)| Some((repo.relative(p)?, *at)))
        .filter(|(p, _)| p.chars().count() <= PATH_CHARS_MAX && !p.chars().any(char::is_control))
        .collect();
    rel.chunks(TOUCH_PATHS_MAX)
        .map(|chunk| Touches {
            agent: agent.clone(),
            repo: key.clone(),
            paths: chunk.iter().map(|(p, _)| p.clone()).collect(),
            scanned,
            modified: chunk
                .iter()
                .filter_map(|(p, at)| Some((p.clone(), (*at)?)))
                .collect(),
        })
        .collect()
}

/// Runs `git` off the async threads, so the hook's deadline can still fire.
async fn detect_repo(cwd: PathBuf) -> Result<RepoInfo> {
    Ok(tokio::task::spawn_blocking(move || RepoInfo::detect(&cwd)).await?)
}

/// True when the heartbeat (and alert check) should run now.
pub fn heartbeat_due(last: Option<Timestamp>, now: Timestamp) -> bool {
    last.is_none_or(|last| {
        (now - last)
            .to_std()
            .map_or(true, |since| since >= HEARTBEAT_EVERY)
    })
}

/// The shell tool's name on both runtimes.
const SHELL_TOOL: &str = "Bash";
/// Time for listing the files a shell call changed; past it the call records none.
const SHELL_SCAN_BUDGET: Duration = Duration::from_millis(500);

/// Time for reading HEAD before a shell call; past it the start mark carries none.
const HEAD_BUDGET: Duration = Duration::from_millis(300);
/// The file that marks a merge stopped in the work tree, naming the commits being merged.
const MERGE_HEAD: &str = "MERGE_HEAD";
/// Files that mark a cherry-pick, revert or rebase stopped in the work tree, each naming
/// the commit being applied.
const APPLYING: [&str; 3] = ["CHERRY_PICK_HEAD", "REVERT_HEAD", "REBASE_HEAD"];

/// Where a work tree's git state lives and what it has checked out.
#[derive(Debug, Clone, PartialEq)]
pub struct GitHead {
    /// The work tree's own git directory (per worktree).
    pub git_dir: PathBuf,
    /// The commit at `HEAD`; `None` before the first commit.
    pub head: Option<String>,
}

/// A read-only `git` in the work tree containing `dir`: no index refresh (so no
/// `index.lock` to collide with), no input, no error output, stopped when dropped.
fn git(dir: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}

/// Runs `git` in `dir` with `args` and returns its output; an error when it fails.
async fn git_output(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git(dir).args(args).output().await?;
    anyhow::ensure!(
        out.status.success(),
        "git {} failed: {}",
        args[0],
        out.status
    );
    Ok(out.stdout)
}

/// NUL-separated paths from `git -z` output, under the work tree `root`.
fn nul_paths<'a>(root: &'a Path, out: &'a [u8]) -> impl Iterator<Item = PathBuf> + 'a {
    out.split(|b| *b == 0)
        .filter_map(|p| std::str::from_utf8(p).ok())
        .filter(|p| !p.is_empty())
        .map(|p| root.join(p))
}

/// The git directory and `HEAD` commit of the work tree containing `dir`; an error outside
/// one or when git fails. One `git rev-parse`, a few milliseconds.
pub async fn git_head(dir: &Path) -> Result<GitHead> {
    let out = git(dir)
        .args(["rev-parse", "--absolute-git-dir", "--verify", "-q", "HEAD"])
        .output()
        .await?;
    // Before the first commit it prints the directory and exits 1; outside a repo, nothing.
    let text = String::from_utf8(out.stdout)?;
    let mut lines = text.lines();
    let git_dir = lines
        .next()
        .filter(|l| !l.is_empty())
        .with_context(|| format!("git rev-parse failed: {}", out.status))?;
    let head = lines.next().filter(|l| !l.is_empty()).map(str::to_owned);
    Ok(GitHead {
        git_dir: PathBuf::from(git_dir),
        head,
    })
}

/// What a shell call that started at `start` changed in the work tree `root`: the files
/// [`changed_since`] finds, less those [`brought_in`] by git during the call.
pub async fn shell_changes(root: &Path, start: &ToolStart) -> Result<Vec<(PathBuf, Timestamp)>> {
    let changed = changed_since(root, start.at).await?;
    if changed.is_empty() {
        return Ok(changed);
    }
    let brought = brought_in(root, start).await?;
    Ok(changed
        .into_iter()
        .filter(|(path, _)| !brought.contains(path))
        .collect())
}

/// Files under `root` that git itself wrote during a shell call that started at `start`,
/// so they are not the call's edits:
/// - when `HEAD` moved (a checkout, switch, merge, rebase, pull or reset), the files that
///   differ between `HEAD` before and after, unless the move is one commit the call made
///   in place of the old `HEAD` (`git commit`, `--amend`), whose files are the agent's own;
/// - for a merge that stopped during the call (a conflicted merge stages what it brings in
///   without moving `HEAD`), what the merged commits changed since the merge base;
/// - for a cherry-pick, revert or rebase that stopped during the call, what the commit it
///   applies changes.
///
/// A stopped operation counts only when its marker is newer than the start mark: one that
/// stopped before is the agent's to finish. An error when git cannot say.
pub async fn brought_in(root: &Path, start: &ToolStart) -> Result<HashSet<PathBuf>> {
    let now = git_head(root).await?;
    let mut ranges: Vec<Vec<String>> = Vec::new();
    if let (Some(before), Some(after)) = (&start.head, &now.head)
        && start.git_dir.as_deref() == Some(now.git_dir.as_path())
        && before != after
        && !committed_only(&now.git_dir, before, after)
    {
        ranges.push(vec![before.clone(), after.clone()]);
    }
    let started_during = |name: &str| {
        let path = now.git_dir.join(name);
        let commits = modified_at(&path)
            .filter(|at| *at >= start.at)
            .and_then(|_| std::fs::read_to_string(&path).ok())
            .unwrap_or_default();
        commits
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    if now.head.is_some() {
        for commit in started_during(MERGE_HEAD) {
            ranges.push(vec![format!("HEAD...{commit}")]);
        }
    }
    for name in APPLYING {
        for commit in started_during(name) {
            ranges.push(vec![format!("{commit}^"), commit]);
        }
    }
    let mut out = HashSet::new();
    for range in ranges {
        let mut args = vec!["diff", "--name-only", "-z", "--no-renames"];
        args.extend(range.iter().map(String::as_str));
        args.push("--");
        out.extend(nul_paths(root, &git_output(root, &args).await?));
    }
    Ok(out)
}

/// Did `HEAD` go from `before` to `after` by commits alone (`git commit`, `--amend`), so the
/// files between them are the agent's own? Read from the newest entries of `HEAD`'s reflog
/// in `git_dir`; no reflog or no unbroken run of commits back to `before` means no.
fn committed_only(git_dir: &Path, before: &str, after: &str) -> bool {
    /// The reflog's tail is enough: a call makes a handful of entries.
    const TAIL: u64 = 64 * 1024;
    let Ok(mut file) = std::fs::File::open(git_dir.join("logs/HEAD")) else {
        return false;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut text = String::new();
    if file
        .seek(SeekFrom::Start(len.saturating_sub(TAIL)))
        .is_err()
        || file.read_to_string(&mut text).is_err()
    {
        return false;
    }
    // Each entry is `<old> <new> <who> <when>\t<message>`.
    let mut want = after;
    for entry in text.lines().rev() {
        let Some((ids, message)) = entry.split_once('\t') else {
            return false;
        };
        let mut ids = ids.split(' ');
        let (Some(old), Some(new)) = (ids.next(), ids.next()) else {
            return false;
        };
        let by_commit = ["commit:", "commit (amend):", "commit (initial):"]
            .iter()
            .any(|kind| message.starts_with(kind));
        if new != want || !by_commit {
            return false;
        }
        if old == before {
            return true;
        }
        want = old;
    }
    false
}

/// Files under the work tree `root` that `git status` lists as changed or untracked and
/// that were modified at or after `since`: what a shell call that started then wrote.
/// Not seen: files the call deleted, changed and committed, or wrote keeping an older
/// modification time (`mv`, `cp -p`, archives). Seen as well: files another writer in the
/// same work tree changed during the call. Dropping the future stops `git`.
pub async fn changed_since(root: &Path, since: Timestamp) -> Result<Vec<(PathBuf, Timestamp)>> {
    let out = git_output(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    )
    .await?;
    // Whole seconds: some file systems keep modification times to the second.
    let since = since.trunc_subsecs(0);
    Ok(out
        .split(|b| *b == 0)
        // Each entry is `XY <path>`; paths are relative to the work tree.
        .filter_map(|entry| std::str::from_utf8(entry.get(3..)?).ok())
        .filter(|path| !path.is_empty())
        .map(|path| root.join(path))
        .filter_map(|path| {
            let modified = modified_at(&path)?;
            (modified >= since).then_some((path, modified))
        })
        .collect())
}

/// A file's modification time, without following a symlink; `None` when it is gone.
fn modified_at(path: &Path) -> Option<Timestamp> {
    let modified = std::fs::symlink_metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified))
}

/// Files a tool call edited: Claude Code's `Edit`, `Write`, `MultiEdit` and
/// `NotebookEdit`, and Codex's `apply_patch` (paths parsed from the patch).
pub fn edited_paths(tool: &str, input: &Value) -> Vec<PathBuf> {
    match tool {
        "Edit" | "Write" | "MultiEdit" => input["file_path"]
            .as_str()
            .map(PathBuf::from)
            .into_iter()
            .collect(),
        "NotebookEdit" => input["notebook_path"]
            .as_str()
            .map(PathBuf::from)
            .into_iter()
            .collect(),
        "apply_patch" => {
            let patch = match &input["command"] {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => input["patch"].as_str().unwrap_or_default().to_owned(),
            };
            patch_paths(&patch)
        }
        _ => Vec::new(),
    }
}

fn patch_paths(patch: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for line in patch.lines() {
        let line = line.trim_end();
        let path = [
            "*** Add File: ",
            "*** Update File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix));
        if let Some(path) = path.map(str::trim).filter(|p| !p.is_empty()) {
            let path = PathBuf::from(path);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

/// Does the failure text describe a usage or rate limit?
pub fn is_usage_limit(texts: &[Option<&str>]) -> bool {
    texts.iter().flatten().any(|t| {
        let t = t.to_ascii_lowercase();
        t == "rate_limit"
            || t.contains("usage limit")
            || t.contains("hit your limit")
            || t.contains("limit reached")
            || t.contains("rate limit")
    })
}

/// Finds a reset time in a usage-limit message: a Unix time after `|`
/// (`Claude AI usage limit reached|1730000000`), an RFC 3339 time, or a local clock time
/// after "resets"/"reset at"/"try again at" (`resets 3pm`, `resets at 5:30 PM`).
pub fn parse_reset(text: &str, now: DateTime<Local>) -> Option<Timestamp> {
    if let Some((_, tail)) = text.rsplit_once('|') {
        let digits: String = tail
            .trim()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Some(t) = digits
            .parse::<i64>()
            .ok()
            .and_then(|s| Utc.timestamp_opt(s, 0).single())
        {
            return Some(t);
        }
    }
    for word in text.split_whitespace() {
        let word = word.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | '.' | '"' | '\''));
        if let Ok(t) = DateTime::parse_from_rfc3339(word) {
            return Some(t.with_timezone(&Utc));
        }
    }
    let lower = text.to_ascii_lowercase();
    let after = ["resets at ", "reset at ", "resets ", "try again at "]
        .iter()
        .find_map(|key| lower.find(key).map(|i| &lower[i + key.len()..]))?;
    let clock = parse_clock(after)?;
    let today = now.date_naive().and_time(clock);
    let mut at = Local.from_local_datetime(&today).earliest()?;
    if at <= now {
        at = Local
            .from_local_datetime(&(today + chrono::Duration::days(1)))
            .earliest()?;
    }
    Some(at.with_timezone(&Utc))
}

/// `3pm`, `3 pm`, `3:30pm`, `15:30`, `5:00 PM` at the start of `text`.
fn parse_clock(text: &str) -> Option<NaiveTime> {
    let text = text.trim_start();
    let digits_end = text
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(text.len());
    let (clock, rest) = text.split_at(digits_end);
    if clock.is_empty() {
        return None;
    }
    let (h, m) = match clock.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.get(..2)?.parse::<u32>().ok()?),
        None => (clock.parse::<u32>().ok()?, 0),
    };
    let rest = rest.trim_start();
    let hour = if rest.starts_with("pm") || rest.starts_with("p.m") {
        if h == 12 { 12 } else { h + 12 }
    } else if rest.starts_with("am") || rest.starts_with("a.m") {
        if h == 12 { 0 } else { h }
    } else if clock.contains(':') {
        h
    } else {
        return None;
    };
    NaiveTime::from_hms_opt(hour, m, 0)
}

/// Used by tests and `install`: the path of this binary.
pub fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| Path::new("board").to_path_buf())
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn the_relays_reports_count_as_the_servers_own_failures() {
        let err = |f: RelayFailure| anyhow::Error::from(f).context("board server unreachable");
        assert_eq!(Failure::of(&err(RelayFailure::Slow)), Failure::Slow);
        assert_eq!(
            Failure::of(&err(RelayFailure::Unreachable)),
            Failure::Unreachable
        );
        assert_eq!(
            relay_failure(&err(RelayFailure::Refused)),
            Some(RelayFailure::Refused)
        );
    }

    #[test]
    fn edit_tools_yield_paths() {
        let p = |tool: &str, input: Value| edited_paths(tool, &input);
        assert_eq!(
            p("Edit", json!({"file_path": "/r/a.rs"})),
            [PathBuf::from("/r/a.rs")]
        );
        assert_eq!(
            p("NotebookEdit", json!({"notebook_path": "/r/n.ipynb"})),
            [PathBuf::from("/r/n.ipynb")]
        );
        assert!(p("Read", json!({"file_path": "/r/a.rs"})).is_empty());
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: b.md\n+hi\n*** Update File: c.rs\n*** Move to: d.rs\n*** End Patch\n";
        assert_eq!(
            p("apply_patch", json!({"command": patch})),
            ["src/a.rs", "b.md", "c.rs", "d.rs"].map(PathBuf::from)
        );
        assert_eq!(
            p("apply_patch", json!({"command": ["apply_patch", patch]})).len(),
            4
        );
    }

    #[test]
    fn heartbeat_runs_once_a_minute() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 6, 12, 0, 0).unwrap();
        assert!(heartbeat_due(None, t0));
        assert!(!heartbeat_due(Some(t0), t0 + chrono::Duration::seconds(59)));
        assert!(heartbeat_due(Some(t0), t0 + chrono::Duration::seconds(60)));
        // A clock that went backwards does not suppress heartbeats forever.
        assert!(heartbeat_due(Some(t0), t0 - chrono::Duration::seconds(5)));
    }

    #[test]
    fn usage_limits_are_recognized() {
        assert!(is_usage_limit(&[Some("rate_limit"), None]));
        assert!(is_usage_limit(&[
            None,
            Some("Claude AI usage limit reached|1762444800")
        ]));
        assert!(!is_usage_limit(&[
            Some("server_error"),
            Some("500 Internal Server Error")
        ]));
    }

    #[test]
    fn reset_times_parse() {
        let now = Local.with_ymd_and_hms(2026, 11, 6, 14, 0, 0).unwrap();
        assert_eq!(
            parse_reset("Claude AI usage limit reached|1762444800", now),
            Utc.timestamp_opt(1_762_444_800, 0).single()
        );
        assert_eq!(
            parse_reset("limit reached; resets at 2026-11-06T20:00:00Z.", now),
            Some(Utc.with_ymd_and_hms(2026, 11, 6, 20, 0, 0).unwrap())
        );
        let local = |h, m, day| {
            Local
                .with_ymd_and_hms(2026, 11, day, h, m, 0)
                .unwrap()
                .with_timezone(&Utc)
        };
        assert_eq!(
            parse_reset("You've hit your limit · resets 3pm (Europe/London)", now),
            Some(local(15, 0, 6))
        );
        assert_eq!(
            parse_reset("5-hour limit reached ∙ resets 5:30 PM", now),
            Some(local(17, 30, 6))
        );
        // Earlier than now: tomorrow.
        assert_eq!(parse_reset("resets 9am", now), Some(local(9, 0, 7)));
        assert_eq!(parse_reset("usage limit reached", now), None);
        assert_eq!(parse_reset("resets soon", now), None);
    }

    #[test]
    fn done_reminders_cover_this_turns_tasks_once_each() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 7, 12, 0, 0).unwrap();
        let mark = |task: &str, mins: i64| DoneMark {
            task: task.parse().unwrap(),
            at: t0 + chrono::Duration::minutes(mins),
        };
        let mut reminded = Vec::new();
        assert_eq!(done_reminder(vec![], Some(t0), &mut reminded), None);
        let text = done_reminder(
            vec![
                mark("factory-11", -1),
                mark("factory-12", 1),
                mark("factory-13", 2),
            ],
            Some(t0),
            &mut reminded,
        )
        .unwrap();
        assert!(
            text.starts_with("board: you marked factory-12, factory-13 done this turn."),
            "{text}"
        );
        assert_eq!(reminded.len(), 2, "the earlier turn's task is not recorded");
        assert_eq!(
            done_reminder(vec![mark("factory-12", 5)], Some(t0), &mut reminded),
            None
        );
        // Unknown turn start (no prompt since the session started): any mark counts.
        assert!(done_reminder(vec![mark("factory-11", -1)], None, &mut reminded).is_some());
        // Bounded: the oldest fall out.
        for n in 0..DONE_REMINDED_KEPT {
            done_reminder(
                vec![mark(&format!("other-{}", n + 1), 1)],
                Some(t0),
                &mut reminded,
            );
        }
        assert_eq!(reminded.len(), DONE_REMINDED_KEPT);
        assert!(!reminded.iter().any(|id| id.as_str() == "factory-12"));
    }

    #[test]
    fn replies_render_per_runtime() {
        let ctx = render_reply(
            HookEvent::UserPromptSubmit,
            Runtime::Claude,
            Reply::Context("x".into()),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&ctx).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "x");
        assert_eq!(
            render_reply(HookEvent::Stop, Runtime::Codex, Reply::Nothing).as_deref(),
            Some("{}")
        );
        assert_eq!(
            render_reply(HookEvent::Stop, Runtime::Claude, Reply::Nothing),
            None
        );
        let block: Value = serde_json::from_str(
            &render_reply(HookEvent::Stop, Runtime::Claude, Reply::Block("r".into())).unwrap(),
        )
        .unwrap();
        assert_eq!(block, json!({"decision": "block", "reason": "r"}));
        let codex = render_reply(HookEvent::Stop, Runtime::Codex, Reply::Block("r".into()));
        assert_eq!(
            serde_json::from_str::<Value>(&codex.unwrap()).unwrap(),
            json!({"decision": "block", "reason": "r"}),
            "Codex takes the same block shape"
        );
    }
}
