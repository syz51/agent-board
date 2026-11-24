//! Runtime-agnostic execution boundary for `board watch` (`docs/SCHEDULER_DESIGN.md`,
//! Section 4.3).
//!
//! Replaces direct coupling to T3-specific CLI types in the daemon's dispatch, wake, and
//! cancellation paths with a 6-method [`Runner`] trait (`kind` plus 5 async lifecycle
//! methods: `spawn`, `send`, `status`, `cancel`, `poll_events`).
//!
//! Concrete implementations in this module:
//! - [`T3Runner`]: wraps any [`crate::t3::T3`] (including the official unmodified `t3` CLI
//!   via [`crate::t3::T3Cli`]) behind [`Runner`].
//! - [`FakeRunner`]: in-memory runner with injectable clock and Trail Rule cancel recording,
//!   for daemon and scheduler tests across Slices 2 and 3.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context as _, Result, bail};
use board_proto::{AgentId, AgentSource, Runtime, Timestamp};
use chrono::Utc;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::config::{Home, ThreadModel};
use crate::state;
use crate::t3::{NewWorktree, T3, TimedOut};

pub use crate::acp_runner::AcpRunner;

/// Injectable clock for timestamping runner events and Trail Rule records.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

/// Default wall-clock implementation (`Utc::now`).
pub fn system_clock() -> Clock {
    Arc::new(Utc::now)
}

/// Maximum summary length in characters for a [`RunEvent`] (`docs/SCHEDULER_DESIGN.md`,
/// Section 5.2).
pub const RUN_EVENT_SUMMARY_MAX: usize = 300;

/// Everything a [`Runner`] needs to start one execution attempt in `workdir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSpec {
    /// Stable identifier for this run attempt (used for worktree paths, logs, and Trail Rule
    /// records).
    pub run_id: String,
    /// Human-readable title (for example `<task-id> <title>` or a judge's title).
    pub title: String,
    /// Initial prompt / brief delivered to the agent on start.
    pub prompt: String,
    /// Optional provider/model selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ThreadModel>,
    /// Optional git worktree specification for runners that create their own worktree inside
    /// a registered project (specifically [`T3Runner`], because `t3 thread start` requires a
    /// path inside a registered T3 project and exposes only `--worktree <base> --branch <branch>`).
    /// Non-T3 runners receive a daemon-owned worktree path directly as `workdir` and leave
    /// this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<NewWorktree>,
    /// Optional structured metadata (board, task ids, run kind, etc.) for telemetry and Trail
    /// Rule records.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// Handle identifying one runner-managed execution attempt across `send`, `status`,
/// `cancel`, and `poll_events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunHandle {
    /// Runner kind that owns this handle (`"t3"`, `"acp"`, `"deterministic"`, `"fake"`, ...).
    pub runner: String,
    /// Runner-native session or thread identifier (for T3, the thread id).
    pub native_id: String,
    /// Working directory associated with the run, when known (worktree or project folder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<PathBuf>,
    /// Optional human-facing URL (for example a T3 thread `https://` link).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Explicit board [`AgentId`] when it cannot be derived directly from `runner` and
    /// `native_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
}

/// Maps a [`Runner::kind`] identifier to the board [`AgentSource`] prefix used for its
/// handles (`"t3"` -> `T3`, `"acp"` / `"antigravity"` -> `Antigravity`, etc.).
pub fn runner_agent_source(runner_kind: &str) -> AgentSource {
    match runner_kind {
        "t3" => AgentSource::T3,
        "claude" => AgentSource::Claude,
        "codex" => AgentSource::Codex,
        "opencode" => AgentSource::Opencode,
        "antigravity" | "acp" => AgentSource::Antigravity,
        _ => AgentSource::T3,
    }
}

fn runner_source_prefix(runner_kind: &str) -> &'static str {
    match runner_agent_source(runner_kind) {
        AgentSource::T3 => "t3",
        AgentSource::Claude => "claude",
        AgentSource::Codex => "codex",
        AgentSource::Opencode => "opencode",
        AgentSource::Antigravity => "antigravity",
        AgentSource::Dispatch => "dispatch",
    }
}

impl RunHandle {
    /// Constructs a handle for `runner` and `native_id`.
    pub fn new(runner: impl Into<String>, native_id: impl Into<String>) -> Self {
        Self {
            runner: runner.into(),
            native_id: native_id.into(),
            workdir: None,
            link: None,
            agent_id: None,
        }
    }

    /// Constructs a handle for a T3 thread id.
    pub fn t3(thread_id: impl Into<String>) -> Self {
        Self::new("t3", thread_id)
    }

    /// Attaches a working directory to the handle.
    #[must_use]
    pub fn with_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }

    /// Attaches an explicit board [`AgentId`].
    #[must_use]
    pub fn with_agent_id(mut self, agent_id: AgentId) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    /// Resolves the board [`AgentId`] corresponding to this handle.
    ///
    /// Uses [`Self::agent_id`] when explicitly set, else ALWAYS qualifies `native_id` with
    /// the source prefix for `runner` (`t3` -> `t3:`, `acp` -> `antigravity:`, etc.) so a
    /// colon-containing `native_id` (such as `dispatch:...` or `claude:...`) can never spoof
    /// another [`AgentSource`].
    pub fn resolve_agent_id(&self) -> Result<AgentId> {
        if let Some(agent_id) = &self.agent_id {
            return Ok(agent_id.clone());
        }
        let source = runner_source_prefix(&self.runner);
        format!("{source}:{}", self.native_id)
            .parse::<AgentId>()
            .with_context(|| {
                format!(
                    "runner {:?} handle {:?} makes no valid agent id",
                    self.runner, self.native_id
                )
            })
    }
}

/// A follow-up message (wake-up, re-brief, or human steer) sent to a running or resumable
/// execution via [`Runner::send`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerMessage {
    pub text: String,
    /// Optional caller-assigned digest or correlation identifier (for example a wake-up
    /// digest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

