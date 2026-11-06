//! `board hook <event> --runtime claude|codex`: reads the runtime's hook JSON from stdin,
//! answers within 1.5 seconds and never fails the turn.
//!
//! Output shapes (Claude Code: code.claude.com/docs/en/hooks; Codex:
//! learn.chatgpt.com/docs/hooks.md):
//! - SessionStart, UserPromptSubmit, PostToolUse: `{"hookSpecificOutput": {"hookEventName":
//!   <event>, "additionalContext": <text>}}` on both runtimes.
//! - Stop: `{"decision": "block", "reason": <text>}` continues the turn with the reason;
//!   Codex requires JSON on stdout, so it gets `{}` otherwise.
//! - PreCompact (Claude Code): plain stdout becomes compaction instructions.
//! - StopFailure and SessionEnd: output is ignored.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use board_proto::{
    AgentId, AgentState, AgentStateUpdate, Runtime, Snapshot, SnapshotFormat, SnapshotKind,
    TaskStatus, Timestamp, Touches,
};
use chrono::{DateTime, Local, NaiveTime, TimeZone, Utc};
use clap::ValueEnum;
use serde_json::{Value, json};

use crate::client::{Client, SnapshotQuery, TaskQuery};
use crate::config::{Config, Home, load_token};
use crate::identity::{RepoInfo, T3_THREAD_ENV, nested_here, resolve_agent, t3_identity_applies};
use crate::render;
use crate::state::{self, AgentLocal};

/// Hard limit for the whole hook, start to output.
const BUDGET: Duration = Duration::from_millis(1500);
/// Kept free after the network work for writing state and output.
const MARGIN: Duration = Duration::from_millis(200);
/// After the server fails to answer, hooks on this device use the cache for this long
/// instead of waiting on it again.
const OFFLINE_BACKOFF: Duration = Duration::from_secs(60);
/// Heartbeats and alert checks from `PostToolUse` happen at most this often.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookEvent {
    #[value(alias = "SessionStart")]
    SessionStart,
    #[value(alias = "UserPromptSubmit")]
    UserPromptSubmit,
    #[value(alias = "PostToolUse")]
    PostToolUse,
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
            Self::PostToolUse => "PostToolUse",
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
            Self::PostToolUse => "post-tool-use",
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
    runtime: Runtime,
    /// The session runs inside another agent session (`identity::is_nested`).
    nested: bool,
    home: Home,
    device: board_proto::DeviceName,
    client: Client,
    agent: AgentId,
    input: &'a Value,
    local: AgentLocal,
    now: Timestamp,
}

async fn handle(
    event: HookEvent,
    runtime: Runtime,
    explicit_agent: Option<&str>,
    input: &Value,
    started: Instant,
) -> Result<Reply> {
    let home = Home::from_env()?;
    let config = Config::load(&home)?;
    let Ok((server, device)) = config.require_login() else {
        return Ok(Reply::Nothing);
    };
    let session = input["session_id"].as_str().filter(|s| !s.is_empty());
    let env = |name: &str| std::env::var(name).ok();
    // The Keychain lookup runs a process: keep it off the async threads. It runs while
    // the process table is read below.
    let token_home = home.clone();
    let token = tokio::task::spawn_blocking(move || load_token(&token_home));
    let nested = nested_session(&home, event, runtime, session, explicit_agent).await?;
    let Some(agent) = resolve_agent(explicit_agent, env, session.map(|s| (runtime, s)), nested)?
    else {
        return Ok(Reply::Nothing);
    };
    let Some(token) = token.await?? else {
        return Ok(Reply::Nothing);
    };
    let remaining = BUDGET.saturating_sub(started.elapsed() + MARGIN);
    let client = Client::new(server, token, Some(agent.clone()), Some(remaining))?;
    let mut hook = Hook {
        event,
        runtime,
        nested,
        device: device.clone(),
        client,
        local: state::load_agent(&home, &agent),
        home,
        agent,
        input,
        now: Utc::now(),
    };
    let offline_until = state::load_offline_until(&hook.home);
    if offline_until.is_some_and(|until| until > hook.now) {
        let reply = hook.offline();
        state::save_agent(&hook.home, &hook.agent, &hook.local)?;
        return Ok(reply);
    }
    let unreachable = |hook: &mut Hook<'_>| -> Result<Reply> {
        let backoff = chrono::Duration::from_std(OFFLINE_BACKOFF)?;
        state::save_offline_until(&hook.home, Some(hook.now + backoff))?;
        Ok(hook.offline())
    };
    let reply = match tokio::time::timeout(remaining, hook.online()).await {
        Ok(Ok(reply)) => {
            hook.local.offline_notified = false;
            if offline_until.is_some() {
                state::save_offline_until(&hook.home, None)?;
            }
            reply
        }
        // The server answered with an error: it is reachable, so the cache is no better.
        Ok(Err(err)) if crate::client::error_code(&err).is_some() => {
            tracing::debug!("hook {}: {err:#}", event.name());
            Reply::Nothing
        }
        Ok(Err(err)) => {
            tracing::debug!("hook {}: {err:#}", event.name());
            unreachable(&mut hook)?
        }
        Err(_) => unreachable(&mut hook)?,
    };
    state::save_agent(&hook.home, &hook.agent, &hook.local)?;
    Ok(reply)
}

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
    let nested = tokio::task::spawn_blocking(nested_here).await?;
    // Nothing to remember at the end of a session.
    if let Some(session) = session.filter(|_| event != HookEvent::SessionEnd) {
        state::save_session(home, runtime, session, &state::SessionLocal { nested })?;
    }
    Ok(nested)
}

