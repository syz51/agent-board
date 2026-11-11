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
use crate::mcp;
use crate::render;
use crate::state::{self, AgentLocal, DoneMark, SessionLocal, ThreadRead, ToolStart};
use crate::t3::{PULL_REQUEST_UPDATE, T3Cli, ThreadFacts};

/// T3 thread ids of threads T3 started for a parent thread's `delegate_task`.
const DELEGATED_THREAD: &str = "thread:delegated-task:";
/// The route a delegated thread's `SessionStart` context names for a PR waiting on checks.
/// T3's PR rules, which every thread sees, say to call `watch_pull_request`, which T3 gives
/// a delegated child none of, and forbid polling and `gh pr checks --watch`; a parent's
/// brief may name this route (the coordinator's does), but children still polled. The
/// wording follows the coordinator's brief (`prompts/coordinator.md`, step 4), and a brief
/// that says otherwise wins.
const DELEGATED_PR_ROUTE: &str = "PR waiting on checks, unless your brief says otherwise: turn on auto-merge only where the base branch requires status checks or reviews, park the task you own in review with a note naming the PR, report, and end; never wait for CI.";

/// Whether T3 started thread `id` for a parent thread's `delegate_task`.
fn delegated_thread(id: &str) -> bool {
    id.starts_with(DELEGATED_THREAD)
}
/// Where T3 tells its threads its CLI is.
const T3_CLI_ENV: &str = "T3CODE_CLI_PATH";
/// Time for asking T3 about the agent's thread (`t3 thread show`, 0.2 to 0.7 seconds
/// measured); past it the agent registers without its title, link or parent, and a
/// background read ([`read_later`]) brings them to a later hook.
const T3_BUDGET: Duration = Duration::from_millis(400);
/// Time a background read gives `t3 thread show`.
const READ_LATER_BUDGET: Duration = Duration::from_secs(30);
/// A session starts a background read at most this often, so two never run at once.
/// Longer than [`READ_LATER_BUDGET`].
const READ_LATER_EVERY: Duration = Duration::from_secs(45);
/// A `Stop` asks T3 for the thread's title at the session's first `Stop`, then at most this
/// often: T3 renames a thread after its first turn, and the user may rename it later.
const TITLE_REFRESH: Duration = Duration::from_secs(3600);

/// Hard limit for the whole hook, start to output.
const BUDGET: Duration = Duration::from_millis(1500);
/// Kept free after the network work for writing state and output.
const MARGIN: Duration = Duration::from_millis(200);
/// Time for opening a connection to the server, TLS included (well under 0.1 s on a
/// tailnet). Past it the server counts as unreachable, not slow: a connect that hangs (a
/// peer asleep or dropping packets) never answers however long the hook waits.
const CONNECT_BUDGET: Duration = Duration::from_millis(500);
/// What a `Stop` leaves of the server share after registering a renamed thread, so the
/// hook answers before its own deadline.
const RENAME_MARGIN: Duration = Duration::from_millis(50);
/// How long before the request's deadline a direct connect's deadline falls when the time
/// left as it starts is less than this beyond [`CONNECT_BUDGET`]: a connect that hangs then
/// fails on its own deadline, as unreachable, instead of racing the request's.
const CONNECT_LEAD: Duration = Duration::from_millis(100);
/// A server share shorter than this, because the hook's local steps took the rest of the
/// budget, is too little to judge the server: a slow answer leaves the breaker as it was, and
/// a direct connect starting with less than this left gets no deadline of its own, so one
/// that hangs ends with the request, as slow. A connection that fails at once is unreachable
/// whatever the share. At least [`CONNECT_LEAD`] plus a fair connect (well over the 0.1 s a
/// tailnet takes).
const SERVER_SHARE_FAIR: Duration = Duration::from_millis(500);
/// Debug builds only: gives the server this many milliseconds from when the hook sets its
/// share, whatever the local steps took, and lets those steps take up to [`BUDGET`] more.
/// Tests set the share with it instead of slowing the machine; a release build ignores it.
pub const SHARE_ENV: &str = "BOARD_HOOK_SERVER_SHARE_MS";
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
    /// Stop the turn now, telling the user why: a paused board holds the agent.
    Halt(String),
}