impl SteerMessage {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            digest: None,
        }
    }

    #[must_use]
    pub fn with_digest(mut self, digest: impl Into<String>) -> Self {
        self.digest = Some(digest.into());
        self
    }
}

impl From<String> for SteerMessage {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl From<&str> for SteerMessage {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

/// High-level lifecycle state reported by [`Runner::status`]. Declared as an open enum
/// (`#[serde(other)] Unknown`) for forward compatibility (`docs/SCHEDULER_DESIGN.md`,
/// Section 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerState {
    Starting,
    Running,
    Idle,
    WaitingExternal,
    Succeeded,
    Failed,
    Cancelled,
    Gone,
    #[serde(other)]
    Unknown,
}

/// Point-in-time status of a run reported by [`Runner::status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerStatus {
    pub state: RunnerState,
    /// Whether the runner can still accept a [`Runner::send`] turn (not archived, cancelled,
    /// gone, or missing its working directory).
    pub runnable: bool,
    /// Whether a turn is currently active or queued.
    pub busy: bool,
    /// Agent runtime reported by the runner, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Runtime>,
    /// Working directory reported by the runner, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<PathBuf>,
    /// Total turns/runs executed so far, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs: Option<u64>,
}

/// Open/extensible kind for [`RunEvent`] (`docs/SCHEDULER_DESIGN.md`, Sections 5.2 & 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEventKind {
    Phase,
    Tool,
    Note,
    Message,
    Pr,
    Error,
    Cancelled,
    Done,
    #[serde(other)]
    Unknown,
}

/// Compact structured progress event returned by [`Runner::poll_events`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    pub seq: u64,
    pub at: Timestamp,
    pub kind: RunEventKind,
    pub summary: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// Returned when a runner (such as [`T3Runner`], whose official `t3` CLI has no cancel or
/// interrupt subcommand) cannot forcefully terminate a live run after recording the Trail
/// Rule entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelUnsupported {
    pub runner: String,
    pub native_id: String,
    pub reason: String,
    pub trail_path: PathBuf,
}

impl fmt::Display for CancelUnsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "runner `{}` cannot cancel live run `{}` (the official CLI has no cancel/interrupt \
             subcommand; reason: {}); cancel request recorded in {}. Stop or archive the session \
             directly in the runner UI.",
            self.runner,
            self.native_id,
            self.reason,
            self.trail_path.display()
        )
    }
}

impl std::error::Error for CancelUnsupported {}

/// Runtime-agnostic 6-method execution boundary (`docs/SCHEDULER_DESIGN.md`, Section 4.3).
///
/// Uses `BoxFuture` (matching [`crate::gh::Gh`]) so heterogeneous runners can be stored
/// behind `Box<dyn Runner>` or `Arc<dyn Runner>`.
pub trait Runner: Send + Sync {
    /// Runner kind identifier (`"t3"`, `"acp"`, `"deterministic"`, `"tmux"`, `"fake"`, ...).
    fn kind(&self) -> &'static str;

    /// Starts a new run for `spec` inside `workdir` and returns its [`RunHandle`].
    fn spawn<'a>(
        &'a self,
        spec: &'a RunSpec,
        workdir: &'a Path,
    ) -> BoxFuture<'a, Result<RunHandle>>;

    /// Delivers a follow-up or steering message to an existing run.
    fn send<'a>(
        &'a self,
        handle: &'a RunHandle,
        msg: &'a SteerMessage,
    ) -> BoxFuture<'a, Result<()>>;

    /// Reads the run's current lifecycle state and runnability.
    fn status<'a>(&'a self, handle: &'a RunHandle) -> BoxFuture<'a, Result<RunnerStatus>>;

    /// Cancels or aborts an active run, recording a Trail Rule entry with `reason` first.
    fn cancel<'a>(&'a self, handle: &'a RunHandle, reason: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Returns normalized [`RunEvent`]s with `seq > after_seq`, oldest first.
    fn poll_events<'a>(
        &'a self,
        handle: &'a RunHandle,
        after_seq: u64,
    ) -> BoxFuture<'a, Result<Vec<RunEvent>>>;
}

/// Clips `text` to [`RUN_EVENT_SUMMARY_MAX`] characters, collapsing whitespace into single
/// spaces so event summaries remain single-line and bounded.
pub fn clip_event_summary(text: &str) -> String {
    let flat = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if flat.chars().count() <= RUN_EVENT_SUMMARY_MAX {
        return flat;
    }
    let mut cut: String = flat.chars().take(RUN_EVENT_SUMMARY_MAX - 1).collect();
    cut.push('…');
    cut
}

/// Extracts the 16-hex wake-up digest from a trailing `(wake-up <digest>)` line, if present.
pub fn extract_wake_digest(text: &str) -> Option<&str> {
    let last = text.lines().last()?.trim();
    let digest = last.strip_prefix("(wake-up ")?.strip_suffix(')')?;
    (!digest.is_empty() && !digest.chars().any(char::is_whitespace)).then_some(digest)
}

/// Writes a durable Trail Rule entry to `$BOARD_HOME/state/runner-cancels.jsonl`.
pub(crate) fn record_cancel_trail(
    home: &Home,
    at: Timestamp,
    runner: &str,
    handle: &RunHandle,
    reason: &str,
    outcome: &str,
    supported: bool,
) -> Result<PathBuf> {
    let record = serde_json::json!({
        "at": at,
        "runner": runner,
        "native_id": handle.native_id,
        "workdir": handle.workdir,
        "reason": reason,
        "supported": supported,
        "outcome": outcome,
    });
    state::append_runner_cancel(home, &record).context("recording runner cancellation trail entry")
}