impl Hook<'_> {
    fn cwd(&self) -> PathBuf {
        self.input["cwd"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    async fn online(&mut self) -> Result<Reply> {
        match self.event {
            HookEvent::SessionStart => self.session_start().await,
            HookEvent::UserPromptSubmit => self.user_prompt_submit().await,
            HookEvent::PostToolUse => self.post_tool_use().await,
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
    fn offline(&mut self) -> Reply {
        match self.event {
            HookEvent::SessionStart => self.cached_snapshot(),
            HookEvent::UserPromptSubmit if !self.local.offline_notified => self.cached_snapshot(),
            HookEvent::PreCompact => self.compact_instructions(None),
            _ => Reply::Nothing,
        }
    }

    fn cached_snapshot(&mut self) -> Reply {
        let Some(snap) = state::load_cached_snapshot(&self.home, &self.agent) else {
            return Reply::Nothing;
        };
        if snap.text.is_empty() {
            return Reply::Nothing;
        }
        self.local.offline_notified = true;
        Reply::Context(format!(
            "board server unreachable: cached snapshot from {} (seq {}); claims and updates fail until it is back.\n{}",
            render::ago(snap.at),
            snap.seq,
            snap.text
        ))
    }

    async fn set_state(&self, state: AgentState, paused_until: Option<Timestamp>) -> Result<()> {
        self.client
            .set_state(
                &self.agent,
                &AgentStateUpdate {
                    state,
                    paused_until,
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

    async fn session_start(&mut self) -> Result<Reply> {
        let repo = detect_repo(self.cwd()).await?;
        let mut native = std::collections::BTreeMap::new();
        if let Some(session) = self.input["session_id"].as_str() {
            let key = match self.runtime {
                Runtime::Claude => "claude_session",
                Runtime::Codex => "codex_session",
            };
            native.insert(key.to_owned(), session.to_owned());
        }
        if let Ok(thread) = std::env::var(T3_THREAD_ENV)
            && !self.nested
        {
            native.insert("t3_thread".to_owned(), thread);
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
        self.local.turn_started = None;
        self.local.reminded_turn = None;
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
        self.local.turn_started = Some(self.now);
        // After an outage, catch up with a full snapshot instead of a delta.
        let kind = if self.local.offline_notified {
            SnapshotKind::Full
        } else {
            SnapshotKind::Delta
        };
        let heartbeat = self.client.set_state(
            &self.agent,
            &AgentStateUpdate {
                state: AgentState::Running,
                paused_until: None,
            },
        );
        let (beat, snap) = tokio::join!(heartbeat, self.fetch(kind));
        beat?;
        let text = self.keep(snap?)?;
        Ok(if text.is_empty() {
            Reply::Nothing
        } else {
            Reply::Context(text)
        })
    }

    async fn post_tool_use(&mut self) -> Result<Reply> {
        let tool = self.input["tool_name"].as_str().unwrap_or_default();
        let mut lines = Vec::new();
        let paths = edited_paths(tool, &self.input["tool_input"]);
        if !paths.is_empty() {
            let repo = detect_repo(self.cwd()).await?;
            if let Some(key) = repo.key.clone() {
                let rel: Vec<String> = paths.iter().filter_map(|p| repo.relative(p)).collect();
                if !rel.is_empty() {
                    let result = self
                        .client
                        .touches(&Touches {
                            agent: self.agent.clone(),
                            repo: key,
                            paths: rel,
                        })
                        .await?;
                    lines.extend(render::overlaps(&result));
                }
            }
        }
        if heartbeat_due(self.local.last_heartbeat, self.now) {
            self.local.last_heartbeat = Some(self.now);
            self.set_state(AgentState::Running, None).await?;
            let alerts = self.snapshot(SnapshotKind::Alerts).await?;
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

    async fn stop(&mut self) -> Result<Reply> {
        self.set_state(AgentState::Idle, None).await?;
        let continued = self.input["stop_hook_active"].as_bool().unwrap_or(false);
        let turn = self.local.turn_started;
        if continued || (self.local.reminded_turn.is_some() && self.local.reminded_turn == turn) {
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
        let Some(task) = doing.first() else {
            return Ok(Reply::Nothing);
        };
        let handoff = self.client.handoff(&self.agent).await?;
        let stale = match (&handoff, turn) {
            (None, _) => true,
            (Some(h), Some(turn)) => h.updated_at < turn,
            (Some(_), None) => false,
        };
        if !stale {
            return Ok(Reply::Nothing);
        }
        self.local.reminded_turn = Some(turn.unwrap_or(self.now));
        Ok(Reply::Block(format!(
            "board: {} is in doing and your handoff is older than this turn. Call handoff_set (now, next, blockers) so the next session can resume, then stop.",
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
    }
}