/// Entry point: reads stdin, handles the event, prints the reply. Never fails.
pub async fn run(event: HookEvent, runtime: Runtime, explicit_agent: Option<&str>) {
    let started = Instant::now();
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    // A guard around the whole hook; `handle` keeps its own, earlier deadline for the
    // network so it can still fall back to the cache.
    let guard = match share_override() {
        Some(share) => share + BUDGET,
        None => BUDGET.saturating_sub(started.elapsed() + MARGIN / 2),
    };
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
        Reply::Halt(reason) => Some(json!({"continue": false, "stopReason": reason}).to_string()),
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
    /// At `SessionStart`, the agent is a T3 thread a parent's `delegate_task` started (its
    /// own thread, not a nested session's).
    delegated: bool,
    home: Home,
    device: board_proto::DeviceName,
    client: Client,
    agent: AgentId,
    /// The runtime's id of the subagent whose tool call this hook is for (`agent_id`).
    subagent: Option<String>,
    input: &'a Value,
    local: AgentLocal,
    /// At `PostToolUse`, the files to report, in batches the server takes.
    touches: Vec<Touches>,
    /// At `SessionStart`, what T3 says of this agent's thread; at `Stop`,
    /// `UserPromptSubmit` and a `PostToolUse` with a heartbeat, the same when its title,
    /// link or parent changed since the hooks registered them.
    t3: Option<SeenThread>,
    /// At `Stop`, the done reminder once worked out, kept for when the server then fails.
    done_reminder: Option<String>,
    now: Timestamp,
    /// Shortly before the hook's server share runs out: what a hook may still spend
    /// registering the thread again ([`Hook::register_again`]).
    deadline: tokio::time::Instant,
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
        // Records of background subagents launched a week ago, long ended.
        if let Err(err) = state::prune_launched(&home, AGENT_FILE_AGE) {
            tracing::debug!("pruning subagent records: {err:#}");
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
    // Claude Code adds `agent_id` to the hooks of a subagent's tool calls, which share the
    // session, and so the agent, of the thread that launched it.
    let subagent = input["agent_id"].as_str().filter(|s| !s.is_empty());
    // Before any return below: the background subagent works on after the turn that
    // launched it ends, which clears what the agent's state knew of that turn.
    if post
        && subagent.is_none()
        && let Some(id) = launched_subagent(input)
    {
        let launched = state::load_agent(&home, &agent).turn_launch();
        state::save_launched(&home, &agent, id, &launched)?;
    }
    let breaker: Breaker = state::load_named(&home, BREAKER);
    if post && breaker.open_at(now).is_some() {
        // Mid-turn hooks stay quiet while the server is skipped: no scan, token or call.
        return Ok(Reply::Nothing);
    }
    let local = state::load_agent(&home, &agent);
    let last_beat = if subagent.is_some() {
        local.subagent_heartbeat
    } else {
        local.last_heartbeat
    };
    // Local work comes before the server's share of the budget is set, so a slow work tree
    // or T3 never looks like a slow server. T3 is asked while the Keychain lookup runs.
    // The agent is the T3 thread it runs in.
    let own_thread = !nested && t3_identity_applies(explicit_agent, env);
    let t3 = match event {
        _ if !own_thread => None,
        // A hook that skips the server registers nothing, so T3's answer would go unused.
        _ if breaker.open_at(now).is_some() => None,
        HookEvent::SessionStart => thread_at_start(&home, runtime, session, now).await,
        HookEvent::Stop => thread_renamed(&home, runtime, session, now).await,
        // Mid-session, only what a background read found ([`read_later`]): at each prompt,
        // and with a tool call's heartbeat, so a registration that keeps failing costs a
        // tool call no more than the heartbeat does.
        HookEvent::UserPromptSubmit => thread_read_later(&home, runtime, session, now),
        HookEvent::PostToolUse | HookEvent::PostToolUseFailure if heartbeat_due(last_beat, now) => {
            thread_read_later(&home, runtime, session, now)
        }
        _ => None,
    };
    steps.mark("t3");
    // A later hook that registers the thread's title registers where the agent runs too.
    let reregister = event != HookEvent::SessionStart && t3.is_some();
    let repo = if event == HookEvent::SessionStart
        || reregister
        || !edited.is_empty()
        || shell_start.is_some()
    {
        Some(detect_repo(input_cwd(input)).await?)
    } else {
        None
    };
    let touches = match &repo {
        Some(repo) if post => local_touches(&agent, repo, edited, shell_start.as_ref()).await,
        _ => Vec::new(),
    };
    let heartbeat = post && heartbeat_due(last_beat, now);
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
    let relayed = matches!(route, Route::Relay(_));
    let connects = std::sync::Arc::new(ConnectClock::default());
    let client = match route {
        Route::Relay(client) => client.with_agent(Some(agent.clone())),
        Route::Direct(token) => direct_client(server, token, Some(agent.clone()), &connects)?,
    };
    steps.mark("client");
    let delegated = event == HookEvent::SessionStart
        && own_thread
        && t3_thread_id().is_some_and(|id| delegated_thread(&id));
    let mut hook = Hook {
        relayed,
        event,
        runtime,
        nested,
        delegated,
        device: device.clone(),
        client,
        local,
        home,
        agent,
        subagent: subagent.map(str::to_owned),
        input,
        touches,
        t3,
        done_reminder: None,
        now,
        // Set with the server's share, below.
        deadline: tokio::time::Instant::now(),
    };
    // Turn boundaries for the done reminder hold whether or not the server answers.
    match event {
        // The prompt's snapshot sets the turn's hold anew, also when the server is not
        // reached, so a mark from an earlier turn never decides this one's halt.
        HookEvent::UserPromptSubmit => {
            hook.local.turn_started = Some(hook.now);
            hook.local.end_turn_hold();
        }
        // A new or resumed session: done marks left over were from a turn whose Stop never
        // ran. Compaction happens within a turn, so it keeps them and the turn's start.
        HookEvent::SessionStart if input["source"] != "compact" => {
            hook.local.turn_started = None;
            hook.local.end_turn_hold();
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
    if event == HookEvent::Stop {
        // Before any server call, so it survives a server that fails (`local_stop`), and
        // before the share is measured, so its file reads count against the share.
        hook.prepare_done_reminder();
    }
    // What the server gets: what the local steps, the client build and a `Stop`'s local work
    // left of the budget.
    let server_deadline = server_deadline(tokio::time::Instant::from_std(started));
    connects.set_deadline(server_deadline);
    let share = server_deadline.saturating_duration_since(tokio::time::Instant::now());
    hook.deadline = server_deadline - RENAME_MARGIN;
    hook.client = hook.client.clone().with_timeout(share);
    let fair = share >= SERVER_SHARE_FAIR;
    tracing::debug!(
        "hook {}: server share {}ms{}",
        event.name(),
        share.as_millis(),
        if fair { "" } else { ", too little to judge" }
    );
    // `judged`: whether a failure says anything about the server.
    let online = tokio::time::timeout_at(server_deadline, hook.online(repo)).await;
    let (reply, failure, judged) = match online {
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
            tracing::debug!("hook {}: no answer within {share:?}", event.name());
            (hook.offline(Failure::Slow), Some(Failure::Slow), true)
        }
    };
    steps.mark("server");
    let next = match failure {
        // The relay failed, not the server.
        _ if !judged => breaker.clone(),
        // This device left the server too little time to judge it slow, or a connect
        // started with too little time left to get a deadline of its own.
        Some(Failure::Slow) if !fair || connects.unjudged() => breaker.clone(),
        failure => breaker.after(failure, hook.now)?,
    };
    if next != breaker {
        state::save_named(&hook.home, BREAKER, &next)?;
    }
    state::save_agent(&hook.home, &hook.agent, &hook.local)?;
    Ok(reply)
}

/// The subagent a background launch started: Claude Code's `Agent` tool (`Task` before)
/// answers at once with `async_launched` and the subagent's id.
fn launched_subagent(input: &Value) -> Option<&str> {
    if !matches!(input["tool_name"].as_str(), Some("Agent" | "Task")) {
        return None;
    }
    let response = &input["tool_response"];
    if response["status"] != "async_launched" {
        return None;
    }
    response["agentId"].as_str().filter(|s| !s.is_empty())
}

/// `text` with its `hold:` line replaced by one for a subagent the hold lets work on; the
/// line goes first when the server's text has none the hook recognizes.
fn subagent_hold(text: &str, boards: &[board_proto::BoardSlug], for_usage: bool) -> String {
    let why = if for_usage {
        "held: the weekly usage reached the user's reserve"
    } else {
        "paused"
    };
    let line = format!(
        "hold: {} {why} -> the turn that launched you began while held and went on, which the \
         hold allows only for the user's own request: finish your task, and start no other \
         task, child thread or wake-up.",
        board_names(boards)
    );
    let mut replaced = false;
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| {
            if l.starts_with("hold: ") {
                replaced = true;
                line.as_str()
            } else {
                l
            }
        })
        .collect();
    if replaced {
        lines.join("\n")
    } else {
        format!("{line}\n{text}")
    }
}

fn board_names(boards: &[board_proto::BoardSlug]) -> String {
    boards
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The prompt is T3's wake-up from a pull request watch, not the user's message.
fn is_pull_request_update(input: &Value) -> bool {
    input["prompt"]
        .as_str()
        .is_some_and(|p| p.starts_with(PULL_REQUEST_UPDATE))
}

/// What the user sees when the hold stops a held agent's turn: a pause, or the weekly
/// usage at the reserve (`Snapshot::held_for_usage`).
fn halt_reason(snap: &Snapshot) -> String {
    if snap.held_for_usage {
        return format!(
            "board: the weekly usage reached your reserve, so this turn for {} stopped. \
             `board usage` shows it; `board usage --reserve` changes it.",
            board_names(&snap.held_by)
        );
    }
    format!(
        "board: {} paused, so this turn stopped. `board resume` continues the work.",
        board_names(&snap.held_by)
    )
}

/// When the hook stops waiting for the server: [`BUDGET`] - [`MARGIN`] after it `started`,
/// however long the local steps, the client build and the local work before the first
/// request took. With [`SHARE_ENV`] (debug builds), that share from now instead; the hook
/// asks just before its first request.
fn server_deadline(started: tokio::time::Instant) -> tokio::time::Instant {
    match share_override() {
        Some(share) => tokio::time::Instant::now() + share,
        None => started + (BUDGET - MARGIN),
    }
}

/// The server share a test set with [`SHARE_ENV`], at most [`BUDGET`]; always `None` in a
/// release build. A value that is not a whole number of milliseconds panics, so a test never
/// runs on the real share by mistake.
fn share_override() -> Option<Duration> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let ms = std::env::var(SHARE_ENV).ok()?;
    let ms: u64 = ms
        .parse()
        .unwrap_or_else(|_| panic!("{SHARE_ENV}={ms:?}: give whole milliseconds"));
    Some(Duration::from_millis(ms).min(BUDGET))
}

/// What a direct client's connects share with the hook: the server's deadline, set when
/// the hook measures the server's share, and whether a connect started with too little time
/// left to judge the server.
#[derive(Default)]
struct ConnectClock {
    deadline: std::sync::OnceLock<tokio::time::Instant>,
    unjudged: std::sync::atomic::AtomicBool,
}

impl ConnectClock {
    fn set_deadline(&self, deadline: tokio::time::Instant) {
        // Set once per hook; a second value would be the same deadline.
        let _ = self.deadline.set(deadline);
    }

    /// The deadline of a connect starting now ([`connect_limit`]); none before the server's
    /// deadline is set.
    fn limit(&self) -> Option<Duration> {
        let deadline = *self.deadline.get()?;
        let limit = connect_limit(deadline.saturating_duration_since(tokio::time::Instant::now()));
        if limit.is_none() {
            self.unjudged
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        limit
    }

    /// A connect started with too little time left to get a deadline of its own, so a slow
    /// answer says nothing about the server.
    fn unjudged(&self) -> bool {
        self.unjudged.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A client that calls the server directly. The connect's deadline never races the
/// request's: set as each connect starts ([`ConnectClock::limit`]), it falls well before
/// it, or, with too little time to judge, there is none, so one that hangs reads as slow.
/// Work between building the client and the first request so shortens no lead. Each
/// request gets [`BUDGET`] until the hook sets the share with [`Client::with_timeout`].
fn direct_client(
    server: &str,
    token: String,
    agent: Option<AgentId>,
    connects: &std::sync::Arc<ConnectClock>,
) -> Result<Client> {
    let connects = connects.clone();
    Client::with_connect_limit(server, token, agent, BUDGET, move || connects.limit())
}

/// The deadline of a direct connection that starts `left` before the server's deadline: at
/// most [`CONNECT_BUDGET`], and [`CONNECT_LEAD`] before the request's. None with less than
/// [`SERVER_SHARE_FAIR`] left, too little to judge, so one that hangs ends with the request.
fn connect_limit(left: Duration) -> Option<Duration> {
    (left >= SERVER_SHARE_FAIR).then(|| CONNECT_BUDGET.min(left - CONNECT_LEAD))
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
    /// The connection failed: refused, DNS, TLS, or not open within its deadline (at most
    /// [`CONNECT_BUDGET`]). The server cannot take writes now.
    Unreachable,
}

impl Failure {
    /// A request that timed out, unless on the connect's own deadline, is slow; any other
    /// failure to get an answer is unreachable, a connect that timed out included.
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
        // The agent is marked ended whether or not its files go.
        if event == HookEvent::SessionEnd
            && let Err(err) = state::remove_session(home, runtime, session)
        {
            tracing::warn!("forgetting session {session}: {err:#}");
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
        let mut local = state::load_session(home, runtime, session).unwrap_or_default();
        local.nested = nested;
        state::save_session(home, runtime, session, &local)?;
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

/// This agent's T3 thread, as the hooks register it.
#[derive(Debug, Clone, PartialEq)]
struct SeenThread {
    id: String,
    facts: ThreadFacts,
    from: Source,
}

/// Where the hook got a thread's facts.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    /// The session's cache: what the hooks registered before.
    Cache,
    /// T3, asked by this hook at this time.
    Asked(Timestamp),
    /// A background read ([`read_later`]) that asked T3 at this time.
    Read(Timestamp),
}

impl Source {
    /// When T3 gave the facts; `None` for the cache.
    fn at(self) -> Option<Timestamp> {
        match self {
            Self::Cache => None,
            Self::Asked(at) | Self::Read(at) => Some(at),
        }
    }
}

/// The T3 thread id this process runs in, when T3 set one.
fn t3_thread_id() -> Option<String> {
    let thread = std::env::var(T3_THREAD_ENV).ok()?;
    Some(thread.trim().to_owned()).filter(|t| !t.is_empty())
}

/// What the hooks registered for `thread`, as the session's cache keeps it; nothing when
/// they registered another thread, or none.
fn registered(cached: &SessionLocal, thread: &str) -> ThreadFacts {
    if cached.t3_thread.as_deref() != Some(thread) {
        return ThreadFacts::default();
    }
    ThreadFacts {
        parent: cached.t3_parent.clone(),
        title: cached.t3_title.clone(),
        link: cached.t3_link.clone(),
    }
}

/// Whether a background read for this session may be running: one started less than
/// [`READ_LATER_EVERY`] ago. Asking T3 again meanwhile would only wait out the budget.
fn reading(cached: &SessionLocal, now: Timestamp) -> bool {
    !due(cached.thread_read_at, now, READ_LATER_EVERY)
}

/// What T3 says of this session's thread at `SessionStart`: from the session's cache once
/// the hooks registered it, else from a background read that finished
/// ([`thread_read_later`]), else asked of T3, unless a background read is running. The
/// parent is kept only for a thread T3 started for a parent's `delegate_task`. `None`
/// outside T3 or when T3 does not answer in time; then a background read brings the answer
/// to a later hook.
async fn thread_at_start(
    home: &Home,
    runtime: Runtime,
    session: Option<&str>,
    now: Timestamp,
) -> Option<SeenThread> {
    let id = t3_thread_id()?;
    let mut cached = session
        .and_then(|s| state::load_session(home, runtime, s))
        .unwrap_or_default();
    if cached.t3_thread.as_deref() == Some(id.as_str()) {
        let facts = registered(&cached, &id);
        return Some(SeenThread {
            id,
            facts,
            from: Source::Cache,
        });
    }
    if let Some(seen) = thread_read_later(home, runtime, session, now) {
        return Some(seen);
    }
    if reading(&cached, now) {
        return None;
    }
    let mut facts = match ask_t3(home, &id).await {
        Ok(facts) => facts?,
        Err(Late) => {
            if let Some(session) = session
                && read_later(home, runtime, session, &mut cached, &id, now) == ReadLater::Started
                && let Err(err) = state::save_session(home, runtime, session, &cached)
            {
                tracing::debug!("recording the background read: {err:#}");
            }
            return None;
        }
    };
    if !delegated_thread(&id) {
        facts.parent = None;
    }
    Some(SeenThread {
        id,
        facts,
        from: Source::Asked(now),
    })
}

/// A background read of this session's thread that finished ([`read_later`]), when it
/// tells something the hooks have not registered: a title, link or parent. One that tells
/// nothing new, asked T3 before the registered facts were read, is over an hour old or was
/// of another thread is dropped. Runs at each hook that may register it, so it only reads
/// files.
fn thread_read_later(
    home: &Home,
    runtime: Runtime,
    session: Option<&str>,
    now: Timestamp,
) -> Option<SeenThread> {
    let id = t3_thread_id()?;
    let session = session?;
    let read = state::load_thread_read(home, runtime, session)?;
    let cached = state::load_session(home, runtime, session).unwrap_or_default();
    let had = registered(&cached, &id);
    let mut facts = ThreadFacts {
        parent: read.parent,
        title: read.title,
        link: read.link,
    };
    if !delegated_thread(&id) {
        facts.parent = None;
    }
    let older = cached.t3_thread.as_deref() == Some(id.as_str())
        && cached.t3_facts_at.is_some_and(|at| at >= read.at);
    let new_parent = facts.parent.is_some() && facts.parent != had.parent;
    let news = new_parent || thread_changed(had.title.as_deref(), had.link.as_deref(), &facts);
    if read.thread != id || older || due(Some(read.at), now, TITLE_REFRESH) || !news {
        if let Err(err) = state::remove_thread_read(home, runtime, session) {
            tracing::debug!("dropping the thread read: {err:#}");
        }
        return None;
    }
    facts.parent = facts.parent.or(had.parent);
    Some(SeenThread {
        id,
        facts,
        from: Source::Read(read.at),
    })
}

/// At `Stop`: when a title check is due ([`title_check_due`]), what T3 says of this
/// session's thread, when its title or link differs from what the hooks registered
/// ([`thread_changed`]); else, or when T3 is too slow, what a finished background read
/// found ([`thread_read_later`]). A due check is asked first, since a read may predate a
/// rename. A check that finds no change or no answer, or that started a background read
/// (which brings its answer to a later hook), counts as done, so a slow T3 costs one `Stop`
/// an hour. While an earlier read runs, the check stays due without asking. A change
/// counts once the server took it ([`Hook::register`]), so a failed registration is tried
/// again at the next `Stop`. `None` without a session cache, which `SessionStart` (or the
/// session's first hook) writes.
async fn thread_renamed(
    home: &Home,
    runtime: Runtime,
    session: Option<&str>,
    now: Timestamp,
) -> Option<SeenThread> {
    let id = t3_thread_id()?;
    let session = session?;
    let mut cached = state::load_session(home, runtime, session)?;
    if !title_check_due(cached.title_checked_at, now) || reading(&cached, now) {
        return thread_read_later(home, runtime, Some(session), now);
    }
    let had = registered(&cached, &id);
    let read = match ask_t3(home, &id).await {
        Ok(read) => read,
        Err(Late) => {
            // Running only when another hook started one meanwhile: the check stays due.
            if read_later(home, runtime, session, &mut cached, &id, now) != ReadLater::Running {
                cached.title_checked_at = Some(now);
                if let Err(err) = state::save_session(home, runtime, session, &cached) {
                    tracing::debug!("recording the title check: {err:#}");
                }
            }
            return thread_read_later(home, runtime, Some(session), now);
        }
    };
    let mut facts = match read {
        Some(facts) if thread_changed(had.title.as_deref(), had.link.as_deref(), &facts) => facts,
        _ => {
            cached.title_checked_at = Some(now);
            // An unchanged answer confirms the registered facts, so a read that asked
            // before it is older news.
            if read.is_some() && cached.t3_thread.as_deref() == Some(id.as_str()) {
                cached.t3_facts_at = Some(now);
            }
            if let Err(err) = state::save_session(home, runtime, session, &cached) {
                tracing::debug!("recording the title check: {err:#}");
            }
            return None;
        }
    };
    // As at `SessionStart`: a parent only for a delegated thread, else the one registered
    // (T3's answer may now have one the session's first registration missed).
    if !delegated_thread(&id) {
        facts.parent = None;
    }
    facts.parent = facts.parent.or(had.parent);
    Some(SeenThread {
        id,
        facts,
        from: Source::Asked(now),
    })
}

/// Whether T3's answer has a title or link other than the ones registered. A field T3 left
/// out is no change: the server keeps what it has.
fn thread_changed(title: Option<&str>, link: Option<&str>, read: &ThreadFacts) -> bool {
    let differs =
        |read: &Option<String>, had: Option<&str>| read.as_deref().is_some_and(|r| Some(r) != had);
    differs(&read.title, title) || differs(&read.link, link)
}

/// True when a `Stop` should ask T3 for the thread's title: at the session's first `Stop`,
/// then once [`TITLE_REFRESH`] has passed.
pub fn title_check_due(last: Option<Timestamp>, now: Timestamp) -> bool {
    due(last, now, TITLE_REFRESH)
}

/// The `t3` this thread's T3 names, else the configured one.
fn t3_cli(home: &Home) -> T3Cli {
    let bin = std::env::var_os(T3_CLI_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| Config::load(home).ok()?.t3_bin);
    T3Cli::locate(bin.as_deref())
}

/// T3 did not answer within [`T3_BUDGET`].
struct Late;

/// Asks T3 about `thread` within [`T3_BUDGET`]. `Ok(None)` when T3 does not know the
/// thread or fails.
async fn ask_t3(home: &Home, thread: &str) -> Result<Option<ThreadFacts>, Late> {
    match tokio::time::timeout(T3_BUDGET, t3_cli(home).thread_facts(thread)).await {
        Ok(Ok(facts)) => Ok(facts),
        Ok(Err(err)) => {
            tracing::debug!("thread {thread}: {err:#}");
            Ok(None)
        }
        Err(_) => Err(Late),
    }
}

/// What [`read_later`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadLater {
    Started,
    /// One started less than [`READ_LATER_EVERY`] ago may still run.
    Running,
    /// `board read-thread` did not start.
    Failed,
}

/// Where `board read-thread` writes what goes wrong, in `$BOARD_HOME/state`: its errors
/// would be lost otherwise, since it runs with no terminal or runtime to read them.
pub const READ_LOG: &str = "read-thread.log";
/// The log starts over once it is this long.
const READ_LOG_BYTES_MAX: u64 = 256 * 1024;

/// T3 answered too slowly for the hook: starts `board read-thread` on its own, which gives
/// `t3 thread show` [`READ_LATER_BUDGET`] and leaves the answer in the session's state for
/// its next hooks ([`thread_read_later`]): `UserPromptSubmit`, `PostToolUse` or `Stop`. At
/// most once per [`READ_LATER_EVERY`]; the session's first is logged at info level. `local`
/// records the start, and the caller saves it.
fn read_later(
    home: &Home,
    runtime: Runtime,
    session: &str,
    local: &mut SessionLocal,
    thread: &str,
    now: Timestamp,
) -> ReadLater {
    if reading(local, now) {
        tracing::debug!("thread {thread}: over budget; a background read started lately");
        return ReadLater::Running;
    }
    let mut command = tokio::process::Command::new(current_exe());
    command
        .args([
            "read-thread",
            "--runtime",
            state::runtime_name(runtime),
            "--",
            session,
            thread,
        ])
        .env("BOARD_HOME", home.root())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(read_log(home));
    // Its own process group, so a runtime that stops the hook's group spares it.
    #[cfg(unix)]
    command.process_group(0);
    // Not waited for: it outlives the hook, and dropping it neither kills nor blocks.
    if let Err(err) = command.spawn() {
        tracing::warn!("thread {thread}: starting a background read: {err:#}");
        return ReadLater::Failed;
    }
    if local.thread_read_at.is_none() {
        tracing::info!(
            "thread {thread}: t3 thread show took over {T3_BUDGET:?}; reading it in the background for a later hook"
        );
    } else {
        tracing::debug!("thread {thread}: over budget; reading it in the background again");
    }
    local.thread_read_at = Some(now);
    ReadLater::Started
}

/// [`READ_LOG`] to append to, started over once it reached [`READ_LOG_BYTES_MAX`]; no
/// output when it cannot be opened.
fn read_log(home: &Home) -> std::process::Stdio {
    let path = home.state_dir().join(READ_LOG);
    let long = std::fs::metadata(&path).is_ok_and(|m| m.len() >= READ_LOG_BYTES_MAX);
    let open = std::fs::create_dir_all(home.state_dir()).and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(!long)
            .write(true)
            .truncate(long)
            .open(&path)
    });
    match open {
        Ok(file) => file.into(),
        Err(err) => {
            tracing::debug!("opening {}: {err:#}", path.display());
            std::process::Stdio::null()
        }
    }
}

/// `board read-thread`: reads `thread` within [`READ_LATER_BUDGET`] and keeps T3's answer,
/// with the time it asked, for the session's next hooks. Errors go to its log
/// ([`READ_LOG`]), never to the caller: a later hook reads again.
pub async fn read_thread(runtime: Runtime, session: &str, thread: &str) {
    let read = async {
        let home = Home::from_env()?;
        let at = Utc::now();
        let facts = tokio::time::timeout(READ_LATER_BUDGET, t3_cli(&home).thread_facts(thread))
            .await
            .map_err(|_| anyhow::anyhow!("no answer within {READ_LATER_BUDGET:?}"))??;
        let Some(facts) = facts else {
            tracing::debug!("thread {thread}: T3 does not know it");
            return Ok(());
        };
        let read = ThreadRead {
            thread: thread.to_owned(),
            at,
            parent: facts.parent,
            title: facts.title,
            link: facts.link,
        };
        state::save_thread_read(&home, runtime, session, &read)
    };
    if let Err(err) = read.await {
        tracing::warn!("reading thread {thread}: {err:#}");
    }
}

impl Hook<'_> {
    fn cwd(&self) -> PathBuf {
        input_cwd(self.input)
    }

    /// `repo`: the work tree, when the local steps detected it.
    async fn online(&mut self, repo: Option<RepoInfo>) -> Result<Reply> {
        match self.event {
            HookEvent::SessionStart => self.session_start(repo).await,
            HookEvent::UserPromptSubmit => {
                let reply = self.user_prompt_submit().await?;
                // Registering marks the agent running, which a halted turn is not.
                if !matches!(reply, Reply::Halt(_)) {
                    self.register_again(repo.as_ref()).await;
                }
                Ok(reply)
            }
            // Handled before login (`handle`).
            HookEvent::PreToolUse => Ok(Reply::Nothing),
            // A failed shell command may still have changed files.
            HookEvent::PostToolUse | HookEvent::PostToolUseFailure => {
                let reply = self.post_tool_use().await?;
                // Registering marks the agent running, which a halted turn is not.
                if !matches!(reply, Reply::Halt(_)) {
                    self.register_again(repo.as_ref()).await;
                }
                Ok(reply)
            }
            HookEvent::Stop => self.stop(repo).await,
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
            HookEvent::SessionStart => match self.cached_snapshot(failure) {
                Reply::Context(text) => Reply::Context(self.board_context(text)),
                reply => reply,
            },
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
        if self.stop_continued() {
            return Ok(None);
        }
        Ok(done_reminder(
            marks,
            self.local.turn_started,
            &mut self.local.done_reminded,
        ))
    }

    /// `SessionStart` context from a board snapshot: a delegated thread's names the route
    /// for a PR waiting on checks ([`DELEGATED_PR_ROUTE`]), then the `ToolSearch` call.
    fn board_context(&self, snapshot: String) -> String {
        let context = if self.delegated {
            format!("{snapshot}\n{DELEGATED_PR_ROUTE}")
        } else {
            snapshot
        };
        self.with_tool_search(context, &[])
    }

    /// Ends `SessionStart` board context with the `ToolSearch` call that loads `first` and
    /// the board tools a session calls first, and says how to load any other. Claude Code
    /// may show MCP tools by name only, so an agent that calls one before loading it guesses
    /// its arguments and fails once. This is the one place that names the tools to load.
    fn with_tool_search(&self, context: String, first: &[&str]) -> String {
        match self.runtime {
            Runtime::Claude => {
                let tools: Vec<String> = first
                    .iter()
                    .chain(mcp::CORE_TOOLS)
                    .map(|tool| format!("mcp__{}__{tool}", mcp::SERVER_NAME))
                    .collect();
                format!(
                    "{context}\nIf the board tools show by name only, load these with your first board work, and any other the same way (mcp__{}__<tool>) when you need it: ToolSearch query \"select:{}\" max_results {}",
                    mcp::SERVER_NAME,
                    tools.join(","),
                    tools.len()
                )
            }
            // Neither has `ToolSearch`: both load every tool schema themselves.
            Runtime::Codex | Runtime::Opencode => context,
        }
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
        self.local.held = !snap.held_by.is_empty();
        self.local.held_since = snap.held_since;
        if snap.kind == SnapshotKind::Full {
            state::save_cached_snapshot(&self.home, &self.agent, &snap)?;
        }
        Ok(snap.text)
    }

    async fn snapshot(&mut self, kind: SnapshotKind) -> Result<String> {
        let snap = self.fetch(kind).await?;
        self.keep(snap)
    }

    /// Registers the agent where it runs, as a worker (the server keeps a board's
    /// coordinator its role and title), with the T3 thread's title, link and parent when
    /// known. Once the server took them, the session's cache keeps them, so a later
    /// `SessionStart` of the session need not ask T3 and a `Stop` sends only a change.
    async fn register(&self, repo: &RepoInfo) -> Result<()> {
        let mut native = std::collections::BTreeMap::new();
        let session = self.input["session_id"].as_str();
        if let Some(session) = session {
            let key = match self.runtime {
                Runtime::Claude => "claude_session",
                Runtime::Codex => "codex_session",
                Runtime::Opencode => "opencode_session",
            };
            native.insert(key.to_owned(), session.to_owned());
        }
        let thread = t3_thread_id().filter(|_| !self.nested);
        if let Some(thread) = &thread {
            native.insert("t3_thread".to_owned(), thread.clone());
        }
        let facts = self.t3.as_ref().map(|t| &t.facts);
        // The server keeps a delegated child's edits and its parent's out of each other's
        // overlaps and alerts.
        if let Some(parent) = facts.and_then(|f| f.parent.clone()) {
            native.insert("t3_parent".to_owned(), parent);
        }
        if let Some(link) = facts.and_then(|f| f.link.clone()) {
            native.insert("t3_link".to_owned(), link);
        }
        self.client
            .register(&board_proto::RegisterAgent {
                id: self.agent.clone(),
                runtime: self.runtime,
                device: self.device.clone(),
                role: board_proto::AgentRole::Worker,
                title: facts.and_then(|f| f.title.clone()),
                repo: repo.key.clone(),
                branch: repo.branch.clone(),
                cwd: Some(repo.cwd.display().to_string()),
                native_ids: native,
                forked_from: None,
            })
            .await?;
        if let (Some(session), Some(seen)) = (session, &self.t3) {
            let mut local =
                state::load_session(&self.home, self.runtime, session).unwrap_or_default();
            local.t3_thread = Some(seen.id.clone());
            local.t3_parent = seen.facts.parent.clone();
            // A field T3 left out keeps what was registered before.
            if seen.facts.title.is_some() {
                local.t3_title = seen.facts.title.clone();
            }
            if seen.facts.link.is_some() {
                local.t3_link = seen.facts.link.clone();
            }
            // Only a check this `Stop` asked counts: a background read may predate a rename.
            if self.event == HookEvent::Stop && matches!(seen.from, Source::Asked(_)) {
                local.title_checked_at = Some(self.now);
            }
            if let Some(at) = seen.from.at() {
                local.t3_facts_at = Some(at);
            }
            if let Err(err) = state::save_session(&self.home, self.runtime, session, &local) {
                tracing::debug!("caching the thread's title and link: {err:#}");
            }
            // A background read waiting that asked T3 no later than these facts is spent.
            if let Some(at) = seen.from.at()
                && state::load_thread_read(&self.home, self.runtime, session)
                    .is_some_and(|read| read.at <= at)
                && let Err(err) = state::remove_thread_read(&self.home, self.runtime, session)
            {
                tracing::debug!("dropping the registered thread read: {err:#}");
            }
        }
        Ok(())
    }

    async fn session_start(&mut self, repo: Option<RepoInfo>) -> Result<Reply> {
        let repo = match repo {
            Some(repo) => repo,
            None => detect_repo(self.cwd()).await?,
        };
        self.register(&repo).await?;
        self.local.reminded_for = None;
        let text = self.snapshot(SnapshotKind::Full).await?;
        if !text.is_empty() {
            return Ok(Reply::Context(self.board_context(text)));
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
            Some(board) => Reply::Context(self.with_tool_search(
                format!(
                    "board: this repo has board {} ({}); board_join {} to see and claim its tasks.",
                    board.slug,
                    serde_json::to_value(board.status)?
                        .as_str()
                        .unwrap_or_default(),
                    board.slug
                ),
                &["board_join"],
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
        let snap = if self.runtime == Runtime::Claude && is_pull_request_update(self.input) {
            // A held thread's pull request watch wakes it at every check or review change,
            // for as long as the pause lasts. Such a turn ends before the model runs, and
            // the watch stays; the resume wake-up has the coordinator check the PRs. The
            // snapshot comes first, so an ended turn changes no state: the agent stays idle
            // (or paused for a usage limit), and its sequence stays, so the next snapshot
            // brings again the alerts this one had. Codex is not ended here, since it may
            // not honor `continue: false` at a prompt; it is told of the hold instead.
            let snap = self.fetch(kind).await?;
            if !snap.held_by.is_empty() {
                self.local.held = true;
                self.local.held_since = snap.held_since;
                return Ok(Reply::Halt(halt_reason(&snap)));
            }
            self.set_state(AgentState::Running, None).await?;
            snap
        } else {
            self.beat_and_fetch(kind).await?
        };
        self.local.turn_seen_at = Some(snap.at);
        let text = self.keep(snap)?;
        // The snapshot's `hold:` line tells the agent; only the user's request continues.
        self.local.hold_told = self.local.held;
        Ok(if text.is_empty() {
            Reply::Nothing
        } else {
            Reply::Context(text)
        })
    }

    async fn post_tool_use(&mut self) -> Result<Reply> {
        // A halt shows no overlap, so it marks none as shown.
        let shown = self.local.overlaps_shown.clone();
        let mut lines = Vec::new();
        for touches in &self.touches {
            let mut result = self.client.touches(touches).await?;
            // Every call reports every overlap; each is shown once per peer change.
            result
                .overlaps
                .retain(|o| self.local.first_sight(o, self.now));
            lines.extend(render::overlaps(&result));
        }
        // A subagent's heartbeat runs on a clock of its own, so a subagent the hold lets
        // work on never takes the heartbeat that halts the agent's own held turn.
        let beat = match self.subagent {
            Some(_) => &mut self.local.subagent_heartbeat,
            None => &mut self.local.last_heartbeat,
        };
        if heartbeat_due(*beat, self.now) {
            *beat = Some(self.now);
            let snap = self.beat_and_fetch(SnapshotKind::Alerts).await?;
            let boards = snap.held_by.clone();
            let for_usage = snap.held_for_usage;
            let reason = halt_reason(&snap);
            let mut alerts = self.keep(snap)?;
            if self.local.held && self.subagent_passes() {
                // The subagent's turn holds no user message, so the hold line would stop
                // the work its launching turn went on with.
                alerts = subagent_hold(&alerts, &boards, for_usage);
            } else if self.local.halted() {
                // A turn that was running when its board was paused stops here, at every
                // heartbeat until it ends: a runtime may not end it at the first (a
                // subagent's tool call, or a runtime that ignores the halt).
                self.local.overlaps_shown = shown;
                self.set_state(AgentState::Idle, None).await?;
                return Ok(Reply::Halt(reason));
            }
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

    /// Whether the hold lets this hook's subagent work on ([`state::Launched::passes`]): one
    /// launched in the background by its record, one in the foreground by the current
    /// turn, which runs it.
    fn subagent_passes(&self) -> bool {
        let Some(id) = &self.subagent else {
            return false;
        };
        state::load_launched(&self.home, &self.agent, id)
            .unwrap_or_else(|| self.local.turn_launch())
            .passes(self.local.held_since)
    }

    /// A `Stop` that follows our own block, ending the turn that answered a reminder.
    fn stop_continued(&self) -> bool {
        self.input["stop_hook_active"].as_bool().unwrap_or(false)
    }

    /// Marks the agent idle with the turn's last message, which the server keeps for the
    /// user while the agent coordinates a board. A turn the reminders continue has not
    /// ended, so its message is not sent; the next `Stop` sends the final one.
    ///
    /// Both reminders go in one block: the turn continues once, and the `Stop` that follows
    /// has `stop_hook_active` and stays quiet, so a second block would never be seen.
    /// `handle` has prepared the done reminder ([`Hook::prepare_done_reminder`]).
    async fn stop(&mut self, repo: Option<RepoInfo>) -> Result<Reply> {
        // The message worth keeping after our own block was the one the reminder interrupted.
        let continued = self.stop_continued();
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
        // A halted turn is not asked to go on: the user paused its work. A turn begun
        // while held (the user's own request) gets the reminders as any other.
        let reply = if continued || self.local.halted() {
            Reply::Nothing
        } else {
            let handoff = self.handoff_reminder().await?;
            let text: Vec<String> = self
                .done_reminder
                .take()
                .into_iter()
                .chain(handoff)
                .collect();
            if text.is_empty() {
                Reply::Nothing
            } else {
                Reply::Block(text.join("\n"))
            }
        };
        // A block continues the turn; otherwise it has ended, and a turn without a prompt
        // that follows has no time of its own.
        if !matches!(reply, Reply::Block(_)) {
            self.local.end_turn_hold();
        }
        self.register_again(repo.as_ref()).await;
        Ok(reply)
    }

    /// T3 renamed the thread, or a background read found its title, link or parent:
    /// registers it again, last and in the time left; at `Stop`, then marks it idle again,
    /// since registering marks it running (mid-turn it is running anyway). Rare (about
    /// twice a thread). A failure is logged and the next such hook tries again, except that
    /// a read the server refuses as invalid is dropped; one that cuts off between the two
    /// requests leaves the agent marked running until its next hook.
    async fn register_again(&self, repo: Option<&RepoInfo>) {
        let (Some(repo), Some(_)) = (repo, &self.t3) else {
            return;
        };
        let again = async {
            self.register(repo).await?;
            if self.event == HookEvent::Stop {
                self.set_state(AgentState::Idle, None).await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        match tokio::time::timeout_at(self.deadline, again).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::debug!("registering the thread's new title: {err:#}");
                // The server refuses what the read found: trying again would not help.
                if crate::client::error_code(&err) == Some(board_proto::ErrorCode::Invalid)
                    && matches!(self.t3.as_ref().map(|t| t.from), Some(Source::Read(_)))
                    && let Some(session) = self.input["session_id"].as_str()
                    && let Err(err) = state::remove_thread_read(&self.home, self.runtime, session)
                {
                    tracing::debug!("dropping the refused thread read: {err:#}");
                }
            }
            Err(_) => tracing::debug!("registering the thread's new title: out of time"),
        }
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
        // The server dates the agent's last change that the handoff must cover (a change to
        // the state of its tasks, or file edits long after the handoff) on the handoff's own
        // clock, so a turn that only read, replied or added notes leaves it current. A server
        // without that date falls back to: a handoff older than the turn is stale.
        let (stale_since, why) = match &handoff {
            None => (Some(turn.unwrap_or(self.now)), "you have no handoff"),
            Some(h) => match h.last_change_at {
                Some(at) => (
                    (at > h.updated_at).then_some(at),
                    "your handoff predates a later change to your tasks or later file edits",
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
            "Keep in the summary, verbatim: the board agent id {}{task}.",
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
    due(last, now, HEARTBEAT_EVERY)
}

/// True when nothing happened yet or `every` has passed since `last`. A clock that went
/// backwards makes it due, so it never waits forever.
fn due(last: Option<Timestamp>, now: Timestamp, every: Duration) -> bool {
    last.is_none_or(|last| (now - last).to_std().map_or(true, |since| since >= every))
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

    #[tokio::test(start_paused = true)]
    async fn the_server_deadline_counts_all_time_since_the_hook_started() {
        let started = tokio::time::Instant::now();
        // Local steps, then the client build and a `Stop`'s local work.
        tokio::time::advance(Duration::from_millis(400)).await;
        let deadline = server_deadline(started);
        tokio::time::advance(Duration::from_millis(300)).await;
        assert_eq!(deadline, started + BUDGET - MARGIN);
        assert_eq!(server_deadline(started), deadline);
    }

    #[test]
    fn a_connect_gets_its_budget_and_falls_before_the_request() {
        let ms = Duration::from_millis;
        assert_eq!(connect_limit(ms(1300)), Some(CONNECT_BUDGET));
        assert_eq!(connect_limit(ms(550)), Some(ms(450)));
        assert_eq!(connect_limit(SERVER_SHARE_FAIR), Some(ms(400)));
        assert_eq!(connect_limit(ms(499)), None, "too little time to judge");
        assert_eq!(connect_limit(Duration::ZERO), None);
    }

    /// A port that takes connections but never answers TLS: a connect to it hangs.
    fn silent_tls() -> (std::net::TcpListener, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        (listener, url)
    }

    /// Work between building the client and the first request (a slow build, a `Stop`'s
    /// local work) leaves the connect's deadline its lead over the request's.
    #[tokio::test(start_paused = true)]
    async fn local_work_before_the_first_request_keeps_the_connects_lead() {
        let (_listener, url) = silent_tls();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
        let connects = std::sync::Arc::new(ConnectClock::default());
        connects.set_deadline(deadline);
        let client = direct_client(&url, "token".into(), None, &connects).unwrap();
        tokio::time::sleep(Duration::from_millis(450)).await;
        let err = tokio::time::timeout_at(deadline, client.health())
            .await
            .expect("the connect fails before the request's deadline")
            .unwrap_err();
        assert_eq!(Failure::of(&err), Failure::Unreachable, "{err:#}");
        let lead = deadline - tokio::time::Instant::now();
        assert!(lead >= CONNECT_LEAD, "lead {lead:?}");
        assert!(!connects.unjudged());
    }

    /// With too little left as it starts, a connect that hangs ends with the request.
    #[tokio::test(start_paused = true)]
    async fn a_connect_with_too_little_time_left_ends_with_the_request() {
        let (_listener, url) = silent_tls();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
        let connects = std::sync::Arc::new(ConnectClock::default());
        connects.set_deadline(deadline);
        let client = direct_client(&url, "token".into(), None, &connects).unwrap();
        tokio::time::sleep(Duration::from_millis(550)).await;
        let answer = tokio::time::timeout_at(deadline, client.health()).await;
        assert!(answer.is_err(), "{answer:?}");
        assert!(
            connects.unjudged(),
            "a slow answer then says nothing about the server"
        );
    }

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
    fn titles_are_checked_at_the_first_stop_then_hourly() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 6, 12, 0, 0).unwrap();
        assert!(title_check_due(None, t0), "the session's first Stop checks");
        assert!(!title_check_due(
            Some(t0),
            t0 + chrono::Duration::minutes(59)
        ));
        assert!(title_check_due(
            Some(t0),
            t0 + chrono::Duration::minutes(60)
        ));
        assert!(title_check_due(Some(t0), t0 - chrono::Duration::seconds(5)));
    }

    #[test]
    fn only_a_changed_title_or_link_is_sent() {
        let link = "https://app.example.test/threads/env-1/thread-1";
        let read = |title: Option<&str>, link: Option<&str>| ThreadFacts {
            parent: None,
            title: title.map(str::to_owned),
            link: link.map(str::to_owned),
        };
        // Unchanged: nothing to send.
        assert!(!thread_changed(
            Some("Fix the parser"),
            Some(link),
            &read(Some("Fix the parser"), Some(link))
        ));
        // T3 renamed the thread after its first turn.
        assert!(thread_changed(
            Some("New thread"),
            Some(link),
            &read(Some("Fix the parser"), Some(link))
        ));
        // Nothing registered yet (T3 did not answer at SessionStart).
        assert!(thread_changed(
            None,
            None,
            &read(Some("Fix the parser"), None)
        ));
        assert!(thread_changed(None, None, &read(None, Some(link))));
        // A field T3 left out is no change: the server keeps what it has.
        assert!(!thread_changed(
            Some("Fix the parser"),
            Some(link),
            &read(None, None)
        ));
        assert!(!thread_changed(None, None, &ThreadFacts::default()));
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
    fn a_passing_subagent_gets_its_own_hold_line_in_place_of_the_servers() {
        let boards = ["factory".parse().unwrap()];
        let text = subagent_hold(
            "<board>\nhold: factory paused -> stop\nusage: 5h 8%\n</board>",
            &boards,
            false,
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(lines[1].starts_with("hold: factory paused -> the turn that launched you"));
        assert!(!text.contains("-> stop"), "{text}");
        // A server text the hook does not recognize keeps its lines, after the hook's own.
        let text = subagent_hold("<board>\nHOLD factory\n</board>", &boards, false);
        assert!(
            text.starts_with("hold: factory paused -> the turn"),
            "{text}"
        );
        assert!(text.ends_with("<board>\nHOLD factory\n</board>"), "{text}");
        let text = subagent_hold(
            "<board>\nhold: factory: weekly usage\n</board>",
            &boards,
            true,
        );
        assert!(
            text.contains("hold: factory held: the weekly usage reached the user's reserve -> "),
            "{text}"
        );
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