/// Wraps any [`crate::t3::T3`] (typically [`crate::t3::T3Cli`]) behind the 6-method
/// [`Runner`] trait.
///
/// ## Worktree compatibility with the official `t3` CLI
///
/// Verified against `t3 thread start --help`: `t3 thread start` accepts `<project>` (a
/// registered T3 project id or a path inside a registered T3 project) and has no flag to
/// attach an external pre-created git worktree outside that project root (`t3 thread list
/// --project <project>` also scopes threads to the registered project). Therefore:
/// - When [`RunSpec::worktree`] is `Some(NewWorktree { base, branch })`, `T3Runner::spawn`
///   passes `--worktree <base> --branch <branch>` with `workdir` as the registered T3
///   project folder so T3 project registration and `t3 thread list --project` stay intact.
/// - When [`RunSpec::worktree`] is `None`, `T3Runner::spawn` starts directly in `workdir`
///   (for non-git folders or workdirs already registered in T3).
/// - Non-T3 runners (such as `AcpRunner`, `TmuxRunner`, and [`FakeRunner`]) always receive
///   daemon-owned worktree paths from [`crate::worktree::WorktreeManager`] with
///   `RunSpec::worktree = None`.
///
/// ## Cancellation
///
/// The official `t3` CLI exposes `list`, `start`, `send`, and `show` under `t3 thread`, with
/// no native `cancel` or `interrupt` subcommand. [`T3Runner::cancel`] checks whether the
/// thread is already gone/archived, writes a Trail Rule record to
/// `$BOARD_HOME/state/runner-cancels.jsonl` first, succeeds if the thread has already ended,
/// and returns [`CancelUnsupported`] if the thread is still live so callers never mistake a
/// no-op for a terminated turn.
/// Per-thread monotonic sequence cursor aligning T3's sliding [`crate::t3::RECENT_MESSAGES`]
/// window across polls so `after_seq` never stalls when older messages roll off the front.
#[derive(Debug, Default)]
struct PollCursor {
    next_seq: u64,
    /// `(fingerprint, event)` for the user messages seen in the previous poll window.
    window: Vec<(String, RunEvent)>,
}

fn message_fingerprint(msg: &crate::t3::UserMessage) -> String {
    let mut hash = Sha256::new();
    if let Some(run) = &msg.run {
        hash.update(run.as_bytes());
    }
    hash.update([0]);
    hash.update(msg.text.as_bytes());
    crate::config::hex(&hash.finalize())
}

/// Computes the largest `k` such that the last `k` fingerprints of `prev` match the first
/// `k` fingerprints of `curr`, aligning a sliding window without dropping or duplicating
/// entries.
fn window_overlap(prev: &[(String, RunEvent)], curr: &[String]) -> usize {
    let max = prev.len().min(curr.len());
    for k in (1..=max).rev() {
        let prev_tail = &prev[prev.len() - k..];
        let curr_head = &curr[..k];
        if prev_tail.iter().zip(curr_head).all(|((p, _), c)| p == c) {
            return k;
        }
    }
    0
}

pub struct T3Runner<T: T3 + 'static> {
    t3: Arc<T>,
    home: Home,
    clock: Clock,
    cursors: Mutex<BTreeMap<String, PollCursor>>,
}

impl<T: T3 + 'static> T3Runner<T> {
    pub fn new(t3: Arc<T>, home: Home) -> Self {
        Self {
            t3,
            home,
            clock: system_clock(),
            cursors: Mutex::new(BTreeMap::new()),
        }
    }

    /// Replaces the clock used for [`RunEvent`] timestamps and Trail Rule records.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn inner(&self) -> &Arc<T> {
        &self.t3
    }
}

impl<T: T3 + 'static> Runner for T3Runner<T> {
    fn kind(&self) -> &'static str {
        "t3"
    }

    fn spawn<'a>(
        &'a self,
        spec: &'a RunSpec,
        workdir: &'a Path,
    ) -> BoxFuture<'a, Result<RunHandle>> {
        Box::pin(async move {
            let thread_id = self
                .t3
                .start_thread(
                    workdir,
                    &spec.title,
                    &spec.prompt,
                    spec.worktree.as_ref(),
                    spec.model.as_ref(),
                )
                .await?;
            Ok(RunHandle {
                runner: self.kind().to_owned(),
                native_id: thread_id,
                workdir: Some(workdir.to_path_buf()),
                link: None,
                agent_id: None,
            })
        })
    }

    fn send<'a>(
        &'a self,
        handle: &'a RunHandle,
        msg: &'a SteerMessage,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.t3.send(&handle.native_id, &msg.text).await })
    }

    fn status<'a>(&'a self, handle: &'a RunHandle) -> BoxFuture<'a, Result<RunnerStatus>> {
        Box::pin(async move {
            let Some(info) = self.t3.thread_info(&handle.native_id).await? else {
                return Ok(RunnerStatus {
                    state: RunnerState::Gone,
                    runnable: false,
                    busy: false,
                    runtime: None,
                    workdir: None,
                    runs: None,
                });
            };
            let runnable = info.runnable(handle.workdir.as_deref());
            let busy = runnable && info.busy;
            let state = if !runnable {
                RunnerState::Gone
            } else if busy {
                RunnerState::Running
            } else {
                RunnerState::Idle
            };
            Ok(RunnerStatus {
                state,
                runnable,
                busy,
                runtime: Some(info.runtime),
                workdir: info.worktree,
                runs: info.runs,
            })
        })
    }

    fn cancel<'a>(&'a self, handle: &'a RunHandle, reason: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let now = (self.clock)();
            let already_ended = match self.t3.thread_info(&handle.native_id).await {
                Ok(None) => true,
                Ok(Some(info)) => !info.runnable(handle.workdir.as_deref()),
                Err(_) => false,
            };
            if already_ended {
                record_cancel_trail(
                    &self.home,
                    now,
                    self.kind(),
                    handle,
                    reason,
                    "already_ended",
                    false,
                )?;
                return Ok(());
            }
            let trail_path = record_cancel_trail(
                &self.home,
                now,
                self.kind(),
                handle,
                reason,
                "unsupported_by_cli",
                false,
            )?;
            Err(CancelUnsupported {
                runner: self.kind().to_owned(),
                native_id: handle.native_id.clone(),
                reason: reason.to_owned(),
                trail_path,
            }
            .into())
        })
    }

    fn poll_events<'a>(
        &'a self,
        handle: &'a RunHandle,
        after_seq: u64,
    ) -> BoxFuture<'a, Result<Vec<RunEvent>>> {
        Box::pin(async move {
            let Some(recent) = self.t3.recent_messages(&handle.native_id).await? else {
                return Ok(Vec::new());
            };
            let now = (self.clock)();
            let busy = if recent.busy { "true" } else { "false" };
            let fps: Vec<String> = recent.user.iter().map(message_fingerprint).collect();

            let mut cursors = self.cursors.lock().unwrap_or_else(PoisonError::into_inner);
            let cursor = cursors.entry(handle.native_id.clone()).or_default();
            if cursor.next_seq == 0 {
                // Start strictly after any persisted `after_seq` across daemon restarts so
                // post-restart T3 messages are never masked or dropped on seq collision.
                cursor.next_seq = after_seq.saturating_add(1).max(1);
            }
            let overlap = window_overlap(&cursor.window, &fps);
            let mut next_window = Vec::with_capacity(recent.user.len());
            if overlap > 0 {
                let start = cursor.window.len() - overlap;
                for (fp, mut ev) in cursor.window[start..].iter().cloned() {
                    ev.metadata.insert("busy".to_owned(), busy.to_owned());
                    next_window.push((fp, ev));
                }
            }
            for (fp, msg) in fps.into_iter().zip(recent.user).skip(overlap) {
                let seq = cursor.next_seq;
                cursor.next_seq += 1;
                let mut metadata = BTreeMap::new();
                metadata.insert("role".to_owned(), "user".to_owned());
                metadata.insert("busy".to_owned(), busy.to_owned());
                if let Some(run_id) = msg.run {
                    metadata.insert("run_id".to_owned(), run_id);
                }
                if let Some(digest) = extract_wake_digest(&msg.text) {
                    metadata.insert("wake_digest".to_owned(), digest.to_owned());
                }
                let event = RunEvent {
                    seq,
                    at: now,
                    kind: RunEventKind::Message,
                    summary: clip_event_summary(&msg.text),
                    metadata,
                };
                next_window.push((fp, event));
            }
            cursor.window = next_window;
            Ok(cursor
                .window
                .iter()
                .filter(|(_, ev)| ev.seq > after_seq)
                .map(|(_, ev)| ev.clone())
                .collect())
        })
    }
}

/// Recorded `Runner::spawn` invocation on [`FakeRunner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeSpawn {
    pub spec: RunSpec,
    pub workdir: PathBuf,
    pub handle: RunHandle,
}

/// Recorded `Runner::send` invocation on [`FakeRunner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeSend {
    pub handle: RunHandle,
    pub message: SteerMessage,
}

/// Recorded `Runner::cancel` invocation on [`FakeRunner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeCancel {
    pub handle: RunHandle,
    pub reason: String,
    pub at: Timestamp,
}

struct FakeRunnerState {
    next_id: u64,
    queued_ids: VecDeque<String>,
    fail_spawn: Option<String>,
    fail_send: Option<String>,
    /// When `Some(delivered)`, `send` returns [`TimedOut`] (after recording the event when
    /// `delivered` is true).
    time_out_send: Option<bool>,
    spawns: Vec<FakeSpawn>,
    sends: Vec<FakeSend>,
    cancels: Vec<FakeCancel>,
    statuses: BTreeMap<String, RunnerStatus>,
    events: BTreeMap<String, Vec<RunEvent>>,
}

impl Default for FakeRunnerState {
    fn default() -> Self {
        Self {
            next_id: 1,
            queued_ids: VecDeque::new(),
            fail_spawn: None,
            fail_send: None,
            time_out_send: None,
            spawns: Vec::new(),
            sends: Vec::new(),
            cancels: Vec::new(),
            statuses: BTreeMap::new(),
            events: BTreeMap::new(),
        }
    }
}

/// Full in-memory [`Runner`] implementation for Slice 2 and Slice 3 daemon/scheduler tests.
///
/// Supports spawning into daemon-owned worktrees, steering messages, customizable status
/// transitions, clean cancellation with Trail Rule persistence (when configured with a
/// [`Home`]), and event polling with `after_seq`.
#[derive(Clone)]
pub struct FakeRunner {
    kind: &'static str,
    home: Option<Home>,
    clock: Clock,
    inner: Arc<Mutex<FakeRunnerState>>,
}

impl Default for FakeRunner {
    fn default() -> Self {
        Self::new("fake")
    }
}

impl FakeRunner {
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            home: None,
            clock: system_clock(),
            inner: Arc::new(Mutex::new(FakeRunnerState::default())),
        }
    }

    /// Configures a `$BOARD_HOME` so [`Runner::cancel`] writes Trail Rule entries to
    /// `$BOARD_HOME/state/runner-cancels.jsonl`.
    #[must_use]
    pub fn with_home(mut self, home: Home) -> Self {
        self.home = Some(home);
        self
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Queues specific native ids to return from successive [`Runner::spawn`] calls.
    pub fn queue_ids<I, S>(&self, ids: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.queued_ids.extend(ids.into_iter().map(Into::into));
    }

    /// Causes [`Runner::spawn`] to fail with `msg` while `Some`.
    pub fn set_fail_spawn(&self, msg: Option<&str>) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.fail_spawn = msg.map(str::to_owned);
    }

    /// Causes [`Runner::send`] to fail with `msg` while `Some`.
    pub fn set_fail_send(&self, msg: Option<&str>) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.fail_send = msg.map(str::to_owned);
    }

    /// Causes [`Runner::send`] to return [`TimedOut`] while `Some(delivered)` (`true` records
    /// the message event first as if delivery succeeded before the timeout).
    pub fn set_time_out_send(&self, mode: Option<bool>) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.time_out_send = mode;
    }

    /// Sets the [`RunnerStatus`] reported for `native_id`.
    pub fn set_status(&self, native_id: &str, status: RunnerStatus) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.statuses.insert(native_id.to_owned(), status);
    }

    /// Appends a [`RunEvent`] for `native_id` with the next monotonic sequence number.
    pub fn push_event(
        &self,
        native_id: &str,
        kind: RunEventKind,
        summary: impl Into<String>,
        metadata: BTreeMap<String, String>,
    ) -> RunEvent {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let list = guard.events.entry(native_id.to_owned()).or_default();
        let seq = (list.len() + 1) as u64;
        let event = RunEvent {
            seq,
            at: (self.clock)(),
            kind,
            summary: clip_event_summary(&summary.into()),
            metadata,
        };
        list.push(event.clone());
        event
    }

    pub fn spawns(&self) -> Vec<FakeSpawn> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .spawns
            .clone()
    }

    pub fn sends(&self) -> Vec<FakeSend> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sends
            .clone()
    }

    pub fn cancels(&self) -> Vec<FakeCancel> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cancels
            .clone()
    }
}

impl Runner for FakeRunner {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn spawn<'a>(
        &'a self,
        spec: &'a RunSpec,
        workdir: &'a Path,
    ) -> BoxFuture<'a, Result<RunHandle>> {
        Box::pin(async move {
            let now = (self.clock)();
            let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(err) = &guard.fail_spawn {
                bail!("{err}");
            }
            let native_id = guard.queued_ids.pop_front().unwrap_or_else(|| {
                let n = guard.next_id;
                guard.next_id += 1;
                format!("{}-run-{n}", self.kind)
            });
            let handle = RunHandle {
                runner: self.kind.to_owned(),
                native_id: native_id.clone(),
                workdir: Some(workdir.to_path_buf()),
                link: None,
                agent_id: None,
            };
            guard.statuses.insert(
                native_id.clone(),
                RunnerStatus {
                    state: RunnerState::Running,
                    runnable: true,
                    busy: true,
                    runtime: Some(Runtime::Antigravity),
                    workdir: Some(workdir.to_path_buf()),
                    runs: Some(1),
                },
            );
            guard.events.entry(native_id).or_default().push(RunEvent {
                seq: 1,
                at: now,
                kind: RunEventKind::Phase,
                summary: clip_event_summary(&format!("spawned {}", spec.title)),
                metadata: BTreeMap::from([("run_id".to_owned(), spec.run_id.clone())]),
            });
            guard.spawns.push(FakeSpawn {
                spec: spec.clone(),
                workdir: workdir.to_path_buf(),
                handle: handle.clone(),
            });
            Ok(handle)
        })
    }

    fn send<'a>(
        &'a self,
        handle: &'a RunHandle,
        msg: &'a SteerMessage,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let now = (self.clock)();
            let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(err) = &guard.fail_send {
                bail!("{err}");
            }
            if let Some(status) = guard.statuses.get(&handle.native_id)
                && !status.runnable
            {
                bail!(
                    "run {} is not runnable ({:?})",
                    handle.native_id,
                    status.state
                );
            }
            guard.sends.push(FakeSend {
                handle: handle.clone(),
                message: msg.clone(),
            });
            let timed_out = guard.time_out_send;
            if timed_out != Some(false) {
                let list = guard.events.entry(handle.native_id.clone()).or_default();
                let seq = (list.len() + 1) as u64;
                let mut metadata = BTreeMap::new();
                if let Some(digest) = msg
                    .digest
                    .as_deref()
                    .or_else(|| extract_wake_digest(&msg.text))
                {
                    metadata.insert("wake_digest".to_owned(), digest.to_owned());
                }
                list.push(RunEvent {
                    seq,
                    at: now,
                    kind: RunEventKind::Message,
                    summary: clip_event_summary(&msg.text),
                    metadata,
                });
            }
            if timed_out.is_some() {
                return Err(TimedOut(format!("{} send {}", self.kind, handle.native_id)).into());
            }
            Ok(())
        })
    }

    fn status<'a>(&'a self, handle: &'a RunHandle) -> BoxFuture<'a, Result<RunnerStatus>> {
        Box::pin(async move {
            let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            Ok(guard
                .statuses
                .get(&handle.native_id)
                .cloned()
                .unwrap_or(RunnerStatus {
                    state: RunnerState::Gone,
                    runnable: false,
                    busy: false,
                    runtime: None,
                    workdir: None,
                    runs: None,
                }))
        })
    }

    fn cancel<'a>(&'a self, handle: &'a RunHandle, reason: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let now = (self.clock)();
            if let Some(home) = &self.home {
                record_cancel_trail(home, now, self.kind, handle, reason, "cancelled", true)?;
            }
            let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            guard.cancels.push(FakeCancel {
                handle: handle.clone(),
                reason: reason.to_owned(),
                at: now,
            });
            let entry = guard
                .statuses
                .entry(handle.native_id.clone())
                .or_insert(RunnerStatus {
                    state: RunnerState::Cancelled,
                    runnable: false,
                    busy: false,
                    runtime: None,
                    workdir: handle.workdir.clone(),
                    runs: None,
                });
            entry.state = RunnerState::Cancelled;
            entry.runnable = false;
            entry.busy = false;
            let list = guard.events.entry(handle.native_id.clone()).or_default();
            let seq = (list.len() + 1) as u64;
            list.push(RunEvent {
                seq,
                at: now,
                kind: RunEventKind::Cancelled,
                summary: clip_event_summary(reason),
                metadata: BTreeMap::from([("reason".to_owned(), reason.to_owned())]),
            });
            Ok(())
        })
    }

    fn poll_events<'a>(
        &'a self,
        handle: &'a RunHandle,
        after_seq: u64,
    ) -> BoxFuture<'a, Result<Vec<RunEvent>>> {
        Box::pin(async move {
            let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            Ok(guard
                .events
                .get(&handle.native_id)
                .map(|list| list.iter().filter(|e| e.seq > after_seq).cloned().collect())
                .unwrap_or_default())
        })
    }
}

/// Dispatches execution dynamically by runner kind (`"acp"`, `"t3"`, `"fake"`, ...) with a
/// clean fallback default so the pull scheduler can select runners per run (`docs/SCHEDULER_DESIGN.md`,
/// Section 4.3).
#[derive(Clone, Default)]
pub struct RunnerRegistry {
    runners: BTreeMap<String, Arc<dyn Runner>>,
    default_kind: Option<String>,
}

impl fmt::Debug for RunnerRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunnerRegistry")
            .field("runners", &self.runners.keys().collect::<Vec<_>>())
            .field("default_kind", &self.default_kind)
            .finish()
    }
}

impl RunnerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `runner` under its [`Runner::kind`]. The first registered runner becomes the
    /// default unless overridden via [`Self::set_default`] / [`Self::with_default`].
    pub fn register(&mut self, runner: Arc<dyn Runner>) -> &mut Self {
        let kind = runner.kind().to_owned();
        if self.default_kind.is_none() {
            self.default_kind = Some(kind.clone());
        }
        self.runners.insert(kind, runner);
        self
    }

    /// Builder-style variant of [`Self::register`].
    #[must_use]
    pub fn with_runner(mut self, runner: Arc<dyn Runner>) -> Self {
        self.register(runner);
        self
    }

    /// Sets the default runner kind; returns an error if `kind` is not registered.
    pub fn set_default(&mut self, kind: &str) -> Result<()> {
        if !self.runners.contains_key(kind) {
            bail!(
                "cannot set default runner to {:?}: not registered (available: {})",
                kind,
                self.available_summary()
            );
        }
        self.default_kind = Some(kind.to_owned());
        Ok(())
    }

    /// Builder-style variant of [`Self::set_default`].
    pub fn with_default(mut self, kind: &str) -> Result<Self> {
        self.set_default(kind)?;
        Ok(self)
    }

    /// Returns the runner registered for `kind`, if present.
    pub fn get(&self, kind: &str) -> Option<Arc<dyn Runner>> {
        self.runners.get(kind).cloned()
    }

    /// Returns the configured default runner, if any.
    pub fn default_runner(&self) -> Option<Arc<dyn Runner>> {
        self.default_kind
            .as_deref()
            .and_then(|kind| self.runners.get(kind))
            .cloned()
    }

    /// Name of the default runner kind, when set.
    pub fn default_kind(&self) -> Option<&str> {
        self.default_kind.as_deref()
    }

    /// Resolves `preferred` if `Some`, otherwise returns the default runner. Returns an
    /// actionable error when a requested runner is missing or the registry is empty.
    pub fn resolve(&self, preferred: Option<&str>) -> Result<Arc<dyn Runner>> {
        match preferred.map(str::trim).filter(|s| !s.is_empty()) {
            Some(kind) => self.get(kind).ok_or_else(|| {
                anyhow::anyhow!(
                    "runner {:?} is not registered on this device (available: {})",
                    kind,
                    self.available_summary()
                )
            }),
            None => self
                .default_runner()
                .ok_or_else(|| anyhow::anyhow!("no runners are registered on this device")),
        }
    }

    /// Resolves the runner that owns `handle`.
    pub fn resolve_for_handle(&self, handle: &RunHandle) -> Result<Arc<dyn Runner>> {
        self.resolve(Some(&handle.runner))
    }

    /// Sorted list of registered runner kinds (suitable for device heartbeats).
    pub fn kinds(&self) -> Vec<String> {
        self.runners.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.runners.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runners.is_empty()
    }

    fn available_summary(&self) -> String {
        if self.runners.is_empty() {
            "none".to_owned()
        } else {
            self.runners.keys().cloned().collect::<Vec<_>>().join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::t3::{ListedThread, RecentMessages, ThreadInfo, UserMessage};

    fn fixed_clock(ts: &str) -> Clock {
        let parsed: Timestamp = ts.parse().unwrap();
        Arc::new(move || parsed)
    }

    struct StubStart {
        _project: PathBuf,
        _title: String,
        _message: String,
        _worktree: Option<NewWorktree>,
        _model: Option<ThreadModel>,
    }

    #[derive(Default)]
    struct StubT3 {
        started: Mutex<Vec<StubStart>>,
        sent: Mutex<Vec<(String, String)>>,
        info: Mutex<BTreeMap<String, ThreadInfo>>,
        recent: Mutex<BTreeMap<String, RecentMessages>>,
    }

    impl T3 for StubT3 {
        async fn start_thread(
            &self,
            project: &Path,
            title: &str,
            message: &str,
            worktree: Option<&NewWorktree>,
            model: Option<&ThreadModel>,
        ) -> Result<String> {
            self.started.lock().unwrap().push(StubStart {
                _project: project.to_path_buf(),
                _title: title.to_owned(),
                _message: message.to_owned(),
                _worktree: worktree.cloned(),
                _model: model.cloned(),
            });
            Ok("thread:demo:1".to_owned())
        }

        async fn send(&self, thread: &str, message: &str) -> Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push((thread.to_owned(), message.to_owned()));
            Ok(())
        }

        async fn thread_info(&self, thread: &str) -> Result<Option<ThreadInfo>> {
            Ok(self.info.lock().unwrap().get(thread).cloned())
        }

        async fn recent_messages(&self, thread: &str) -> Result<Option<RecentMessages>> {
            Ok(self.recent.lock().unwrap().get(thread).cloned())
        }

        async fn thread_link(&self, _: &str) -> Result<Option<String>> {
            Ok(None)
        }

        async fn threads_titled(&self, _: &Path, _: &str) -> Result<Vec<ListedThread>> {
            Ok(Vec::new())
        }

        async fn running_threads(&self, _: &Path) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn t3_runner_adapts_spawn_send_status_poll_and_cancel_with_trail_record() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let stub = Arc::new(StubT3::default());
        stub.info.lock().unwrap().insert(
            "thread:demo:1".to_owned(),
            ThreadInfo {
                archived: false,
                runtime: Runtime::Antigravity,
                worktree: None,
                runs: Some(2),
                busy: true,
            },
        );
        stub.recent.lock().unwrap().insert(
            "thread:demo:1".to_owned(),
            RecentMessages {
                busy: true,
                user: vec![
                    UserMessage {
                        text: "first prompt".to_owned(),
                        run: Some("r-1".to_owned()),
                    },
                    UserMessage {
                        text: "wake line\n(wake-up deadbeef01234567)".to_owned(),
                        run: None,
                    },
                ],
            },
        );

        let runner = T3Runner::new(Arc::clone(&stub), home.clone())
            .with_clock(fixed_clock("2026-11-24T10:00:00Z"));
        assert_eq!(runner.kind(), "t3");

        let spec = RunSpec {
            run_id: "run-1".to_owned(),
            title: "factory-1 Fix parser".to_owned(),
            prompt: "Do the task".to_owned(),
            model: None,
            worktree: Some(NewWorktree {
                base: "origin/main".to_owned(),
                branch: "task/factory-1".to_owned(),
            }),
            metadata: BTreeMap::new(),
        };
        let handle = runner.spawn(&spec, &project).await.unwrap();
        assert_eq!(handle.runner, "t3");
        assert_eq!(handle.native_id, "thread:demo:1");
        assert_eq!(
            handle.resolve_agent_id().unwrap().as_str(),
            "t3:thread:demo:1"
        );

        runner
            .send(&handle, &SteerMessage::new("continue please"))
            .await
            .unwrap();
        assert_eq!(
            stub.sent.lock().unwrap().as_slice(),
            &[("thread:demo:1".to_owned(), "continue please".to_owned())]
        );

        let status = runner.status(&handle).await.unwrap();
        assert!(status.runnable);
        assert!(status.busy);
        assert_eq!(status.state, RunnerState::Running);
        assert_eq!(status.runtime, Some(Runtime::Antigravity));
        assert_eq!(status.runs, Some(2));
        stub.info
            .lock()
            .unwrap()
            .get_mut("thread:demo:1")
            .unwrap()
            .busy = false;
        let idle_status = runner.status(&handle).await.unwrap();
        assert!(idle_status.runnable);
        assert!(!idle_status.busy);
        assert_eq!(idle_status.state, RunnerState::Idle);

        let events = runner.poll_events(&handle, 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 1);
        assert_eq!(events[0].kind, RunEventKind::Message);
        assert_eq!(
            events[0].metadata.get("run_id").map(String::as_str),
            Some("r-1")
        );
        assert_eq!(
            events[0].metadata.get("busy").map(String::as_str),
            Some("true")
        );
        assert_eq!(events[1].seq, 2);
        assert_eq!(
            events[1].metadata.get("wake_digest").map(String::as_str),
            Some("deadbeef01234567")
        );
        // Pagination via `after_seq`.
        let tail = runner.poll_events(&handle, 1).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 2);

        // Cancel on a live thread writes a Trail Rule record first and returns CancelUnsupported.
        let err = runner.cancel(&handle, "preempted by p1").await.unwrap_err();
        let unsupported = err
            .downcast_ref::<CancelUnsupported>()
            .expect("CancelUnsupported error");
        assert_eq!(unsupported.runner, "t3");
        assert_eq!(unsupported.native_id, "thread:demo:1");
        assert!(unsupported.trail_path.exists());
        let trail = std::fs::read_to_string(&unsupported.trail_path).unwrap();
        let lines: Vec<serde_json::Value> = trail
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["at"], "2026-11-24T10:00:00Z");
        assert_eq!(lines[0]["runner"], "t3");
        assert_eq!(lines[0]["native_id"], "thread:demo:1");
        assert_eq!(lines[0]["reason"], "preempted by p1");
        assert_eq!(lines[0]["outcome"], "unsupported_by_cli");

        // Cancel on an already-archived thread succeeds and still writes a Trail Rule record.
        stub.info
            .lock()
            .unwrap()
            .get_mut("thread:demo:1")
            .unwrap()
            .archived = true;
        runner
            .cancel(&handle, "cleanup after archive")
            .await
            .unwrap();
        let trail = std::fs::read_to_string(&unsupported.trail_path).unwrap();
        let lines: Vec<serde_json::Value> = trail
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["outcome"], "already_ended");
    }

    #[tokio::test]
    async fn fake_runner_supports_full_lifecycle_and_cancel_trail() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let runner = FakeRunner::new("acp")
            .with_home(home.clone())
            .with_clock(fixed_clock("2026-11-24T11:30:00Z"));
        assert_eq!(runner.kind(), "acp");

        let spec = RunSpec {
            run_id: "run-9".to_owned(),
            title: "factory-9 Build runner".to_owned(),
            prompt: "Implement it".to_owned(),
            model: None,
            worktree: None,
            metadata: BTreeMap::new(),
        };
        let workdir = dir.path().join("wt");
        let handle = runner.spawn(&spec, &workdir).await.unwrap();
        assert_eq!(handle.native_id, "acp-run-1");
        assert_eq!(
            handle.resolve_agent_id().unwrap().as_str(),
            "antigravity:acp-run-1"
        );

        let status = runner.status(&handle).await.unwrap();
        assert_eq!(status.state, RunnerState::Running);
        assert!(status.runnable);

        runner
            .send(
                &handle,
                &SteerMessage::new("focus on tests").with_digest("abc"),
            )
            .await
            .unwrap();
        runner.cancel(&handle, "user requested stop").await.unwrap();

        let status = runner.status(&handle).await.unwrap();
        assert_eq!(status.state, RunnerState::Cancelled);
        assert!(!status.runnable);
        assert!(
            runner
                .send(&handle, &SteerMessage::new("too late"))
                .await
                .is_err()
        );

        let events = runner.poll_events(&handle, 0).await.unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, RunEventKind::Phase);
        assert_eq!(events[1].kind, RunEventKind::Message);
        assert_eq!(events[2].kind, RunEventKind::Cancelled);
        assert_eq!(events[2].summary, "user requested stop");

        let trail_path = home.state_dir().join("runner-cancels.jsonl");
        let line: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&trail_path).unwrap().trim()).unwrap();
        assert_eq!(line["runner"], "acp");
        assert_eq!(line["native_id"], "acp-run-1");
        assert_eq!(line["outcome"], "cancelled");
        assert_eq!(line["supported"], true);
    }

    #[test]
    fn open_enums_deserialize_future_variants_as_unknown() {
        let state: RunnerState = serde_json::from_str("\"hibernated\"").unwrap();
        assert_eq!(state, RunnerState::Unknown);
        let kind: RunEventKind = serde_json::from_str("\"artifact_uploaded\"").unwrap();
        assert_eq!(kind, RunEventKind::Unknown);
    }

    #[test]
    fn event_summaries_collapse_whitespace_and_bound_length() {
        assert_eq!(clip_event_summary("  hello\n\tworld  "), "hello world");
        let long = "x".repeat(RUN_EVENT_SUMMARY_MAX + 50);
        let clipped = clip_event_summary(&long);
        assert_eq!(clipped.chars().count(), RUN_EVENT_SUMMARY_MAX);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn resolve_agent_id_always_prefixes_runner_source_for_colon_native_ids() {
        let t3_colon = RunHandle::t3("dispatch:mac-air")
            .resolve_agent_id()
            .unwrap();
        assert_eq!(t3_colon.source(), AgentSource::T3);
        assert_eq!(t3_colon.as_str(), "t3:dispatch:mac-air");

        let acp_colon = RunHandle::new("acp", "claude:s1")
            .resolve_agent_id()
            .unwrap();
        assert_eq!(acp_colon.source(), AgentSource::Antigravity);
        assert_eq!(acp_colon.as_str(), "antigravity:claude:s1");
    }

    #[tokio::test]
    async fn t3_runner_poll_events_keeps_monotonic_seq_across_sliding_and_shrinking_windows() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let stub = Arc::new(StubT3::default());
        let runner =
            T3Runner::new(Arc::clone(&stub), home).with_clock(fixed_clock("2026-11-24T12:00:00Z"));
        let handle = RunHandle::t3("thread:slide:1");

        let msg = |text: &str, run: &str| UserMessage {
            text: text.to_owned(),
            run: Some(run.to_owned()),
        };
        // Window 1: [m1, m2, m3] -> seq 1, 2, 3.
        stub.recent.lock().unwrap().insert(
            "thread:slide:1".to_owned(),
            RecentMessages {
                busy: false,
                user: vec![msg("m1", "r1"), msg("m2", "r2"), msg("m3", "r3")],
            },
        );
        let first = runner.poll_events(&handle, 0).await.unwrap();
        assert_eq!(
            first
                .iter()
                .map(|e| (e.seq, e.summary.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "m1"), (2, "m2"), (3, "m3")]
        );

        // Window 2 slides forward: [m2, m3, m4] -> only m4 is new, with seq 4!
        stub.recent.lock().unwrap().insert(
            "thread:slide:1".to_owned(),
            RecentMessages {
                busy: false,
                user: vec![msg("m2", "r2"), msg("m3", "r3"), msg("m4", "r4")],
            },
        );
        let second = runner.poll_events(&handle, 3).await.unwrap();
        assert_eq!(
            second
                .iter()
                .map(|e| (e.seq, e.summary.as_str()))
                .collect::<Vec<_>>(),
            vec![(4, "m4")]
        );

        // Window 3 shrinks due to assistant messages: [m4, m5] -> only m5 is new, with seq 5!
        stub.recent.lock().unwrap().insert(
            "thread:slide:1".to_owned(),
            RecentMessages {
                busy: true,
                user: vec![msg("m4", "r4"), msg("m5", "r5")],
            },
        );
        let third = runner.poll_events(&handle, 4).await.unwrap();
        assert_eq!(
            third
                .iter()
                .map(|e| (e.seq, e.summary.as_str()))
                .collect::<Vec<_>>(),
            vec![(5, "m5")]
        );
    }

    #[test]
    fn runner_registry_dispatches_by_kind_and_defaults_cleanly() {
        let empty = RunnerRegistry::new();
        assert!(empty.is_empty());
        assert!(empty.resolve(None).is_err());
        assert!(empty.resolve(Some("acp")).is_err());

        let acp: Arc<dyn Runner> = Arc::new(FakeRunner::new("acp"));
        let t3: Arc<dyn Runner> = Arc::new(FakeRunner::new("t3"));
        let registry = RunnerRegistry::new()
            .with_runner(Arc::clone(&acp))
            .with_runner(Arc::clone(&t3));
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.kinds(), vec!["acp", "t3"]);
        assert_eq!(registry.default_kind(), Some("acp"));
        assert_eq!(registry.resolve(None).unwrap().kind(), "acp");
        assert_eq!(registry.resolve(Some("t3")).unwrap().kind(), "t3");
        assert_eq!(registry.resolve(Some("acp")).unwrap().kind(), "acp");
        assert_eq!(
            registry
                .resolve_for_handle(&RunHandle::t3("thread-1"))
                .unwrap()
                .kind(),
            "t3"
        );

        let registry = registry.with_default("t3").unwrap();
        assert_eq!(registry.resolve(None).unwrap().kind(), "t3");
        assert!(registry.with_default("missing").is_err());
    }
}
