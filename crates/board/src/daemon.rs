//! `board watch`: the device daemon.
//!
//! It follows the server's event stream and, on this device:
//! - dispatches each autopilot board that lists this device in `home_devices`: it holds the
//!   board's coordinator lease as `dispatch:<device>` and starts a worker thread for each
//!   next ready task while one of the board's `parallel_tasks` slots is free (see
//!   [`Worker`]). No long-lived coordinator thread holds work;
//! - starts the board's judges (review, verdict, goal and triage), merges the pull request
//!   of a task whose review passed and finishes that task (see [`judges`]);
//! - on an active board that lists this device in `home_devices` and is not on autopilot,
//!   starts verdict judges for its pending proposals, and nothing else: no lease, no
//!   worker and no other judge;
//! - wakes the owner of a blocked task when its last blocker is done, the owner of a task
//!   in `doing` when the server announces it has been away from it (`idle_doing`), and the
//!   asker when its ask is answered;
//! - does itself what an agent no device can wake would be woken for (an answer to a gone
//!   asker, an owner still away at the repeat idle notice; see [`Owed`]);
//! - never wakes a paused agent before its `paused_until`;
//! - wakes no Claude agent while a window of a fresh usage reading is at 100 percent: their
//!   wake-ups are kept and sent once at that window's reset;
//! - wakes an ended agent like an idle one while T3 still has its thread: T3 closes a
//!   session left idle, which marks its agent ended, and a message resumes the thread. One
//!   whose session closed while it waited for a usage-limit reset keeps that reset, and
//!   waits for it like a paused agent.
//!
//! Wake-ups are queued as what they are about (a task, an ask), not as text. Each flush
//! checks them again before sending: one that no longer holds drops its line. Lines
//! for a running agent are held until its `agent_state_changed` event says it stopped, at
//! most [`WAKE_HOLD_MAX`]; a paused agent's are kept until its reset. A failed send backs
//! off, and a gone thread drops its lines. A send that timed out may have been delivered:
//! a flush looks for its message in the thread a minute later, by the marker it ends with,
//! and queues its lines again, once, only when the thread never got it.
//!
//! Wake-up messages contain only board-generated text and the user's own answers.
//!
//! A board where a worker cannot be started here is retried with a growing delay until its
//! repos, home devices, autopilot switch or status change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use board_proto::{
    AddNote, Agent, AgentId, AgentRole, AgentSource, AgentState, Board, BoardSlug, BoardStatus,
    ClaimCoordinator, ClaimedRun, DeviceHeartbeat, DeviceName, DispatchClaimRequest, ErrorCode,
    Event, EventType, JudgeKind, NewRunEvent, NoteKind, PushRunEvents, RUN_EVENTS_BATCH_MAX,
    RUN_LEASE_TTL_SECS_DEFAULT, RegisterAgent, RunEventKind as ProtoRunEventKind, RunId, RunKind,
    RunState, Runtime, StickyWake, Task, TaskId, TaskStatus, Timestamp, UpdateRun, UpdateTask,
    Usage, normalize_repo,
};
use chrono::{TimeDelta, Utc};
use eventsource_stream::Eventsource as _;
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::acp_runner::AcpRunner;
use crate::client::{Client, RelayFailure, ServerError, TaskQuery, error_code, is_unreachable};
use crate::config::{ArchiveConfig, Config, DispatchMode, Home};
use crate::deterministic_runner::{
    DETERMINISTIC_RUNNER_KIND, DeterministicRunner, HousekeepingOutcome, MergeOutcome,
    RealUpgradeDeployer, UpgradeDeployer, UpgradeRunOutput,
};
use crate::gh::{Gh, GhCli, PrState};
use crate::local_db::{
    LocalDb, LocalRunRecord, LocalRunState, ResourceEntry, ResourceOrigin, SqliteWorktreeStore,
};
use crate::ops::Ctx;
use crate::runner::{
    CancelUnsupported, Clock, RunEvent, RunEventKind, RunHandle, RunSpec, RunTokenUsage, Runner,
    RunnerRegistry, RunnerState, RunnerStatus, SteerMessage, T3Runner, clip_event_summary,
    runner_agent_source, system_clock,
};
use crate::state;
use crate::t3::{SendMode, T3, T3Cli, TimedOut, folder_gone};
use crate::usage::UsageSource;
use crate::worktree::WorktreeManager;

pub mod device;
pub mod device_rpc;
pub mod judges;
pub mod prepack;

pub use judges::{
    JUDGE_REGISTER_WAIT, JUDGE_RETRY, JUDGE_START_RETRY, JUDGE_TASKS_AT_ONCE, JUDGES_AT_ONCE,
    JudgeRetry, JudgeRun, Merge, MergeHold, MergeHoldKind, PR_HOLD_WAIT, PR_READ_EVERY,
    REVIEW_PR_WAIT,
};

/// Wake-ups for an agent shown running are held at most this long, then sent anyway: T3
/// gives a busy thread the message as its next turn, and a thread left marked running (a
/// turn ended on an API error, an interrupt, a Codex error) must still be woken.
pub const WAKE_HOLD_MAX: TimeDelta = TimeDelta::minutes(3);
/// A wake-up not delivered this long after it was queued, or after the reset its paused
/// agent waits for, is dropped.
pub const WAKE_TTL: TimeDelta = TimeDelta::hours(1);
/// Delay before sending again after a failed send, doubled per failure up to the cap.
pub const WAKE_RETRY_FIRST: TimeDelta = TimeDelta::seconds(30);
pub const WAKE_RETRY_MAX: TimeDelta = TimeDelta::minutes(10);
/// How long one wake-up send may take; one that runs out may have been delivered, so it is
/// sent again only when a later check finds the thread lacks it (see [`UnsureWakes`]).
const WAKE_SEND_TIMEOUT: Duration = Duration::from_secs(20);
/// How many agents' wake-ups a flush checks at once.
const FLUSH_CHECKS_AT_ONCE: usize = 8;
/// How many of the tasks that waited on a finished task are read at once. The client speaks
/// HTTP/1.1, so each read at once holds a connection of its own.
const BLOCKED_CHECKS_AT_ONCE: usize = 4;
/// Renew the 45-minute coordinator lease of a dispatched board this often.
pub const RENEW_EVERY: chrono::TimeDelta = chrono::TimeDelta::minutes(10);
/// The daemon reads the usage endpoint at most this often; between its own readings, each
/// reconcile takes the newest reading any device reported.
pub const USAGE_POLL_EVERY: TimeDelta = TimeDelta::minutes(5);
/// ... and this often while the weekly pace is near a line ([`Usage::near_line`]).
pub const USAGE_POLL_NEAR: TimeDelta = TimeDelta::minutes(1);
/// After the usage endpoint refuses a reading for its rate limit, the daemon reads it only
/// every [`USAGE_POLL_EVERY`] for this long.
pub const USAGE_REFUSED_BACKOFF: TimeDelta = TimeDelta::minutes(30);
/// The longest wait the endpoint may ask for with a refusal (`Retry-After`); the shortest
/// is [`USAGE_POLL_NEAR`].
pub const USAGE_RETRY_MAX: TimeDelta = TimeDelta::minutes(10);
/// T3 Code's reading stands in for the daemon's own while it is at most this much older
/// than the time between two readings: T3 Code takes one every 5 minutes.
pub const USAGE_SNAPSHOT_SLACK: TimeDelta = TimeDelta::minutes(1);
/// While T3 Code's readings stand in, the daemon still reads the endpoint this often, to
/// catch a reading T3 Code dated anew without reading it anew.
pub const USAGE_CHECK_EVERY: TimeDelta = TimeDelta::minutes(15);
/// How many thread reads of a flush run at once. Each `t3` process needs about half a
/// second of CPU to start; nine at once on a busy device took 4-5 seconds each, and longer
/// than [`Timing::thread_check`] under a build, so every check timed out together.
pub const THREAD_CHECKS_AT_ONCE: usize = 2;
/// How long a worker thread may take to claim its task: one that has not by then, or whose
/// session ended first, no longer holds a slot.
pub const WORKER_CLAIM_WAIT: TimeDelta = TimeDelta::minutes(20);
/// Brief startup grace before an idle session with zero completed turns is considered failed.
const RUN_START_GRACE: TimeDelta = TimeDelta::seconds(60);
/// How long a task whose worker never claimed it, whose start failed, or that its worker
/// gave back to `open` while no other task was in `doing` or `review`, waits before the
/// dispatcher starts another worker for it, so a worker that stops at once (a failing
/// session) is not started again on every reconcile. One given back while other tasks were
/// in `doing` or `review`, as a lease overlap does, waits instead until one of those leaves
/// `doing` and `review` ([`WatchState::worker_skips`]).
pub const WORKER_RETRY: TimeDelta = TimeDelta::minutes(30);
/// How long a usage-limit stop is remembered: it holds a further worker back only while a
/// task in progress started before it.
pub const USAGE_STOP_KEPT: TimeDelta = TimeDelta::days(7);

/// How long the daemon remembers that it recorded a hold by the device limit. A hold that
/// lasts longer gets a new line in the trail file, and its task a new note.
pub const HOLD_KEPT: TimeDelta = TimeDelta::days(7);

/// How long after the server's repeat idle notice the dispatcher waits before it sets the
/// task of an owner still away `open`: the notice also wakes the owner, which may come
/// back.
pub const RELEASE_GRACE: TimeDelta = TimeDelta::minutes(5);
/// Twice the server's default `doing_idle_secs` (30 minutes): used only by legacy
/// board-lease dispatch (against older servers that lack pull dispatch) to recognize a
/// late first `idle_doing` notice for which the server sends no separate repeat. Pull
/// dispatch on current servers uses the server's configured `2 * doing_idle_secs` directly.
pub const DOING_IDLE_TWICE: TimeDelta = TimeDelta::minutes(60);
/// How many tasks in `doing` the dispatcher reads per board to count its slots.
const DOING_READ: u32 = 100;
const STATE_NAME: &str = "watch";
/// Delay before starting workers on a board again after one failed to start, doubled per failure up to the cap.
pub const START_RETRY_FIRST: TimeDelta = TimeDelta::minutes(1);
pub const START_RETRY_MAX: TimeDelta = TimeDelta::minutes(30);
/// Initial backoff for a repository/project path after a macOS TCC privacy refusal on this
/// device, doubled per consecutive refusal up to [`TCC_REPO_BACKOFF_MAX`] so peer runners
/// handle its work cleanly instead of stalling reconcile (`agent-board-778`).
pub const TCC_REPO_BACKOFF_FIRST: TimeDelta = TimeDelta::minutes(5);
pub const TCC_REPO_BACKOFF_MAX: TimeDelta = TimeDelta::minutes(30);
/// Events whose handling failed on a transient error are retried for this long.
pub const RETRY_EVENTS_FOR: TimeDelta = TimeDelta::minutes(30);
const RETRY_EVENTS_MAX: usize = 500;
/// The cap of the event stream's reconnect delay, which doubles per failed or short-lived
/// connection from [`Timing::reconnect_first`].
const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// A connection that stayed open this long resets the reconnect delay.
const STREAM_HEALTHY: Duration = Duration::from_secs(60);

/// How often `board watch` sends `PUT /v1/devices/{name}/heartbeat` (`docs/SCHEDULER_DESIGN.md`).
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);

/// How often active managed runs renew their server-side 3-minute lease.
pub const RUN_LEASE_RENEW_EVERY: TimeDelta = TimeDelta::seconds(30);

/// Safety cap on how many pull claims a single reconcile pass may perform.
const PULL_CLAIMS_PER_TICK_MAX: usize = 16;
/// Exponential backoff schedule (seconds) for daemon-side fallback when talking to an older
/// server that lacks server-side fast spawn-failure backoff (`agent-board-818`).
const LLM_SPAWN_BACKOFF_SECS: [i64; 4] = [30, 60, 120, 300];
/// Maximum elapsed time since a previous local spawn failure's expiry before resetting streak.
const LLM_SPAWN_BACKOFF_STREAK_RESET: TimeDelta = TimeDelta::hours(1);

/// How often the daemon acts, and how long each step may take, so a server or `t3` that
/// stops answering holds up only that step. Tests shorten them.
#[derive(Debug, Clone)]
pub struct Timing {
    /// The boards are reconciled this often.
    pub reconcile_every: Duration,
    /// Queued wake-ups are sent this often.
    pub flush_every: Duration,
    /// Device heartbeats (`PUT /v1/devices/{name}/heartbeat`) are sent this often.
    pub heartbeat_every: Duration,
    /// An event stream with no bytes for this long (the server sends a keep-alive every 15
    /// seconds) is taken as dead and opened again after `last_seq`.
    pub stream_idle: Duration,
    /// A reconcile still running after this long is stopped and runs again on its next tick.
    pub reconcile_budget: Duration,
    /// Checking the queued wake-ups before a flush sends them stops after this long; the
    /// queues not checked by then back off.
    pub flush_budget: Duration,
    /// The last flush before exiting gets this long; what it does not send stays saved.
    pub stop_budget: Duration,
    /// Each `t3 thread show` gets this long.
    pub thread_check: Duration,
    /// The first delay before reopening the event stream.
    pub reconnect_first: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            reconcile_every: Duration::from_secs(60),
            flush_every: Duration::from_secs(3),
            heartbeat_every: HEARTBEAT_EVERY,
            stream_idle: Duration::from_secs(45),
            reconcile_budget: Duration::from_secs(90),
            flush_budget: Duration::from_secs(30),
            stop_budget: Duration::from_secs(10),
            thread_check: Duration::from_secs(10),
            reconnect_first: Duration::from_secs(1),
        }
    }
}

/// What the daemon remembers between runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WatchState {
    /// The last event handled.
    pub last_seq: Option<i64>,
    /// Events whose handling failed on a transient error, retried on the reconcile tick.
    #[serde(default)]
    pub retry: Vec<Event>,
    /// Wake-ups not sent yet, by agent: held while the agent runs or is paused, or kept
    /// after a failed send. Saved so a restart does not lose them; an entry this version
    /// cannot read is skipped rather than resetting the rest of the state.
    #[serde(default, deserialize_with = "lenient_queues")]
    pub wakes: BTreeMap<AgentId, WakeQueue>,
    /// The wake-up messages to each agent whose send timed out, until a flush finds them
    /// in the thread or owes what they carried again (see [`UnsureWakes`]). Read leniently
    /// like `wakes`.
    #[serde(
        default,
        deserialize_with = "lenient_unsure",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub unsure_wakes: BTreeMap<AgentId, UnsureWakes>,
    /// The reset the Claude agents' wake-ups last waited for while a usage window was full
    /// ([`Usage::full_until`]), or when that hold ended, if before its reset: those
    /// wake-ups live from it on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_full_until: Option<Timestamp>,
    /// Worker threads this device's dispatcher started on daemon-dispatch boards, by task,
    /// while each holds a slot. Saved before `t3 thread start` runs, so a start the daemon did not live to finish is
    /// found again.
    /// Read leniently like `wakes`.
    #[serde(
        default,
        deserialize_with = "lenient_workers",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub workers: BTreeMap<TaskId, Worker>,
    /// Tasks whose worker stopped without parking them, or whose start failed, and when
    /// another worker may start (see [`WORKER_RETRY`]). Read leniently like `wakes`.
    #[serde(
        default,
        deserialize_with = "lenient_worker_retry",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub worker_retry: BTreeMap<TaskId, Timestamp>,
    /// Tasks whose worker gave them back to `open` while other tasks were in `doing` or
    /// `review` (a lease overlap, `prompts/worker.md` step 3; tasks in `review` keep their
    /// leases), with those tasks: no worker starts for one until a task in its set leaves
    /// `doing` and `review`. Read leniently like `wakes`.
    #[serde(
        default,
        deserialize_with = "lenient_worker_skips",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub worker_skips: BTreeMap<TaskId, BTreeSet<TaskId>>,
    /// The last usage-limit stop of each agent (its move to `paused`), kept for
    /// [`USAGE_STOP_KEPT`]: a further worker starts beside the tasks in progress only when
    /// no agent of the board stopped since they started. Read leniently like `wakes`.
    #[serde(
        default,
        deserialize_with = "lenient_usage_stops",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub usage_stops: BTreeMap<AgentId, Timestamp>,
    /// The holds by the device limit that the device's trail file has, and when each was
    /// written, until their task or judge starts or [`HOLD_KEPT`] passes: `worker <task>`
    /// and `judge <title>`, with ` unread` for a hold on a T3 that listed no threads. Saved
    /// so a restart writes no second line for a hold it recorded. Read leniently like
    /// `wakes`.
    #[serde(
        default,
        deserialize_with = "lenient_holds",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub holds: BTreeMap<String, Timestamp>,
    /// The tasks that carry a note of the device limit holding them, and when it was
    /// written, kept like `holds`.
    #[serde(
        default,
        deserialize_with = "lenient_hold_notes",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub hold_notes: BTreeMap<TaskId, Timestamp>,
    /// The first idle notice of each task in `doing` on a board this device dispatches: the
    /// server's repeat of the same spell releases the task when its owner is still away.
    /// Read leniently like `wakes`.
    #[serde(
        default,
        deserialize_with = "lenient_idle_notices",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub idle_notices: BTreeMap<TaskId, IdleNotice>,
    /// Steps the dispatcher owes for agents it cannot wake, done on its next reconcile.
    /// Those this version cannot read are skipped.
    #[serde(
        default,
        deserialize_with = "lenient_owed",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub owed: Vec<Owed>,
    /// The judge threads the dispatcher started, until their judge ends. Those this
    /// version cannot read are skipped.
    #[serde(
        default,
        deserialize_with = "judges::lenient_judges",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub judges: Vec<JudgeRun>,
    /// Tasks whose judge ended, by `<kind>:<task>` (`goal:<board>` for a goal review): no
    /// judge of that kind starts for them again until the retry passes or the task
    /// changes. Read leniently like `wakes`.
    #[serde(
        default,
        deserialize_with = "judges::lenient_judge_retry",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub judge_retry: BTreeMap<String, JudgeRetry>,
    /// The pull requests of the reviewed tasks the dispatcher closes, by task. Read
    /// leniently like `wakes`.
    #[serde(
        default,
        deserialize_with = "judges::lenient_merges",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub merges: BTreeMap<TaskId, Merge>,
    /// Active pull-claimed managed runs supervised by this daemon until completion or
    /// failure (`docs/SCHEDULER_DESIGN.md`, Section 3.3). Read leniently like `wakes`.
    #[serde(
        default,
        deserialize_with = "lenient_managed_runs",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub managed_runs: BTreeMap<RunId, ManagedRun>,
    /// Boards this device dispatches where a task was finished since the last goal
    /// review, mapped to the finished task that made the review due (when known): their
    /// goal review is due once nothing is ready or in progress.
    #[serde(
        default,
        deserialize_with = "lenient_goal_due",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub goal_due: BTreeMap<BoardSlug, Option<TaskId>>,
}

/// One pull-claimed managed [`board_proto::Run`] currently supervised by this daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedRun {
    pub run_id: RunId,
    pub board: BoardSlug,
    pub kind: RunKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TaskId>,
    pub runner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub cost_units: u32,
    pub title: String,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_renewed_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<Timestamp>,
    #[serde(default)]
    pub noted: bool,
    /// True while the server `Run` is in `waiting_external` (e.g., a blocked worker waiting
    /// on an ask answer) so `renew_runs` transitions it back to `running` once active again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waiting_external: bool,
    /// Last `Runner::poll_events` sequence number successfully pushed to `POST /v1/runs/{id}/events`,
    /// persisted in `daemon.db` so restarts never skip or duplicate live run events.
    #[serde(default)]
    pub last_event_seq: u64,
}

/// `telemetry_outbox.kind` for pending run transcript archival jobs.
pub const TELEMETRY_KIND_TRANSCRIPT_ARCHIVE: &str = "transcript_archive";

/// First retry delay (seconds) for failed transcript archive uploads in `telemetry_outbox`.
const ARCHIVE_RETRY_FIRST_SECS: i64 = 30;
/// Maximum retry delay (seconds) for exponential backoff in `telemetry_outbox` (1 hour).
const ARCHIVE_RETRY_MAX_SECS: i64 = 3600;
/// Timeout for one invocation of the configured `[archive] command`.
/// Timeout for a single transcript archival command invocation, bounded well below
/// `device_stale_secs` (90s) and `RUN_LEASE_TTL_SECS_DEFAULT` (180s) so a hung upload never
/// starves heartbeats or run lease renewals.
const ARCHIVE_CMD_TIMEOUT: Duration = Duration::from_secs(20);
/// Maximum due `telemetry_outbox` items attempted in a single flush tick.
const ARCHIVE_MAX_PER_TICK: u32 = 4;
/// Maximum validated length of a `transcript_uri` recorded via `PATCH /v1/runs/{id}`.
const TRANSCRIPT_URI_MAX_CHARS: usize = 1_000;

/// Durable outbox payload for uploading a terminal run's transcript and recording
/// `transcript_uri` + `transcript_sha256` on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptArchiveJob {
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board: Option<BoardSlug>,
    pub file: PathBuf,
    pub sha256: String,
    /// Cached URI once upload succeeds so a retry after a transient server PATCH error does
    /// not re-upload the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploaded_uri: Option<String>,
}

/// The server's first notice that a task's owner is away from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdleNotice {
    pub away: AgentId,
    /// Since when the owner has been away, as the notice says.
    pub since: Timestamp,
    /// When the notice came.
    pub noticed: Timestamp,
}

/// A step the dispatcher of a board owes for an agent no device can wake
/// (`docs/DESIGN.md`, "Daemon dispatch", Routing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Owed {
    /// An ask answered whose asker is gone: a `decision` note with the answer on its task,
    /// then the task back to `open`. `check_gone` when nothing has shown yet that the
    /// asker is gone: the step is done only when it has ended or is unknown.
    Answer {
        board: BoardSlug,
        ask: i64,
        asker: AgentId,
        check_gone: bool,
        /// Whether the note is on the task, so a retry does not add it twice.
        #[serde(default)]
        noted: bool,
        at: Timestamp,
    },
    /// An owner still away at the repeat of the idle notice, or whose thread is gone: the
    /// task back to `open` with a note naming its old thread and branch.
    Release {
        board: BoardSlug,
        task: TaskId,
        away: AgentId,
        since: Timestamp,
        noticed: Timestamp,
        #[serde(default)]
        noted: bool,
        at: Timestamp,
    },
}

impl Owed {
    fn board(&self) -> &BoardSlug {
        match self {
            Owed::Answer { board, .. } | Owed::Release { board, .. } => board,
        }
    }

    fn at(&self) -> Timestamp {
        match self {
            Owed::Answer { at, .. } | Owed::Release { at, .. } => *at,
        }
    }

    /// Whether `other` is the same step: the same ask, or the same task.
    fn same(&self, other: &Owed) -> bool {
        match (self, other) {
            (Owed::Answer { ask: a, .. }, Owed::Answer { ask: b, .. }) => a == b,
            (Owed::Release { task: a, .. }, Owed::Release { task: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// A worker thread the dispatcher started for one task. It holds one of the board's
/// `parallel_tasks` slots until it parks the task, or stops without claiming it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Worker {
    /// Its thread or runner-native session id; `None` until `Runner::spawn` returns. A start
    /// the daemon did not live to finish is found by `title` among the threads created since
    /// `started_at` (for T3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    pub title: String,
    /// The branch of its worktree, when it runs in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Runner kind that owns this worker (`"t3"` when omitted in older state files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    /// Pre-assigned or resolved board [`AgentId`] when it differs from `<runner>:<thread>`
    /// (for example ACP runs whose `BOARD_AGENT` is pre-assigned before spawning so `board mcp`
    /// inherits it at startup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    /// Daemon-managed worktree record id, when [`WorktreeManager`] created the worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<String>,
    /// Path of the daemon-managed worktree, when [`WorktreeManager`] created it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    /// Local run id in `daemon.db`'s `local_runs` table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Weighted resource units accounted in `daemon.db`'s `resource_ledger`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_units: Option<u32>,
    pub started_at: Timestamp,
    /// Whether it has claimed the task: from then on, the task leaving its `doing` frees
    /// the slot.
    #[serde(default)]
    pub claimed: bool,
    /// Whether its thread is noted on the task.
    #[serde(default)]
    pub noted: bool,
}

/// A task that holds one of a board's slots, as the dispatcher counts them.
#[derive(Debug, Clone)]
struct InProgress {
    /// `None` when it could not be read: nothing starts beside it then.
    task: Option<Task>,
    /// When it started: its worker's start, else its last update.
    since: Timestamp,
}

/// What became of a worker the dispatcher follows.
#[derive(Debug, Clone)]
enum Followed {
    /// It holds its slot: it has its task in `doing`, or may still claim it. `task` is
    /// `None` when the task could not be read.
    Holds {
        task: Option<Box<Task>>,
        since: Timestamp,
    },
    /// It claimed its task and gave it back to `open`.
    Left,
    /// It no longer holds a slot for any other reason.
    Freed,
}

/// Whether no task in progress waits on `task`. A ready task waits on none of them: the
/// server counts a task ready only once its blockers are done.
fn independent(task: &Task, in_progress: &BTreeMap<TaskId, InProgress>) -> bool {
    !in_progress
        .values()
        .filter_map(|p| p.task.as_ref())
        .any(|p| p.blockers.contains(&task.id))
}

/// What one owed step came to.
enum Paid {
    /// It set this task `open`.
    Opened(TaskId),
    /// It is done, or has nothing left to do.
    Nothing,
    /// It waits for a later reconcile.
    Later,
}

/// The wake-up messages to one agent whose send timed out: T3 may or may not have
/// delivered them. A flush looks for each one's [`wake_marker`] in the thread
/// [`SEND_CHECK_AFTER`] after its send, and holds the agent's next lines until then, so
/// they go out with what is owed again. Found, a message is settled; missing while no run
/// of the thread may still take it, what it carried is owed again, once: its wake-ups are
/// queued again (see [`Wake::resent`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UnsureWakes {
    #[serde(default)]
    pub messages: Vec<UnsureMessage>,
    /// The thread is not read again before this: it was busy, or a check failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_after: Option<Timestamp>,
    /// The delay after the last failed check, doubled per failure; none once a check
    /// worked. While set, the agent's next lines are not held for a check: a T3 that
    /// cannot be read must not hold up every wake-up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_delay_secs: Option<i64>,
}

impl UnsureWakes {
    /// Whether the agent's next lines wait for a check: a message is not looked for yet,
    /// and no check has failed.
    pub fn holds(&self, now: Timestamp) -> bool {
        self.failed_delay_secs.is_none()
            && self
                .messages
                .iter()
                .any(|m| now - m.since < SEND_CHECK_AFTER)
    }

    /// Whether a flush reads the thread now: a message is due, and no wait is on.
    fn read_due(&self, now: Timestamp) -> bool {
        self.read_after.is_none_or(|after| now >= after)
            && self
                .messages
                .iter()
                .any(|m| now - m.since >= SEND_CHECK_AFTER)
    }
}

/// A wake-up message whose send timed out, by what it carried for the first time; the text
/// itself is never kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnsureMessage {
    /// The [`wake_digest`] its marker names.
    pub digest: String,
    /// Its wake-ups not sent twice already.
    #[serde(
        default,
        deserialize_with = "lenient_wakes",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub wakes: Vec<Wake>,
    /// When the send that may have delivered it ran.
    pub since: Timestamp,
}

impl UnsureMessage {
    fn is_empty(&self) -> bool {
        self.wakes.is_empty()
    }
}

/// One agent's wake-ups not sent yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WakeQueue {
    #[serde(default, deserialize_with = "lenient_wakes")]
    pub wakes: Vec<Wake>,
    /// Set after a failed send: nothing is sent before `retry_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff: Option<Backoff>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Backoff {
    pub delay_secs: i64,
    pub retry_at: Timestamp,
    /// Set after a failed agent lookup, which is not tried again before `retry_at`. After a
    /// failed send, only the send waits: the agent is still checked, so its answers are
    /// passed on and expired lines go.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lookup: bool,
}

/// A queued wake-up: what it is about, checked again and rendered when it is sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wake {
    pub board: BoardSlug,
    pub about: WakeAbout,
    pub queued_at: Timestamp,
    /// An answer whose asker cannot be woken now, already passed on to the dispatcher.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forwarded: bool,
    /// Queued again after a send that timed out never reached the thread: one more send
    /// that times out counts as delivered, so a wake-up is sent at most twice.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeAbout {
    /// For the owner of the blocked `task`: `done` was its last blocker not done. The line
    /// names only the task, so two blockers done close together make one line.
    Unblocked { done: TaskId, task: TaskId },
    /// For `asker`: the user answered its ask.
    Answered { ask: i64, asker: AgentId },
    /// For `away`: the server noticed at `noticed` that it, the owner of `task` in `doing`,
    /// has been away from it since `since`.
    Idle {
        task: TaskId,
        away: AgentId,
        since: Timestamp,
        noticed: Timestamp,
    },
    /// A server-routed sticky wake (`GET /v1/devices/{name}/wakes`) persisted in
    /// `daemon.db`'s `unsure_sends` before delivery via `Runner::send`.
    Sticky { wake_id: i64, wake_kind: String },
}

impl WakeAbout {
    /// Its kind, as `watch.log` names it.
    fn kind(&self) -> &'static str {
        match self {
            Self::Unblocked { .. } => "unblocked",
            Self::Answered { .. } => "answered",
            Self::Idle { .. } => "idle",
            Self::Sticky { .. } => "sticky",
        }
    }

    /// Whether this wake-up is urgent and should steer an active turn mid-flight
    /// (`SendMode::Auto`) rather than waiting for the running-agent hold or queueing behind it.
    fn is_urgent(&self) -> bool {
        matches!(self, Self::Answered { .. })
    }

    /// Whether this wake-up is an away-from-doing idle notice targeting `agent`.
    fn is_idle_for(&self, agent: &AgentId) -> bool {
        matches!(self, Self::Idle { away, .. } if away == agent)
    }
}

/// Delivery mode for a server-routed sticky wake kind (`answered`, `steer`, ...).
fn sticky_wake_send_mode(kind: &str) -> SendMode {
    match kind {
        "answered" | "steer" => SendMode::Auto,
        _ => SendMode::Queue,
    }
}

/// Each agent's queue read on its own, so one this version cannot read (a newer kind, a
/// damaged entry) is skipped instead of failing the whole state file.
fn lenient_queues<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<AgentId, WakeQueue>, D::Error> {
    let mut queues: BTreeMap<AgentId, WakeQueue> =
        lenient_by_key(serde_json::Value::deserialize(d)?, "the wake-ups");
    queues.retain(|_, queue| !queue.wakes.is_empty());
    Ok(queues)
}

/// Each agent's timed-out wake-up message read on its own, like [`lenient_queues`].
fn lenient_unsure<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<AgentId, UnsureWakes>, D::Error> {
    let mut unsure: BTreeMap<AgentId, UnsureWakes> = lenient_by_key(
        serde_json::Value::deserialize(d)?,
        "the timed-out wake-up messages",
    );
    unsure.retain(|_, u| !u.messages.is_empty());
    Ok(unsure)
}

/// Each worker read on its own, like [`lenient_queues`].
fn lenient_workers<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<TaskId, Worker>, D::Error> {
    Ok(lenient_by_key(
        serde_json::Value::deserialize(d)?,
        "the worker",
    ))
}

/// Each task's retry time read on its own, like [`lenient_queues`].
fn lenient_worker_retry<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<TaskId, Timestamp>, D::Error> {
    Ok(lenient_by_key(
        serde_json::Value::deserialize(d)?,
        "the worker retry",
    ))
}

/// Each task's skip read on its own, like [`lenient_queues`].
fn lenient_worker_skips<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<TaskId, BTreeSet<TaskId>>, D::Error> {
    Ok(lenient_by_key(
        serde_json::Value::deserialize(d)?,
        "the worker skip",
    ))
}

/// Each recorded hold read on its own, like [`lenient_queues`].
fn lenient_holds<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, Timestamp>, D::Error> {
    Ok(lenient_by_key(
        serde_json::Value::deserialize(d)?,
        "the recorded hold",
    ))
}

/// Each task's note of a hold read on its own, like [`lenient_queues`].
fn lenient_hold_notes<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<TaskId, Timestamp>, D::Error> {
    Ok(lenient_by_key(
        serde_json::Value::deserialize(d)?,
        "the note of a hold",
    ))
}

/// Each agent's usage-limit stop read on its own, like [`lenient_queues`].
fn lenient_usage_stops<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<AgentId, Timestamp>, D::Error> {
    Ok(lenient_by_key(
        serde_json::Value::deserialize(d)?,
        "the usage-limit stop",
    ))
}

/// Each task's idle notice read on its own, like [`lenient_queues`].
fn lenient_idle_notices<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<TaskId, IdleNotice>, D::Error> {
    Ok(lenient_by_key(
        serde_json::Value::deserialize(d)?,
        "the idle notice",
    ))
}

/// Each managed run read on its own, like [`lenient_queues`].
fn lenient_managed_runs<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<RunId, ManagedRun>, D::Error> {
    Ok(lenient_by_key(
        serde_json::Value::deserialize(d)?,
        "the managed run",
    ))
}

/// Reads `goal_due` from either the current `{board: task_id | null}` map or the legacy
/// `[board, ...]` array, skipping unreadable entries.
fn lenient_goal_due<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<BoardSlug, Option<TaskId>>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Object(entries) => Ok(entries
            .into_iter()
            .filter_map(|(key, val)| match key.parse::<BoardSlug>() {
                Ok(slug) => match serde_json::from_value::<Option<TaskId>>(val) {
                    Ok(task) => Some((slug, task)),
                    Err(_) => {
                        tracing::warn!(
                            "watch state: skipped unreadable goal_due task for {slug}; keeping board due"
                        );
                        Some((slug, None))
                    }
                },
                Err(_) => {
                    tracing::warn!(
                        "watch state: skipped unreadable goal_due board {key}"
                    );
                    None
                }
            })
            .collect()),
        serde_json::Value::Array(items) => Ok(items
            .into_iter()
            .filter_map(|item| match serde_json::from_value::<BoardSlug>(item) {
                Ok(slug) => Some((slug, None)),
                Err(_) => {
                    tracing::warn!("watch state: skipped unreadable legacy goal_due board");
                    None
                }
            })
            .collect()),
        _ => Ok(BTreeMap::new()),
    }
}

/// Owed steps read one by one; those this version cannot read are skipped.
fn lenient_owed<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Owed>, D::Error> {
    let serde_json::Value::Array(steps) = serde_json::Value::deserialize(d)? else {
        return Ok(Vec::new());
    };
    Ok(steps
        .into_iter()
        .filter_map(|step| match serde_json::from_value(step) {
            Ok(step) => Some(step),
            Err(_) => {
                tracing::warn!("watch state: skipped an owed step this version cannot read");
                None
            }
        })
        .collect())
}

/// The entries of `value`, an object by agent or task id, that this version can read;
/// `what` names an entry in the warning for one skipped.
fn lenient_by_key<K: std::str::FromStr + Ord, V: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    what: &str,
) -> BTreeMap<K, V> {
    let serde_json::Value::Object(entries) = value else {
        return BTreeMap::new();
    };
    entries
        .into_iter()
        .filter_map(
            |(key, entry)| match (key.parse::<K>(), serde_json::from_value(entry)) {
                (Ok(key), Ok(entry)) => Some((key, entry)),
                _ => {
                    tracing::warn!(
                        "watch state: skipped {what} for {key}, which this version cannot read"
                    );
                    None
                }
            },
        )
        .collect()
}

/// Wake-ups read one by one; those this version cannot read are skipped.
fn lenient_wakes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Wake>, D::Error> {
    let serde_json::Value::Array(wakes) = serde_json::Value::deserialize(d)? else {
        return Ok(Vec::new());
    };
    Ok(wakes
        .into_iter()
        .filter_map(|wake| match serde_json::from_value(wake) {
            Ok(wake) => Some(wake),
            Err(err) => {
                tracing::warn!("watch state: skipped a wake-up this version cannot read: {err}");
                None
            }
        })
        .collect())
}

/// What the daemon knows of an agent with queued wake-ups: read from the server once, then
/// kept current by the agent's `agent_state_changed` events, so a hold needs no polling.
#[derive(Debug, Clone)]
struct Seen {
    agent: Agent,
    /// When `agent.state` was last read or changed.
    since: Timestamp,
    /// The agent stopped running since it was first read: its held wake-ups are due.
    idled: bool,
    /// For an ended agent: what T3 said of its thread, asked once per state change.
    thread: ThreadCheck,
    /// Until when `WakeAbout::Idle` wake-ups are deferred after the runner reported an active
    /// run (`ThreadInfo::busy`), avoiding polling `t3 thread show` on every flush while leaving
    /// urgent wakes unblocked. Cleared on any `agent_state_changed` event.
    idle_busy_until: Option<Timestamp>,
}

/// What T3 said of an ended agent's thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadCheck {
    /// Not asked since the agent's state last changed.
    NotAsked,
    /// T3 can run the thread: it has it, has not archived it, and its folder is there.
    Live,
    /// T3 no longer has the thread, archived it, or its folder is gone, so every run fails.
    Gone,
    /// T3 did not answer: the send is tried, and a failed send asks again.
    NoAnswer,
}

impl Seen {
    /// An ended agent handled by this daemon's runner on this device whose session has not
    /// been asked about yet. T3 closes a Claude session that sits idle (after 30 minutes),
    /// and the session's end marks the agent ended, but the thread lives on and a message
    /// resumes it.
    fn needs_thread_check(&self, device: &DeviceName, runner_source: AgentSource) -> bool {
        self.agent.state == AgentState::Ended
            && self.thread == ThreadCheck::NotAsked
            && &self.agent.device == device
            && self.agent.id.source() == runner_source
    }
}

/// Whether an agent's wake-ups can be sent now.
enum Reach {
    Now,
    /// Running: held until it goes idle or for [`WAKE_HOLD_MAX`].
    Held,
    /// Paused until a usage-limit reset still to come.
    Paused,
    /// A Claude agent while a usage window of the account is full: a turn would stop on
    /// the limit at once, so its wake-ups wait for that window's reset.
    Full,
    /// Not to be woken by this daemon.
    Never(Unreachable),
}

/// Why an agent's wake-ups cannot be sent from this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unreachable {
    /// Another device's agent: that device's daemon wakes it.
    Elsewhere,
    /// Not a T3 thread.
    NotT3,
    /// T3 no longer has its thread, archived it, or its folder is gone.
    ThreadGone,
    /// Paused with no reset time: resuming is left to the user.
    NoReset,
}

impl Unreachable {
    /// Expected on a device that is not the agent's: nothing is lost, so nothing is logged.
    fn expected(self) -> bool {
        matches!(self, Self::Elsewhere | Self::NotT3)
    }
}

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Elsewhere => "it runs on another device",
            Self::NotT3 => "it is not a T3 thread",
            Self::ThreadGone => {
                "T3 no longer has its thread, archived it, or the thread's folder is gone"
            }
            Self::NoReset => "it is paused with no reset time",
        })
    }
}

impl Reach {
    /// `full_until` is the reset a full usage window of the Claude account waits for
    /// ([`Daemon::usage_full_until`]).
    fn of(
        seen: &Seen,
        device: &DeviceName,
        runner_source: AgentSource,
        full_until: Option<Timestamp>,
        now: Timestamp,
    ) -> Self {
        match Self::alone(seen, device, runner_source, now) {
            // Codex and opencode agents spend other limits; Claude and Antigravity respect
            // the daemon's active usage reading.
            Self::Now | Self::Held
                if full_until.is_some()
                    && matches!(seen.agent.runtime, Runtime::Claude | Runtime::Antigravity) =>
            {
                Self::Full
            }
            reach => reach,
        }
    }

    /// By the agent's own state, whatever the account's usage.
    fn alone(seen: &Seen, device: &DeviceName, runner_source: AgentSource, now: Timestamp) -> Self {
        let agent = &seen.agent;
        if &agent.device != device {
            return Self::Never(Unreachable::Elsewhere);
        }
        if agent.id.source() != runner_source {
            return Self::Never(Unreachable::NotT3);
        }
        if seen.thread == ThreadCheck::Gone {
            return Self::Never(Unreachable::ThreadGone);
        }
        match agent.state {
            // T3 closes idle sessions, which marks their agents ended; a message resumes
            // the thread unless T3 says it is gone. When T3 did not answer, the send finds out.
            AgentState::Ended => match seen.thread {
                ThreadCheck::Gone => Self::Never(Unreachable::ThreadGone),
                // The session closed while it waited for a usage-limit reset: the limit
                // still holds, and a message before the reset would only stop on it again.
                _ if agent.paused_until.is_some_and(|until| until > now) => Self::Paused,
                ThreadCheck::NotAsked | ThreadCheck::Live | ThreadCheck::NoAnswer => Self::Now,
            },
            AgentState::Paused => match agent.paused_until {
                Some(until) if until > now => Self::Paused,
                Some(_) => Self::Now,
                // Without a reset time, resuming is left to the user.
                None => Self::Never(Unreachable::NoReset),
            },
            AgentState::Running if !seen.idled && now - seen.since < WAKE_HOLD_MAX => Self::Held,
            AgentState::Running | AgentState::Idle => Self::Now,
        }
    }
}

/// A wake-up message ready to send, with the wake-ups it carries.
struct Outgoing {
    agent: AgentId,
    message: String,
    mode: SendMode,
    wakes: Vec<Wake>,
    /// The queue's back-off before this send, which a failure doubles.
    backoff: Option<Backoff>,
    /// The [`wake_digest`] its marker names, when its send was recorded in
    /// [`WatchState::unsure_wakes`] before it ran: it carries something not sent twice yet.
    unsure: Option<String>,
}

/// What a check found of a wake-up message whose send timed out.
enum Arrival {
    /// The thread has it.
    Found,
    /// Not in the thread, but a run of it is active or queued, which may still take it.
    Waiting,
    /// The thread never got it.
    Missing,
}

/// What a flush does about an agent's timed-out wake-up messages ([`UnsureWakes`]), read
/// before anything changes.
#[derive(Default)]
struct Look {
    /// The agent's lines wait for a message not looked for yet.
    hold: bool,
    /// What a read of the thread found of the messages due, if it was read.
    read: Option<Result<Sorted>>,
    /// The thread's latest messages, when just read: the fresh-session check uses them
    /// instead of reading again.
    recent: Option<crate::t3::RecentMessages>,
}

/// The due timed-out messages of an agent, sorted by what a read of its thread found.
struct Sorted {
    /// The digests of those the thread has.
    arrived: Vec<String>,
    /// Those the thread never got: what they carried is owed again.
    missing: Vec<UnsureMessage>,
    /// Whether some are neither, since a run of the thread may still take them.
    waiting: bool,
}

/// Sorts `unsure`'s messages due at `now` by `read`, a read of the agent's thread. A thread
/// T3 does not know never got them: the send of their lines finds it gone and passes its
/// answers on.
fn sort_unsure(
    unsure: &UnsureWakes,
    read: Option<&crate::t3::RecentMessages>,
    now: Timestamp,
) -> Sorted {
    let mut sorted = Sorted {
        arrived: Vec::new(),
        missing: Vec::new(),
        waiting: false,
    };
    for message in unsure
        .messages
        .iter()
        .filter(|m| now - m.since >= SEND_CHECK_AFTER)
    {
        let marker = wake_marker(&message.digest);
        let arrival = read.map_or(Arrival::Missing, |recent| {
            arrival_in(recent, |user| user.iter().any(|m| m.text.contains(&marker)))
        });
        match arrival {
            Arrival::Found => sorted.arrived.push(message.digest.clone()),
            Arrival::Waiting => sorted.waiting = true,
            _ => sorted.missing.push(message.clone()),
        }
    }
    sorted
}

/// Adds `wake` to `wakes` unless one about the same thing is there; then that one counts as
/// sent twice only when both do, so a new event keeps its own second send.
fn add_wake(wakes: &mut Vec<Wake>, wake: Wake) {
    match wakes
        .iter_mut()
        .find(|w| w.board == wake.board && w.about == wake.about)
    {
        Some(queued) => queued.resent &= wake.resent,
        None => wakes.push(wake),
    }
}

/// `queue` with the wake-ups of `missing`, messages the thread never got, added again, each
/// marked [`Wake::resent`].
fn requeue_unsure(mut queue: WakeQueue, missing: &[UnsureMessage]) -> WakeQueue {
    for wake in missing.iter().flat_map(|m| &m.wakes) {
        let wake = Wake {
            resent: true,
            ..wake.clone()
        };
        add_wake(&mut queue.wakes, wake);
    }
    queue
}

/// Whether `recent`, a thread's latest messages, holds a message whose send timed out: `has`
/// says whether its user messages, oldest first, include it. Without it, it counts as
/// missing only when T3 has no run of the thread active or queued, which might still take
/// the message.
fn arrival_in(
    recent: &crate::t3::RecentMessages,
    has: impl FnOnce(&[crate::t3::UserMessage]) -> bool,
) -> Arrival {
    if has(&recent.user) {
        Arrival::Found
    } else if recent.busy {
        Arrival::Waiting
    } else {
        Arrival::Missing
    }
}

/// How a wake-up send went.
enum Sent {
    Delivered,
    /// It ran out of time: T3 may have delivered it, so its wake-up lines
    /// are sent again only when a later check finds the thread lacks it.
    MaybeDelivered(anyhow::Error),
    /// T3 no longer has the thread, it is archived, or its worktree is gone.
    Gone(anyhow::Error),
    Failed(anyhow::Error),
}

/// Per-wake outcome computed during [`Daemon::check`] before [`Daemon::decide`] mutates state.
enum WakeCheck {
    /// Checked against the server: `Ok(Some(line))` sends it, `Ok(None)` drops it as stale,
    /// `Err(err)` retries on transient error or drops on non-transient error.
    Checked(Result<Option<String>>),
    /// Kept in the queue while the agent is in its running-agent hold (non-urgent wake
    /// alongside an urgent wake that bypasses the hold).
    Deferred,
    /// Kept in the queue because the runner reported an active run (`ThreadInfo::busy`) for
    /// a `WakeAbout::Idle` wake-up.
    DeferredBusy,
}

/// What a flush learned of one agent's queue, read before anything changes, so a flush
/// cut short leaves the queue as it was.
enum Checked {
    /// Not checked before the flush's deadline: left as it was, for the next flush.
    NotReached,
    /// Backing off after a failed lookup: left as it was.
    Waiting,
    /// The lookup failed.
    LookupFailed(anyhow::Error),
    /// Unknown to the server.
    Unknown,
    /// Known; `read` when just read from the server or updated by a runner check. `rechecks`
    /// holds the outcome of each wake-up still fresh, in order, when any wake-up can be
    /// evaluated now and no failed send is backing off.
    Known {
        seen: Box<Seen>,
        read: bool,
        rechecks: Option<Vec<WakeCheck>>,
        /// True when `idle_busy_until` transitioned from `None` to `Some` in this check, so
        /// `decide` writes one Trail Rule hold record at the start of the busy spell.
        record_busy_idle_hold: bool,
    },
}

/// When a wake-up for an agent that waited for the reset `waited_until`
/// ([`Daemon::waited_until`]) is still worth sending: its wake-ups live from the reset on.
fn is_fresh(wake: &Wake, waited_until: Option<Timestamp>, now: Timestamp) -> bool {
    let from = waited_until.map_or(wake.queued_at, |until| until.max(wake.queued_at));
    now - from < WAKE_TTL
}

/// Whether a wake-up about `about`, for `from`, goes to the dispatcher when `from` cannot
/// be woken: an answer to `from`'s own ask, and, when no device can wake `from` (`idle`),
/// the notice that `from` is away from its task.
fn passed_on(from: &AgentId, about: &WakeAbout, idle: bool) -> bool {
    match about {
        WakeAbout::Answered { asker, .. } => asker == from,
        WakeAbout::Idle { away, .. } => idle && away == from,
        _ => false,
    }
}

/// The usage-limit reset `seen` waits for: a paused agent's, or that of one whose session
/// ended while it waited, which the server keeps.
fn paused_until(seen: &Seen) -> Option<Timestamp> {
    match seen.agent.state {
        AgentState::Paused | AgentState::Ended => seen.agent.paused_until,
        _ => None,
    }
}

/// The next delay of a doubling back-off: `first`, then twice the `last`, up to `max`.
fn next_delay(last: Option<TimeDelta>, first: TimeDelta, max: TimeDelta) -> TimeDelta {
    last.map_or(first, |last| (last * 2).min(max))
}

/// A wake-up message whose send timed out is looked for in the thread this long
/// after the send, since T3 may still be handling the request the timed-out
/// `t3 thread send` made.
pub const SEND_CHECK_AFTER: TimeDelta = TimeDelta::minutes(1);

/// Prefix of the dispatcher's worker-start note.
pub(super) const WORKER_NOTE_PREFIX: &str = "note: the dispatcher started a worker for this task";
/// Prefix when the dispatcher's worker-start note names the worktree branch.
pub(super) const WORKER_NOTE_BRANCH: &str =
    "note: the dispatcher started a worker for this task on branch ";
/// Prefix when the dispatcher's worker-start note has no worktree branch (in-place worker).
pub(super) const WORKER_NOTE_IN_PLACE: &str =
    "note: the dispatcher started a worker for this task: ";

/// The last line of a wake-up message, before its marker. The lines above it name what
/// changed, and the agent's prompt hook adds the board delta (alerts and changed counts) to
/// the same prompt, so the message asks for no snapshot: that call would cost the agent one
/// more turn for what the prompt already holds.
const WAKE_END: &str = "Continue from this and the board delta in this prompt, if there is one.";

/// What this device's daemon last saved (from `daemon.db`, falling back to legacy
/// `watch.json` when `daemon.db` does not exist yet).
pub fn load_watch_state(home: &Home) -> WatchState {
    let db_path = LocalDb::path_for(home);
    if db_path.exists() {
        return LocalDb::open_path(&db_path)
            .and_then(|db| db.load_watch_state())
            .unwrap_or_default();
    }
    state::load_named(home, STATE_NAME)
}

/// The agent id of a T3 thread.
pub fn thread_agent(thread: &str) -> Result<AgentId> {
    format!("t3:{thread}")
        .parse::<AgentId>()
        .with_context(|| format!("thread id {thread:?} makes no agent id"))
}

/// The board settings that decide whether this device can start its workers.
#[derive(Debug, Clone, PartialEq)]
struct HostingConfig {
    repos: Vec<String>,
    autopilot: bool,
    status: BoardStatus,
    home_devices: Vec<DeviceName>,
}

impl HostingConfig {
    fn of(board: &Board) -> Self {
        Self {
            repos: board.repos.clone(),
            autopilot: board.autopilot,
            status: board.status,
            home_devices: board.home_devices.clone(),
        }
    }
}

/// A failed worker start, retried at `retry_at` unless the board's settings change.
#[derive(Debug, Clone)]
struct StartFailure {
    config: HostingConfig,
    delay: TimeDelta,
    retry_at: Timestamp,
}

/// How long to pause claiming new runs after spawning a detached self-restart command while
/// waiting for launchd/systemd to send `SIGTERM`. If the daemon was started manually outside
/// a service manager and no `SIGTERM` arrives in this window, normal dispatch resumes.
const SELF_RESTART_DRAIN_SECS: i64 = 15;

/// Returns `true` when the installed `board` binary commit on disk is a known commit that
/// differs from this running daemon's build commit (so a graceful self-restart is needed).
pub(crate) fn should_self_restart(running_commit: &str, installed_commit: Option<&str>) -> bool {
    if running_commit.is_empty() || running_commit == "unknown" {
        return false;
    }
    match installed_commit {
        Some(installed) if !installed.is_empty() && installed != "unknown" => {
            installed != running_commit
        }
        _ => false,
    }
}

/// Injectable boundary for probing the installed `board` binary commit and triggering a
/// detached graceful self-restart of `board watch` after a deterministic after-merge deploy
/// completes `task_done` and marks its server run terminal.
pub trait SelfRestarter: Send + Sync {
    fn running_commit(&self) -> &str;
    fn installed_commit(&self) -> BoxFuture<'_, Option<String>>;
    fn trigger_restart(&self, running: &str, installed: &str) -> Result<()>;
}

/// Production [`SelfRestarter`] probing `board --version` on disk and triggering a detached
/// `launchctl kickstart -k` (macOS) or `systemctl --user restart` (Linux).
#[derive(Debug, Clone, Default)]
pub struct RealSelfRestarter;

impl SelfRestarter for RealSelfRestarter {
    fn running_commit(&self) -> &str {
        crate::COMMIT
    }

    fn installed_commit(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(crate::client::installed_commit)
                .await
                .ok()
                .flatten()
        })
    }

    fn trigger_restart(&self, _running: &str, _installed: &str) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            use std::process::Stdio;
            #[cfg(target_os = "macos")]
            let script = {
                let uid = rustix::process::geteuid().as_raw();
                format!(
                    "sleep 1; exec launchctl kickstart -k 'gui/{uid}/{}'",
                    crate::install::LAUNCHD_LABEL
                )
            };
            #[cfg(not(target_os = "macos"))]
            let script = format!(
                "sleep 1; exec systemctl --user restart '{}'",
                crate::install::SYSTEMD_UNIT
            );
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg("-c")
                .arg(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0);
            let _child = cmd
                .spawn()
                .with_context(|| format!("spawning detached self-restart ({script})"))?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("self-restart is only supported on Unix systems")
        }
    }
}

/// Completion summary returned by a background deterministic `after-merge-deploy` task.
#[derive(Debug, Clone, Copy)]
struct BgMergeResult {
    local_state: LocalRunState,
    check_self_restart: bool,
}

/// Wraps an [`UpgradeDeployer`] and records whether `run_upgrade` was actually invoked.
struct TrackingDeployer {
    inner: Arc<dyn UpgradeDeployer>,
    invoked: AtomicBool,
}

impl TrackingDeployer {
    fn new(inner: Arc<dyn UpgradeDeployer>) -> Self {
        Self {
            inner,
            invoked: AtomicBool::new(false),
        }
    }

    fn was_invoked(&self) -> bool {
        self.invoked.load(Ordering::Acquire)
    }
}

impl UpgradeDeployer for TrackingDeployer {
    fn run_upgrade<'a>(
        &'a self,
        project: &'a Path,
        sha: &'a str,
    ) -> BoxFuture<'a, Result<UpgradeRunOutput>> {
        self.invoked.store(true, Ordering::Release);
        self.inner.run_upgrade(project, sha)
    }
}

/// Returns true when a run's `outcome` JSON already carries non-zero token usage counters.
fn outcome_has_tokens(outcome: Option<&serde_json::Value>) -> bool {
    let Some(obj) = outcome.and_then(serde_json::Value::as_object) else {
        return false;
    };
    obj.get("tokens_in")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
        > 0
        || obj
            .get("tokens_out")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            > 0
}

pub struct Daemon<T: T3 + 'static> {
    ctx: Ctx,
    t3: Arc<T>,
    /// Local SQLite WAL database (`$BOARD_HOME/state/daemon.db`) holding write-ahead intent,
    /// resource ledger, sticky wakes, unsure sends, worktrees, cursor, and outbox.
    db: LocalDb,
    /// Non-`None` when opening `$BOARD_HOME/state/daemon.db` failed and we fell back to an
    /// in-memory DB so `Daemon::new` never panics while `save()` surfaces the failure.
    db_open_error: Option<String>,
    /// Default runtime-agnostic execution boundary for legacy dispatch and wake delivery.
    runner: Arc<dyn Runner>,
    /// Multi-runner registry (`"t3"`, `"acp"`, `"fake"`, ...) used by capability-matched
    /// pull dispatch (`docs/SCHEDULER_DESIGN.md`, Section 4.3).
    runners: RunnerRegistry,
    /// Zero-LLM native Rust executor for `RunKind::Merge` and `RunKind::Housekeeping`.
    deterministic: DeterministicRunner,
    /// Daemon-owned git worktree lifecycle manager (`$BOARD_HOME/worktrees/<repo-slug>/<id>`).
    worktrees: WorktreeManager,
    /// Whether this daemon creates worktrees directly via `worktrees` (true for all non-T3
    /// runners; false for `T3Runner`, because `t3 thread start` requires a path inside a
    /// registered T3 project and exposes only `--worktree <base> --branch <branch>`).
    daemon_worktrees: bool,
    /// How the dispatcher reads, updates and merges pull requests.
    gh: Arc<dyn Gh>,
    /// Injectable executor for `deploy/upgrade.sh --yes --no-restart-local <sha>`.
    upgrade_deployer: Arc<dyn UpgradeDeployer>,
    /// Injectable local binary probe + detached graceful self-restart trigger.
    self_restarter: Arc<dyn SelfRestarter>,
    /// Long-running deterministic `after-merge-deploy` tasks spawned off the reconcile loop so
    /// multi-minute `deploy/upgrade.sh --yes --no-restart-local <sha>` runs never exceed
    /// `reconcile_budget` (90s) or stall heartbeats and lease renewals.
    bg_merges: BTreeMap<RunId, (TaskId, tokio::task::JoinHandle<BgMergeResult>)>,
    /// Set once a deterministic after-merge deploy finishes so the daemon checks whether the
    /// installed binary commit changed and triggers a detached graceful self-restart once no
    /// background deploys remain.
    self_restart_needed: bool,
    /// When a detached self-restart was triggered; pauses new pull claims briefly while
    /// waiting for launchd/systemd to send `SIGTERM`.
    self_restart_triggered_at: Option<Timestamp>,
    /// Injectable clock for periodic ticks, lease renewals, and Trail Rule records.
    clock: Clock,
    /// Whether the connected server supports `POST /v1/dispatch/claim` (`dispatch-claim-v1`):
    /// `None` until detected via `/healthz` or the first claim call.
    dispatch_claim_supported: Option<bool>,
    /// Set once the server answers 404/405 to `GET /v1/devices/{name}/wakes`.
    wakes_unsupported: bool,
    /// Set once the server answers 404/405 to `POST /v1/runs/{id}/events`.
    run_events_unsupported: bool,
    /// Set once the server answers 404/405 to per-provider usage bucket endpoints.
    usage_buckets_unsupported: bool,
    /// True when `self.usage` currently holds a legacy `anthropic/default` fallback reading
    /// rather than a reading from this daemon's own `quota_bucket()` (so `follows()` must
    /// never reject genuine local snapshots from another provider).
    usage_from_fallback: bool,
    /// True once startup/reconnect reconciliation of orphaned local transcript files has run.
    orphaned_transcripts_checked: bool,
    /// Run ids already archived (or confirmed not needing archival) in this daemon process.
    archived_transcripts: BTreeSet<String>,
    /// Path to T3 Code's local `statev2.sqlite` (read-only) for token backfill on orphaned
    /// managed and interactive runs. Overridable in tests.
    t3_state_db_path: Option<PathBuf>,
    /// Terminal run ids already inspected for token backfill in this daemon lifetime.
    token_backfill_checked: BTreeSet<String>,
    /// Last timestamp when periodic `backfill_missing_run_tokens` ran.
    last_token_backfill: Option<Timestamp>,
    /// Per-task fallback merge backoff when talking to an older server that lacks
    /// server-side per-task merge cooldowns. Only populated when the same deferred/unclear/
    /// failed merge task is re-offered within a single reconcile tick.
    merge_backoffs: MergeBackoff,
    /// Per-task review-wait deadlines (`parked_at + REVIEW_PR_WAIT`) preventing tight
    /// spin-claim-cancel loops when a task enters `review` before its PR or branch is pushed.
    review_waits: ReviewWaitBackoff,
    /// Per-(kind, subject, runner) fast spawn-failure fallback backoff for talking to older
    /// servers that lack server-side `SpawnBackoff` (`agent-board-818`).
    llm_spawn_backoffs: LlmSpawnBackoff,
    /// Per-repo and per-project-path backoff after macOS TCC privacy refusals (`EINTR`,
    /// `EPERM`, timeout, or unreadable `.git` on protected folders) so peer devices handle
    /// the repo cleanly instead of stalling reconcile (`agent-board-778`).
    tcc_repo_backoffs: TccRepoBackoff,
    /// Sticky wake ids that timed out once, were verified missing from the runner, and are
    /// allowed at most one resend before being held/dropped with a Trail Rule record.
    sticky_resent: BTreeSet<i64>,
    /// Sticky wake ids already resent once after a timeout so they are never sent a third time.
    sticky_exhausted: BTreeSet<i64>,
    pub state: WatchState,
    boards: BTreeMap<BoardSlug, Board>,
    /// Boards where a worker failed to start here, by board.
    failures: BTreeMap<BoardSlug, StartFailure>,
    /// Agents with queued wake-ups, as last read or reported.
    agents: BTreeMap<AgentId, Seen>,
    /// Agents whose wake-up message is being sent. A flush cut short while sending leaves
    /// them here: their lines count as maybe delivered, never sent twice.
    sending: Vec<AgentId>,
    /// The text of `config.toml` as last read, so a reconcile re-reads it only after an edit.
    config_text: Option<String>,
    /// Optional channel publishing reloaded `Config` snapshots to background tasks (e.g. device RPC).
    config_tx: Option<tokio::sync::watch::Sender<Config>>,
    pub timing: Timing,
    /// Where the account's usage is read. [`UsageSource::Server`] unless set: `board
    /// watch` sets Claude Code's endpoint.
    pub usage_source: UsageSource,
    /// The newest usage reading, as last read or reported.
    pub usage: Option<Usage>,
    /// When this daemon last read the usage endpoint itself.
    usage_polled: Option<Timestamp>,
    /// When the usage endpoint last refused a reading for its rate limit.
    usage_refused: Option<Timestamp>,
    /// When the endpoint may be read again, after a refusal that said how long to wait.
    usage_retry_at: Option<Timestamp>,
    /// The date of the newest reading taken from T3 Code's file.
    usage_snapshot_at: Option<Timestamp>,
    /// Whether this device's dispatch agent is registered since the daemon started.
    dispatch_registered: bool,
    /// When this daemon last claimed or renewed each board's lease as its dispatch agent.
    dispatch_renewed: BTreeMap<BoardSlug, Timestamp>,
    /// The runs this device may have at once when `runs_at_once` is not set; `None`
    /// leaves the device without a limit of its own.
    default_run_limit: Option<usize>,
    /// Host memory in bytes (read once at startup in `run`, or injected in tests).
    host_memory: Option<u64>,
    /// Optional test override for auto-detected `deploy:upgrade-conf` capability (`None` probes
    /// `${XDG_CONFIG_HOME:-$HOME/.config}/agent-board/upgrade.conf` on disk).
    upgrade_conf_override: Option<bool>,
    /// Set once the server answers 404/405 to `PUT /v1/devices/{name}/heartbeat`, so older
    /// servers are never spammed.
    heartbeat_unsupported: bool,
    /// The boards that wait for one of the device's worker slots, and for a judge slot.
    worker_turns: device::Turns,
    judge_turns: device::Turns,
    /// The boards off autopilot that wait for one of their verdict judges' slots
    /// ([`judges::HOME_VERDICTS_AT_ONCE`]): a queue of their own, so that their backlog
    /// never puts a dispatched board's judge behind them.
    verdict_turns: device::Turns,
    /// The time of the last reconcile.
    last_pass: Option<Timestamp>,
    /// The threads with a run in progress on this device, read from T3 at most once in a
    /// reconcile, when a board first wants a slot; `None` before that.
    running: Option<device::Running>,
}

impl<T: T3 + 'static> Daemon<T> {
    /// A daemon acting as the device of `ctx` (whose client must carry no agent id).
    pub fn new(ctx: Ctx, t3: T) -> Self {
        let (db, db_open_error) = match LocalDb::open_for_home(&ctx.home) {
            Ok(db) => (db, None),
            Err(err) => {
                let msg = format!("opening daemon.db: {err:#}");
                tracing::warn!("{msg}; falling back to in-memory daemon state");
                (
                    LocalDb::open_in_memory().expect("opening in-memory SQLite database"),
                    Some(msg),
                )
            }
        };
        let state = match db.load_watch_state() {
            Ok(state) => state,
            Err(err) => {
                tracing::warn!("loading state from daemon.db ({err:#}); starting fresh");
                WatchState::default()
            }
        };
        let config_text = std::fs::read_to_string(ctx.home.config_path()).ok();
        let t3 = Arc::new(t3);
        let acp_runner: Option<Arc<dyn Runner>> = ctx
            .config
            .acp_runner
            .as_ref()
            .filter(|acp_cfg| acp_cfg.resolved_bin().is_some())
            .map(|acp_cfg| {
                Arc::new(AcpRunner::new(
                    acp_cfg.clone(),
                    ctx.home.clone(),
                    db.clone(),
                )) as Arc<dyn Runner>
            });
        let explicit_excludes_t3 =
            !ctx.config.runners.is_empty() && !ctx.config.runners.iter().any(|r| r.trim() == "t3");
        let (default_runner, runners, daemon_worktrees) =
            if explicit_excludes_t3 && let Some(acp) = acp_runner {
                let registry = RunnerRegistry::new().with_runner(Arc::clone(&acp));
                (acp, registry, true)
            } else {
                let t3_runner: Arc<dyn Runner> =
                    Arc::new(T3Runner::new(Arc::clone(&t3), ctx.home.clone()));
                let mut registry = RunnerRegistry::new().with_runner(Arc::clone(&t3_runner));
                if let Some(acp) = acp_runner {
                    registry.register(acp);
                }
                (t3_runner, registry, false)
            };
        let worktrees = WorktreeManager::with_store(
            ctx.home.clone(),
            Arc::new(SqliteWorktreeStore::new(db.clone())),
        );
        Self {
            ctx,
            t3,
            db,
            db_open_error,
            runner: default_runner,
            runners,
            deterministic: DeterministicRunner::new(),
            worktrees,
            daemon_worktrees,
            gh: Arc::new(GhCli::locate()),
            upgrade_deployer: Arc::new(RealUpgradeDeployer),
            self_restarter: Arc::new(RealSelfRestarter),
            bg_merges: BTreeMap::new(),
            self_restart_needed: false,
            self_restart_triggered_at: None,
            clock: system_clock(),
            dispatch_claim_supported: None,
            wakes_unsupported: false,
            run_events_unsupported: false,
            usage_buckets_unsupported: false,
            usage_from_fallback: false,
            orphaned_transcripts_checked: false,
            archived_transcripts: BTreeSet::new(),
            t3_state_db_path: crate::runner::default_t3_state_db_path(),
            token_backfill_checked: BTreeSet::new(),
            last_token_backfill: None,
            merge_backoffs: MergeBackoff::default(),
            review_waits: ReviewWaitBackoff::default(),
            llm_spawn_backoffs: LlmSpawnBackoff::default(),
            tcc_repo_backoffs: TccRepoBackoff::default(),
            sticky_resent: BTreeSet::new(),
            sticky_exhausted: BTreeSet::new(),
            state,
            boards: BTreeMap::new(),
            failures: BTreeMap::new(),
            agents: BTreeMap::new(),
            sending: Vec::new(),
            config_text,
            config_tx: None,
            timing: Timing::default(),
            usage_source: UsageSource::Server,
            usage: None,
            usage_polled: None,
            usage_refused: None,
            usage_retry_at: None,
            usage_snapshot_at: None,
            dispatch_registered: false,
            dispatch_renewed: BTreeMap::new(),
            default_run_limit: None,
            host_memory: None,
            upgrade_conf_override: None,
            heartbeat_unsupported: false,
            worker_turns: device::Turns::default(),
            judge_turns: device::Turns::default(),
            verdict_turns: device::Turns::default(),
            last_pass: None,
            running: None,
        }
    }

    /// The local SQLite WAL database (`$BOARD_HOME/state/daemon.db`).
    pub fn db(&self) -> &LocalDb {
        &self.db
    }

    /// Supplies the host's physical memory in bytes for capacity reporting in device
    /// heartbeats.
    #[must_use]
    pub fn with_host_memory(mut self, bytes: Option<u64>) -> Self {
        self.host_memory = bytes;
        self
    }

    /// Overrides local `upgrade.conf` auto-detection for `"deploy:upgrade-conf"` capability
    /// advertisement (for deterministic tests independent of the host's `$HOME`).
    #[must_use]
    pub fn with_upgrade_conf(mut self, has_upgrade_conf: bool) -> Self {
        self.upgrade_conf_override = Some(has_upgrade_conf);
        self
    }

    /// Replaces the deterministic after-merge [`UpgradeDeployer`] (for testing).
    #[must_use]
    pub fn with_upgrade_deployer(mut self, deployer: impl UpgradeDeployer + 'static) -> Self {
        self.upgrade_deployer = Arc::new(deployer);
        self
    }

    /// Replaces the local binary probe + graceful [`SelfRestarter`] (for testing).
    #[must_use]
    pub fn with_self_restarter(mut self, restarter: impl SelfRestarter + 'static) -> Self {
        self.self_restarter = Arc::new(restarter);
        self
    }

    fn advertised_capabilities(&self) -> Vec<String> {
        let has_upgrade_conf = self
            .upgrade_conf_override
            .unwrap_or_else(device::has_upgrade_conf);
        device::effective_capabilities_with_upgrade_conf(&self.ctx.config, has_upgrade_conf)
    }

    /// Replaces the daemon's execution [`Runner`]. Non-T3 runners automatically use
    /// daemon-owned git worktrees created by [`WorktreeManager`].
    #[must_use]
    pub fn with_runner(mut self, runner: impl Runner + 'static) -> Self {
        let runner: Arc<dyn Runner> = Arc::new(runner);
        self.daemon_worktrees = runner.kind() != "t3";
        self.runners = RunnerRegistry::new().with_runner(Arc::clone(&runner));
        self.runner = runner;
        self
    }

    /// Replaces the daemon's [`RunnerRegistry`] (for multi-runner setups or pull-dispatch
    /// integration tests). The registry's default runner also becomes the fallback runner.
    #[must_use]
    pub fn with_runner_registry(mut self, registry: RunnerRegistry) -> Self {
        if let Some(default) = registry.default_runner() {
            self.daemon_worktrees = default.kind() != "t3";
            self.runner = default;
        }
        self.runners = registry;
        self
    }

    /// Overrides the configured [`DispatchMode`] (`auto`, `pull`, or `legacy`).
    #[must_use]
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.ctx.config.dispatch_mode = mode;
        self
    }

    /// Overrides the configured `[archive]` transcript archival settings.
    #[must_use]
    pub fn with_archive_config(mut self, archive: Option<ArchiveConfig>) -> Self {
        self.ctx.config.archive = archive;
        self
    }

    /// Overrides the local T3 Code SQLite DB path used for read-only token backfill (for tests).
    #[must_use]
    pub fn with_t3_state_db_path(mut self, path: Option<PathBuf>) -> Self {
        self.t3_state_db_path = path;
        self
    }

    /// Replaces the clock used by `follow()`, `step()`, `save()`, and Trail Rule records.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Pre-populates whether the server advertises `dispatch-claim-v1` (e.g. from `/healthz`).
    pub fn set_dispatch_claim_supported(&mut self, supported: bool) {
        self.dispatch_claim_supported = Some(supported);
    }

    /// Whether this daemon currently uses capability-matched pull dispatch rather than legacy
    /// 45-minute board coordinator leases.
    pub fn is_pull_mode(&self) -> bool {
        match self.ctx.config.dispatch_mode {
            DispatchMode::Pull => true,
            DispatchMode::Legacy => false,
            DispatchMode::Auto => self.dispatch_claim_supported != Some(false),
        }
    }

    /// Replaces the daemon's [`WorktreeManager`] (for custom persistence backends such as
    /// Slice 1.3 `daemon.db` or isolated tests).
    #[must_use]
    pub fn with_worktree_manager(mut self, worktrees: WorktreeManager) -> Self {
        self.worktrees = worktrees;
        self
    }

    /// The kind identifier of the active [`Runner`] (`"t3"`, `"acp"`, `"fake"`, ...).
    pub fn runner_kind(&self) -> &'static str {
        self.runner.kind()
    }

    /// The [`AgentSource`] owned by this daemon's active [`Runner`].
    fn runner_source(&self) -> AgentSource {
        runner_agent_source(self.runner.kind())
    }

    /// The daemon's [`WorktreeManager`].
    pub fn worktrees(&self) -> &WorktreeManager {
        &self.worktrees
    }

    /// Cancels an active run through the active [`Runner`], recording a Trail Rule entry.
    pub async fn cancel_run(&self, handle: &RunHandle, reason: &str) -> Result<()> {
        self.runner.cancel(handle, reason).await
    }

    /// Polls normalized [`RunEvent`]s for `handle` after sequence number `after_seq`.
    pub async fn poll_run_events(
        &self,
        handle: &RunHandle,
        after_seq: u64,
    ) -> Result<Vec<RunEvent>> {
        self.runner.poll_events(handle, after_seq).await
    }

    /// Limits the runs of a device that sets no `runs_at_once` to `limit`.
    #[must_use]
    pub fn with_default_run_limit(mut self, limit: usize) -> Self {
        self.default_run_limit = Some(limit);
        self
    }

    /// How many runs, workers and judges together, this device may have at once across
    /// its boards: `runs_at_once`, or the default from the host's memory.
    fn run_limit(&self) -> usize {
        device::run_limit(&self.ctx.config, self.default_run_limit)
    }

    /// How many turns this device leaves free for launches from threads the daemon did
    /// not start.
    fn reserve(&self) -> usize {
        device::reserve(&self.ctx.config)
    }

    /// How many total runs on the device may be active while the daemon still starts a
    /// new worker or judge: `run_limit` minus the owner-thread reserve.
    fn daemon_run_limit(&self) -> usize {
        device::daemon_limit(self.run_limit(), self.reserve())
    }

    /// Whether the runs recorded for `slug` take slots of this device: it hosts the board.
    /// A paused board counts, since its threads still run. A run
    /// recorded for a board that left this device takes no slot: nothing here follows it
    /// to its end.
    fn counts_runs(&self, slug: &BoardSlug) -> bool {
        self.boards.get(slug).is_some_and(|b| self.hosts(b))
    }

    /// Whether the judges recorded for `slug` take slots of this device: it hosts the
    /// board, or follows the board's verdict judges ([`Self::judges_proposals`]).
    fn counts_judges(&self, slug: &BoardSlug) -> bool {
        let board = self.boards.get(slug);
        board.is_some_and(|b| self.hosts(b) || self.judges_proposals(b))
    }

    /// The workers this device has now, on every board.
    fn worker_runs(&self) -> usize {
        let workers = self.state.workers.keys();
        workers.filter(|t| self.counts_runs(&t.board())).count()
    }

    /// The runs of the dispatcher on this device now, workers and judges, on every board.
    fn own_runs(&self) -> usize {
        let judges = self.state.judges.iter();
        self.worker_runs() + judges.filter(|r| self.counts_judges(&r.board)).count()
    }

    /// The threads with a run in progress that are no run of the dispatcher counted in
    /// [`Self::own_runs`]: the user's threads, the threads agents launched, and the
    /// dispatcher's runs for a board this device no longer hosts. Empty until
    /// [`Self::read_running`] read them in this reconcile.
    fn other_turns(&self) -> Vec<&str> {
        let workers = self.state.workers.iter();
        let workers = workers.filter(|(task, _)| self.counts_runs(&task.board()));
        let judges = self.state.judges.iter();
        let judges = judges.filter(|run| self.counts_judges(&run.board));
        let own: BTreeSet<&str> = workers
            .filter_map(|(_, worker)| worker.thread.as_deref())
            .chain(judges.filter_map(|run| run.thread.as_deref()))
            .chain(
                self.state
                    .managed_runs
                    .values()
                    .filter(|run| run.runner == "t3")
                    .filter_map(|run| run.native_id.as_deref()),
            )
            .collect();
        let running = self.running.iter().flat_map(|r| &r.threads);
        let running = running.map(String::as_str);
        running.filter(|thread| !own.contains(thread)).collect()
    }

    /// What takes the device's slots now: the dispatcher's runs, and the turns of every
    /// other thread that runs on the device.
    fn runs(&self) -> usize {
        self.own_runs() + self.other_turns().len()
    }

    /// Reads the threads that run on this device, once in a reconcile: before the first
    /// start it decides on. A device without a limit reads none. Non-T3 runners do not
    /// query T3's project thread listing.
    async fn read_running(&mut self) {
        if self.running.is_none()
            && (self.run_limit() != usize::MAX || self.ctx.config.weight_units.is_some())
        {
            if self.runner.kind() != "t3" {
                self.running = Some(device::Running::default());
                return;
            }
            let running = device::running_threads(&*self.t3, &self.ctx.config).await;
            if let Some(unknown) = running.unknown() {
                tracing::warn!("{unknown}; the dispatcher starts nothing in this pass");
            }
            self.running = Some(running);
        }
    }

    /// Why the device's load is unknown in this reconcile: T3 could not list the threads
    /// of a project. The limit protects the device's memory, so the dispatcher starts
    /// nothing while it cannot count (`docs/DESIGN.md`, "Daemon dispatch", Device limit).
    fn load_unknown(&self) -> Option<String> {
        self.running.as_ref().and_then(device::Running::unknown)
    }

    /// What takes the device's slots now, in words: for the log and for the note on a task
    /// the limit holds.
    fn load(&self) -> String {
        let others = self.other_turns();
        let limit = self.run_limit();
        let reserve = self.reserve();
        let daemon_limit = self.daemon_run_limit();
        let reserved = if limit != usize::MAX && reserve > 0 {
            format!(" (daemon limit {daemon_limit}, {reserve} reserved)")
        } else {
            String::new()
        };
        let mut text = format!(
            "device {} has {} of {} runs{reserved}: {} of the dispatcher, {} turns of other threads",
            self.ctx.device,
            self.runs(),
            limit,
            self.own_runs(),
            others.len()
        );
        if !others.is_empty() {
            let shown = others.iter().take(device::THREADS_SHOWN);
            let shown = shown.copied().collect::<Vec<_>>().join(", ");
            text.push_str(&format!(" ({shown}"));
            match others.len().saturating_sub(device::THREADS_SHOWN) {
                0 => {}
                more => text.push_str(&format!(" and {more} more")),
            }
            text.push(')');
        }
        if let Some(unknown) = self.load_unknown() {
            text.push_str(&format!(
                "; {unknown}, so the turns of other threads are not known and nothing starts"
            ));
        }
        text
    }

    /// Reads, updates and merges pull requests through `gh` instead of the `gh` CLI.
    #[must_use]
    pub fn with_gh(mut self, gh: impl Gh + 'static) -> Self {
        self.gh = Arc::new(gh);
        self
    }

    /// Applies an edited `config.toml`: its `[projects]`, `[board_projects]` and
    /// `feedback_board` take effect at once, and boards whose start failed are tried again,
    /// since a missing project may be what failed. `runs_at_once` takes effect at once
    /// too. `server`, `device`, `t3_bin`, `[thread_model]`, `[judge_model]` and
    /// `[judge_models]` are fixed while the daemon runs, so changing them asks for a
    /// restart. A file gone or
    /// unreadable keeps the settings in use, with one warning per edit.
    fn reload_config(&mut self) {
        let path = self.ctx.home.config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                if self.config_text.take().is_some() {
                    tracing::warn!(
                        "reading {}: {err}; keeping the configuration in use",
                        path.display()
                    );
                }
                return;
            }
        };
        if self.config_text.as_deref() == Some(text.as_str()) {
            return;
        }
        let parsed = toml::from_str::<Config>(&text);
        self.config_text = Some(text);
        let new = match parsed {
            Ok(new) => new,
            Err(err) => {
                tracing::warn!(
                    "invalid {}: {err}; keeping the configuration in use until it is fixed",
                    path.display()
                );
                return;
            }
        };
        let current = &mut self.ctx.config;
        if (
            &new.server,
            &new.device,
            &new.t3_bin,
            &new.thread_model,
            &new.judge_model,
            &new.judge_models,
        ) != (
            &current.server,
            &current.device,
            &current.t3_bin,
            &current.thread_model,
            &current.judge_model,
            &current.judge_models,
        ) {
            tracing::warn!(
                "{} changes server, device, t3_bin, thread_model, judge_model or judge_models: restart board watch to apply them",
                path.display()
            );
        }
        if new.runs_at_once != current.runs_at_once {
            current.runs_at_once = new.runs_at_once;
            tracing::info!("re-read {}: runs_at_once updated", path.display());
        }
        if new.reserve_runs != current.reserve_runs {
            current.reserve_runs = new.reserve_runs;
            tracing::info!("re-read {}: reserve_runs updated", path.display());
        }
        if new.adaptive_review_effort != current.adaptive_review_effort {
            current.adaptive_review_effort = new.adaptive_review_effort;
            tracing::info!("re-read {}: adaptive_review_effort updated", path.display());
        }
        if (
            new.weight_units,
            &new.capabilities,
            &new.runners,
            &new.protocol_features,
        ) != (
            current.weight_units,
            &current.capabilities,
            &current.runners,
            &current.protocol_features,
        ) {
            current.weight_units = new.weight_units;
            current.capabilities = new.capabilities;
            current.runners = new.runners;
            current.protocol_features = new.protocol_features;
            tracing::info!(
                "re-read {}: device capacity/capabilities updated",
                path.display()
            );
        }
        if new.dispatch_mode != current.dispatch_mode {
            current.dispatch_mode = new.dispatch_mode;
            self.dispatch_claim_supported = None;
            tracing::info!(
                "re-read {}: dispatch_mode updated to {:?}",
                path.display(),
                current.dispatch_mode
            );
        }
        if new.archive != current.archive {
            current.archive = new.archive;
            self.orphaned_transcripts_checked = false;
            tracing::info!("re-read {}: archive settings updated", path.display());
        }
        if (
            &new.projects,
            &new.board_projects,
            &new.project_base_branches,
            &new.feedback_board,
        ) != (
            &current.projects,
            &current.board_projects,
            &current.project_base_branches,
            &current.feedback_board,
        ) {
            current.projects = new.projects;
            current.board_projects = new.board_projects;
            current.project_base_branches = new.project_base_branches;
            current.feedback_board = new.feedback_board;
            self.failures.clear();
            self.tcc_repo_backoffs.clear();
            tracing::info!(
                "re-read {}: projects, base branches, and feedback board updated",
                path.display()
            );
        }
        if let Some(tx) = &self.config_tx {
            tx.send_replace(self.ctx.config.clone());
        }
    }

    /// Adds the start of a hold by the device limit to the device's trail
    /// (`state/device-holds.jsonl`), once until the held task or judge starts: the time,
    /// the board, what is held, the limit and its `cause` (the device's load, or the count
    /// that another limit holds it by). It is the record of a held judge,
    /// which gets no note, and of a held task whose note the server did not take.
    fn record_hold(
        &mut self,
        kind: &str,
        board: &BoardSlug,
        subject: &str,
        limit: usize,
        cause: &str,
        now: Timestamp,
    ) {
        let unread = self.running.as_ref().map(|r| r.unread.as_slice());
        let unread = unread.unwrap_or_default();
        // A hold whose cause changes to a T3 that does not answer gets a line of its own.
        let key = if unread.is_empty() {
            format!("{kind} {subject}")
        } else {
            format!("{kind} {subject} unread")
        };
        if self.state.holds.contains_key(&key) {
            return;
        }
        let record = serde_json::json!({
            "at": now,
            "kind": kind,
            "board": board,
            "device": self.ctx.device,
            "subject": subject,
            "limit": limit,
            "cause": cause,
            "others": self.other_turns(),
            "unread": unread,
        });
        match state::append_hold(&self.ctx.home, &record) {
            Ok(()) => {
                self.state.holds.insert(key, now);
            }
            Err(err) => tracing::warn!("recording that the device limit holds {key}: {err:#}"),
        }
    }

    /// Records on `task` that the device limit holds it, once until it starts, so that a
    /// reader of the board sees why a ready task did not start and what ran instead. A
    /// note that fails is logged and tried again; the hold is in the device's trail file
    /// either way.
    async fn note_held(
        &mut self,
        task: &TaskId,
        agent: &AgentId,
        workers: usize,
        load: &str,
        now: Timestamp,
    ) {
        if self.state.hold_notes.contains_key(task) {
            return;
        }
        self.state.hold_notes.insert(task.clone(), now);
        let text = clip_note(format!(
            "The device limit holds this task: {load}; workers may take {workers}. The \
             dispatcher starts it when a slot is free, after the boards that waited longer. \
             The device keeps each hold in state/device-holds.jsonl."
        ));
        let client = self.client().with_agent(Some(agent.clone()));
        if let Err(err) = add_note(&client, task, agent, NoteKind::Note, text).await {
            self.state.hold_notes.remove(task);
            tracing::warn!("noting on {task} that the device limit holds it: {err:#}");
        }
    }

    /// Forgets the recorded hold of `subject`: it started, so a later hold is a new one.
    fn hold_ended(&mut self, kind: &str, subject: &str) {
        self.state.holds.remove(&format!("{kind} {subject}"));
        self.state.holds.remove(&format!("{kind} {subject} unread"));
    }

    pub fn save(&self) -> Result<()> {
        self.save_at((self.clock)())
    }

    pub(super) fn save_at(&self, now: Timestamp) -> Result<()> {
        if let Some(err) = &self.db_open_error {
            anyhow::bail!("{err}");
        }
        self.db.save_watch_state(&self.state, now)
    }

    /// Writes a Trail Rule record to `$BOARD_HOME/state/daemon-drops.jsonl` before dropping,
    /// holding, refusing, or giving back work (`docs/DESIGN.md`, "The trail rule",
    /// `agent-board-539`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_drop(
        &self,
        kind: &str,
        board: Option<&BoardSlug>,
        task: Option<&TaskId>,
        agent: Option<&AgentId>,
        cause: &str,
        subject: serde_json::Value,
        now: Timestamp,
    ) -> Option<PathBuf> {
        let record = serde_json::json!({
            "at": now,
            "kind": kind,
            "device": self.ctx.device,
            "board": board,
            "task": task,
            "agent": agent,
            "cause": cause,
            "subject": subject,
        });
        match state::append_daemon_drop(&self.ctx.home, &record) {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::warn!("recording trail drop {kind}: {err:#}");
                None
            }
        }
    }

    /// Writes a Trail Rule record to `$BOARD_HOME/state/daemon-drops.jsonl` and backs off
    /// claiming runs for `repos`/`project` on this device after a macOS TCC privacy refusal
    /// (`agent-board-778`). Returns `(backoff_until, trail_path)`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_tcc_repo_backoff(
        &mut self,
        board: &BoardSlug,
        task: Option<&TaskId>,
        run_id: Option<&RunId>,
        repos: &[String],
        project: &Path,
        detail: &str,
        now: Timestamp,
    ) -> (Timestamp, Option<PathBuf>) {
        let (delay, until, attempts) = self.tcc_repo_backoffs.next_schedule(repos, project, now);
        let repo_label = if repos.is_empty() {
            project.display().to_string()
        } else {
            repos.join(", ")
        };
        let cause = format!(
            "macOS TCC privacy refusal on {repo_label} ({}); backing off claims on {} for {}s \
             until {} (attempt {attempts}): {detail}",
            project.display(),
            self.ctx.device,
            delay.num_seconds(),
            until.to_rfc3339(),
        );
        // Record before acting (Trail Rule).
        let trail_path = self.record_drop(
            "tcc_repo_backoff",
            Some(board),
            task,
            None,
            &cause,
            serde_json::json!({
                "repos": repos,
                "project": project,
                "run_id": run_id,
                "backoff_secs": delay.num_seconds(),
                "backoff_until": until,
                "attempts": attempts,
                "error": detail,
            }),
            now,
        );
        self.tcc_repo_backoffs.record(
            repos,
            project.to_path_buf(),
            delay,
            until,
            attempts,
            detail.to_owned(),
        );
        tracing::warn!(
            "macOS TCC privacy refusal on {repo_label} ({}); backed off for {}s until {} \
             (attempt {attempts}, trail: {})",
            project.display(),
            delay.num_seconds(),
            until.to_rfc3339(),
            trail_path
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "write failed".to_owned()),
        );
        (until, trail_path)
    }

    pub(super) fn clear_tcc_repo_backoff(&mut self, repos: &[String], project: &Path) {
        self.tcc_repo_backoffs.clear_for(repos, project);
    }

    /// Configured repositories that are not currently in macOS TCC privacy backoff at `now`
    /// (checked by both normalized repo key and local checkout path so shared paths or
    /// `[board_projects]` backoffs also suppress sibling `[projects]` entries).
    fn available_repos(&self, now: Timestamp) -> Vec<String> {
        self.tcc_repo_backoffs
            .filter_projects(&self.ctx.config.projects, now)
    }

    /// Whether any `[board_projects]` checkout is not currently in macOS TCC privacy backoff.
    fn has_healthy_board_project(&self, now: Timestamp) -> bool {
        self.ctx
            .config
            .board_projects
            .values()
            .any(|path| !self.tcc_repo_backoffs.is_path_backed_off(path, now))
    }

    /// Synchronizes live managed runs (workers + judges) and detected interactive turns into
    /// `daemon.db`'s `resource_ledger` table so `free_units` stays accurate for weighted
    /// capacity enforcement and device heartbeats.
    pub(super) fn sync_resource_ledger(&self, now: Timestamp) {
        let mut managed = Vec::new();
        for (task, worker) in &self.state.workers {
            if !self.counts_runs(&task.board()) {
                continue;
            }
            let agent_id = worker
                .agent_id
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| {
                    worker.thread.as_deref().and_then(|thread| {
                        RunHandle::new(worker.runner.as_deref().unwrap_or("t3"), thread)
                            .resolve_agent_id()
                            .ok()
                            .map(|id| id.to_string())
                    })
                });
            managed.push(ResourceEntry {
                entry_id: format!("worker:{task}"),
                origin: ResourceOrigin::Managed,
                agent_id,
                run_id: worker.run_id.clone(),
                cost_units: worker.cost_units.unwrap_or(device::STANDARD_RUN_UNITS),
                updated_at: now,
            });
        }
        for judge in &self.state.judges {
            if !self.counts_judges(&judge.board) {
                continue;
            }
            let agent_id = judge
                .agent_id
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| {
                    judge.thread.as_deref().and_then(|thread| {
                        RunHandle::new(judge.runner.as_deref().unwrap_or("t3"), thread)
                            .resolve_agent_id()
                            .ok()
                            .map(|id| id.to_string())
                    })
                });
            managed.push(ResourceEntry {
                entry_id: format!(
                    "judge:{}:{}:{}",
                    judge.board,
                    judge.title,
                    judge.started_at.timestamp_millis()
                ),
                origin: ResourceOrigin::Managed,
                agent_id,
                run_id: judge.run_id.clone(),
                cost_units: device::cost_units_for_judge(judge.kind),
                updated_at: now,
            });
        }
        for run in self.state.managed_runs.values() {
            managed.push(ResourceEntry {
                entry_id: format!("run:{}", run.run_id),
                origin: ResourceOrigin::Managed,
                agent_id: run.agent_id.as_ref().map(ToString::to_string),
                run_id: Some(run.run_id.to_string()),
                cost_units: run.cost_units,
                updated_at: now,
            });
        }
        if let Err(err) = self
            .db
            .replace_resources_by_origin(ResourceOrigin::Managed, &managed)
        {
            tracing::warn!("syncing managed resource_ledger entries: {err:#}");
        }
        // Remove any interactive:<agent> row for an agent now tracked as managed so a
        // managed run is never double-counted alongside an earlier passive hook.sock entry.
        for entry in &managed {
            if let Some(ref aid) = entry.agent_id {
                let _ = self.db.remove_resource(&format!("interactive:{aid}"));
            }
        }
        // Reconcile only T3-scraped interactive turns (never on non-T3 runners where
        // `self.running` is an empty placeholder), using canonical `interactive:t3:<thread>`
        // ids so passive `hook.sock` rows for non-T3 runtimes (`claude:`, `codex:`,
        // `opencode:`, `antigravity:`) are preserved (`agent-board-689`).
        if self.runner.kind() == "t3" && self.running.is_some() && self.load_unknown().is_none() {
            let interactive: Vec<ResourceEntry> = self
                .other_turns()
                .into_iter()
                .map(|thread| ResourceEntry {
                    entry_id: format!("interactive:t3:{thread}"),
                    origin: ResourceOrigin::Interactive,
                    agent_id: Some(format!("t3:{thread}")),
                    run_id: None,
                    cost_units: device::STANDARD_RUN_UNITS,
                    updated_at: now,
                })
                .collect();
            if let Err(err) = self.db.replace_t3_interactive_resources(&interactive) {
                tracing::warn!("syncing T3 interactive resource_ledger entries: {err:#}");
            }
        }
    }

    /// Maximum weighted capacity units when this device has a bounded limit (`weight_units`,
    /// `runs_at_once`, or `default_run_limit`), or `None` when unlimited.
    fn bounded_weight_units(&self) -> Option<u32> {
        if self.ctx.config.weight_units.is_some() || self.run_limit() != usize::MAX {
            Some(device::effective_weight_units(
                &self.ctx.config,
                self.default_run_limit,
                self.host_memory,
            ))
        } else {
            None
        }
    }

    /// Whether a run costing `needed_units` exceeds the weighted budget in `resource_ledger`.
    /// Always checked on any bounded device (`weight_units`, `runs_at_once`, or
    /// `default_run_limit`) so an active heavy run's 4 units cannot be over-committed by
    /// subsequent default-weight workers or judges.
    pub(super) fn weighted_full(&self, needed_units: u32) -> bool {
        let Some(max_units) = self.bounded_weight_units() else {
            return false;
        };
        let reserve = device::reserve_units(&self.ctx.config);
        match self.db.free_units(max_units, reserve) {
            Ok(free) => needed_units > free,
            Err(err) => {
                tracing::warn!("reading free_units from resource_ledger: {err:#}");
                false
            }
        }
    }

    /// Weighted device load description with exact unit numbers (`active`, `max`, `reserve`,
    /// `free`, `needed`) for Trail Rule hold records and board notes whenever a weighted
    /// limit is configured or triggers a hold.
    pub(super) fn weighted_load(&self, needed_units: u32) -> (usize, String) {
        let max_units = self.bounded_weight_units().unwrap_or(0);
        let reserve = device::reserve_units(&self.ctx.config);
        let active = self.db.active_units().unwrap_or(0);
        let free = max_units.saturating_sub(reserve).saturating_sub(active);
        let others = self.other_turns();
        let mut text = format!(
            "device {} has {active} of {max_units} weight units active ({reserve} reserved, \
             {free} free, need {needed_units}): {} managed runs, {} turns of other threads",
            self.ctx.device,
            self.own_runs(),
            others.len()
        );
        if !others.is_empty() {
            let shown = others.iter().take(device::THREADS_SHOWN);
            let shown = shown.copied().collect::<Vec<_>>().join(", ");
            text.push_str(&format!(" ({shown}"));
            match others.len().saturating_sub(device::THREADS_SHOWN) {
                0 => {}
                more => text.push_str(&format!(" and {more} more")),
            }
            text.push(')');
        }
        if let Some(unknown) = self.load_unknown() {
            text.push_str(&format!(
                "; {unknown}, so the turns of other threads are not known and nothing starts"
            ));
        }
        (max_units as usize, text)
    }

    /// Builds the [`DeviceHeartbeat`] payload for `PUT /v1/devices/{name}/heartbeat` from
    /// host metadata, `[projects]`, config, and `daemon.db`'s live `resource_ledger`.
    pub fn build_heartbeat(&self) -> DeviceHeartbeat {
        self.build_heartbeat_at((self.clock)())
    }

    /// Like [`Daemon::build_heartbeat`], evaluated at an explicit `now` timestamp so repos
    /// currently in macOS TCC privacy backoff are excluded deterministically.
    pub(crate) fn build_heartbeat_at(&self, now: Timestamp) -> DeviceHeartbeat {
        let capacity =
            device::device_capacity(&self.ctx.config, self.default_run_limit, self.host_memory);
        let reserve = device::reserve_units(&self.ctx.config);
        let free_units = self
            .db
            .free_units(capacity.weight_units, reserve)
            .unwrap_or(0);
        DeviceHeartbeat {
            os: Some(device::host_os().to_owned()),
            arch: Some(device::host_arch().to_owned()),
            capacity: Some(capacity),
            free_units: Some(free_units),
            capabilities: self.advertised_capabilities(),
            runners: device::advertised_runners(&self.ctx.config, &self.runners.kinds()),
            repos: self.available_repos(now),
            protocol_features: device::effective_protocol_features(&self.ctx.config),
        }
    }

    /// Sends one `PUT /v1/devices/{name}/heartbeat` update, renewing active managed run
    /// leases first. Refreshes live interactive thread load for T3 runners before computing
    /// `free_units` so ended or newly started interactive turns never leave stale entries in
    /// `resource_ledger`. Returns `Ok(true)` when accepted, `Ok(false)` when gracefully
    /// disabled after a 404/405 from an older server, or `Err` on transient/auth failures
    /// (callers in the daemon loop log and continue without blocking dispatch).
    pub async fn send_heartbeat(&mut self, now: Timestamp) -> Result<bool> {
        if (!self.state.managed_runs.is_empty() || !self.bg_merges.is_empty())
            && let Err(err) = self.renew_runs(now).await
        {
            tracing::warn!("renewing managed run leases during heartbeat: {err:#}");
        }
        if self.heartbeat_unsupported {
            return Ok(false);
        }
        if self.runner.kind() == "t3" && self.runners.get("t3").is_some() {
            let running = device::running_threads(&*self.t3, &self.ctx.config).await;
            if running.unknown().is_none() {
                self.running = Some(running);
            }
        }
        self.sync_resource_ledger(now);
        let body = self.build_heartbeat_at(now);
        match self
            .client()
            .device_heartbeat(&self.ctx.device, &body)
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if is_heartbeat_unsupported(&err) => {
                self.heartbeat_unsupported = true;
                tracing::warn!(
                    "device heartbeat endpoint unsupported by server ({err:#}); disabling periodic heartbeats until restart"
                );
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }

    fn client(&self) -> &Client {
        &self.ctx.client
    }

    fn is_home(&self, board: &Board) -> bool {
        self.hosts(board) && board.status == BoardStatus::Active
    }

    /// An autopilot board with this device among its homes, whatever its status.
    fn hosts(&self, board: &Board) -> bool {
        board.autopilot && board.home_devices.contains(&self.ctx.device)
    }

    /// An active board with this device among its homes that is not on autopilot and has
    /// automatic verdict judges enabled: its pending proposals get verdict judges, and the
    /// board gets nothing else from this daemon (`docs/DESIGN.md`, "Daemon dispatch", Boards
    /// off autopilot).
    fn judges_proposals(&self, board: &Board) -> bool {
        !board.autopilot
            && board.status == BoardStatus::Active
            && board.verdict_judges
            && board.home_devices.contains(&self.ctx.device)
    }

    /// Dispatches the boards this device runs: in pull mode (default when the server supports
    /// `dispatch-claim-v1`), renews active managed run leases, pulls capability-matched work
    /// via `POST /v1/dispatch/claim`, executes zero-LLM merges/housekeeping via
    /// [`DeterministicRunner`], and delivers device sticky wakes. Falls back cleanly to
    /// legacy 45-minute board-lease dispatch against older servers returning 404/405 (or when
    /// `dispatch_mode = "legacy"`).
    pub async fn reconcile(&mut self, now: Timestamp) -> Result<()> {
        // A board that did not ask for a slot again in the last pass lost its place.
        if let Some(last) = self.last_pass.replace(now) {
            self.worker_turns.keep_asked_since(last);
            self.judge_turns.keep_asked_since(last);
            self.verdict_turns.keep_asked_since(last);
        }
        self.running = None;
        // Before the boards are dispatched, so that a hold past this age is written anew
        // in this pass.
        self.state.holds.retain(|_, at| now - *at < HOLD_KEPT);
        self.state.hold_notes.retain(|_, at| now - *at < HOLD_KEPT);
        self.reload_config();
        let boards = self.client().boards().await?;
        self.boards = boards.iter().map(|b| (b.slug.clone(), b.clone())).collect();
        self.refresh_usage(now).await;

        if self.is_pull_mode() && self.reconcile_pull(&boards, now).await? {
            return Ok(());
        }

        // Legacy board-lease dispatch path (used when `dispatch_mode = "legacy"` or when an
        // older server returns 404/405 for `POST /v1/dispatch/claim`).
        // The boards off autopilot come after the dispatched ones, which so get the
        // device's free runs first.
        let mut off_autopilot = Vec::new();
        for board in &boards {
            if !self.is_home(board) {
                self.failures.remove(&board.slug);
                self.dispatch_renewed.remove(&board.slug);
                // A paused board keeps its dispatcher named and starts no worker.
                if self.hosts(board) && board.status == BoardStatus::Paused {
                    continue;
                }
                if board.coordinator.as_ref() == Some(&self.dispatch_agent()) {
                    self.release(&board.slug).await;
                }
                if self.judges_proposals(board) {
                    off_autopilot.push(board);
                } else if !board.autopilot && !board.verdict_judges {
                    // Turning verdict judges off ends lease-less judge rights on the
                    // server; drop tracked runs so they neither leak in daemon.db nor
                    // trigger a spurious JUDGE_RETRY delay if re-enabled later.
                    let dropped: Vec<JudgeRun> = self
                        .state
                        .judges
                        .iter()
                        .filter(|r| r.board == board.slug)
                        .cloned()
                        .collect();
                    for run in &dropped {
                        self.record_drop(
                            "reconcile_dropped_judge",
                            Some(&board.slug),
                            run.tasks.first(),
                            None,
                            "autopilot and verdict_judges are both disabled on this board",
                            serde_json::to_value(run).unwrap_or_default(),
                            now,
                        );
                        if let Some(run_id) = run.run_id.as_deref() {
                            let _ = self.db.update_local_run_state(
                                run_id,
                                LocalRunState::Cancelled,
                                None,
                                now,
                            );
                        } else if let Some(thread) = run.thread.as_deref() {
                            let _ = self.db.finish_local_run_by_session(
                                thread,
                                LocalRunState::Cancelled,
                                now,
                            );
                        }
                    }
                    self.state.judges.retain(|r| r.board != board.slug);
                    for run in dropped {
                        if let Some(wt_id) = run.worktree_id.as_deref() {
                            self.cleanup_worktree(wt_id, now).await;
                        }
                    }
                }
                continue;
            }
            if let Err(err) = self.dispatch(board, now).await {
                tracing::warn!("dispatcher of {}: {err:#}", board.slug);
            }
        }
        for board in off_autopilot {
            // What keeps a board's proposals from their verdicts is written to the
            // device's trail, once until it is over.
            let subject = format!("the verdict judges of {}", board.slug);
            match self.judge_proposals(board, now).await {
                Ok(()) => self.hold_ended("verdicts", &subject),
                Err(err) => {
                    tracing::warn!("{subject}: {err:#}");
                    let cause = format!("{err:#}");
                    self.record_hold("verdicts", &board.slug, &subject, 0, &cause, now);
                }
            }
        }
        let boards = &self.boards;
        self.failures.retain(|slug, _| boards.contains_key(slug));
        let dropped_workers: Vec<(TaskId, Worker)> = self
            .state
            .workers
            .iter()
            .filter(|(task, _)| !boards.contains_key(&task.board()))
            .map(|(task, w)| (task.clone(), w.clone()))
            .collect();
        let dropped_judges: Vec<JudgeRun> = self
            .state
            .judges
            .iter()
            .filter(|r| !boards.contains_key(&r.board))
            .cloned()
            .collect();
        let dropped_owed: Vec<(Owed, &'static str)> = self
            .state
            .owed
            .iter()
            .filter_map(|step| {
                if !boards.contains_key(step.board()) {
                    Some((step.clone(), "board no longer present on server"))
                } else if now - step.at() >= WAKE_TTL {
                    Some((step.clone(), "owed step exceeded WAKE_TTL before reconcile"))
                } else {
                    None
                }
            })
            .collect();
        let mut dropped_worktrees: Vec<String> = Vec::new();
        for (task, worker) in &dropped_workers {
            let board_slug = task.board();
            self.record_drop(
                "reconcile_dropped_worker",
                Some(&board_slug),
                Some(task),
                None,
                "board no longer present on server",
                serde_json::to_value(worker).unwrap_or_default(),
                now,
            );
            let _ =
                self.db
                    .finish_local_runs_for_task("worker", task, LocalRunState::Cancelled, now);
            if let Some(wt_id) = worker.worktree_id.clone() {
                dropped_worktrees.push(wt_id);
            }
        }
        for run in &dropped_judges {
            self.record_drop(
                "reconcile_dropped_judge",
                Some(&run.board),
                run.tasks.first(),
                None,
                "board no longer present on server",
                serde_json::to_value(run).unwrap_or_default(),
                now,
            );
            if let Some(run_id) = run.run_id.as_deref() {
                let _ = self
                    .db
                    .update_local_run_state(run_id, LocalRunState::Cancelled, None, now);
            } else if let Some(thread) = run.thread.as_deref() {
                let _ = self
                    .db
                    .finish_local_run_by_session(thread, LocalRunState::Cancelled, now);
            }
            if let Some(wt_id) = run.worktree_id.clone() {
                dropped_worktrees.push(wt_id);
            }
        }
        for (step, cause) in &dropped_owed {
            self.record_drop(
                "reconcile_dropped_owed",
                Some(step.board()),
                match step {
                    Owed::Release { task, .. } => Some(task),
                    Owed::Answer { .. } => None,
                },
                None,
                cause,
                serde_json::to_value(step).unwrap_or_default(),
                now,
            );
        }
        let boards = &self.boards;
        self.state
            .workers
            .retain(|task, _| boards.contains_key(&task.board()));
        self.state.judges.retain(|r| boards.contains_key(&r.board));
        for wt_id in dropped_worktrees {
            self.cleanup_worktree(&wt_id, now).await;
        }
        self.cleanup_ended_worktrees(now).await;
        self.state
            .worker_retry
            .retain(|task, at| *at > now && boards.contains_key(&task.board()));
        self.state
            .worker_skips
            .retain(|task, _| boards.contains_key(&task.board()));
        self.state.idle_notices.retain(|task, n| {
            boards.contains_key(&task.board()) && now - n.noticed < USAGE_STOP_KEPT
        });
        self.state
            .owed
            .retain(|step| boards.contains_key(step.board()) && now - step.at() < WAKE_TTL);
        self.state
            .usage_stops
            .retain(|_, at| now - *at < USAGE_STOP_KEPT);
        self.dispatch_renewed
            .retain(|slug, _| boards.contains_key(slug));
        self.reconcile_stale_local_runs(now).await;
        self.sync_resource_ledger(now);
        self.save_at(now)
    }

    // -------------------------------------------------------------------------
    // Capability-matched pull dispatch (`POST /v1/dispatch/claim`, Slice 2.2)
    // -------------------------------------------------------------------------

    /// Available weighted resource units for `POST /v1/dispatch/claim` from `daemon.db`.
    fn free_units_for_claim(&self) -> u32 {
        let capacity =
            device::device_capacity(&self.ctx.config, self.default_run_limit, self.host_memory);
        let reserve = device::reserve_units(&self.ctx.config);
        self.db
            .free_units(capacity.weight_units, reserve)
            .unwrap_or(0)
    }

    /// Ensures this device's `dispatch:<device>` agent is registered with the server.
    async fn ensure_dispatch_registered(&mut self) -> Result<AgentId> {
        let agent = self.dispatch_agent();
        if !self.dispatch_registered {
            self.register_dispatch(&agent).await?;
            self.dispatch_registered = true;
        }
        Ok(agent)
    }

    /// Runs one capability-matched pull-dispatch reconcile pass. Returns `Ok(true)` when
    /// handled in pull mode, or `Ok(false)` in `DispatchMode::Auto` when the server returns
    /// 404/405 for `POST /v1/dispatch/claim` so the caller immediately falls back to legacy
    /// board-lease dispatch.
    async fn reconcile_pull(&mut self, boards: &[Board], now: Timestamp) -> Result<bool> {
        let pull_confirmed = self.dispatch_claim_supported == Some(true)
            || self.ctx.config.dispatch_mode == DispatchMode::Pull;
        if let Err(err) = self.renew_runs(now).await {
            tracing::warn!("renewing managed runs: {err:#}");
        }
        self.reconcile_stale_local_runs(now).await;
        if self.self_restart_draining(now) {
            return Ok(true);
        }
        if pull_confirmed && (!self.state.workers.is_empty() || !self.state.judges.is_empty()) {
            self.retire_legacy_runs(now).await;
        }
        let track_t3_load = pull_confirmed
            || device::run_limit(&self.ctx.config, self.default_run_limit) < usize::MAX
            || self.ctx.config.weight_units.is_some();
        if track_t3_load && self.runner.kind() == "t3" && self.runners.get("t3").is_some() {
            let running = device::running_threads(&*self.t3, &self.ctx.config).await;
            if pull_confirmed && let Some(unknown) = running.unknown() {
                tracing::warn!(
                    "{unknown}; holding LLM pull claims in this pass to protect device memory"
                );
                let hold_board = boards
                    .first()
                    .map(|b| b.slug.clone())
                    .unwrap_or_else(|| self.ctx.config.feedback_board());
                self.record_hold(
                    "pull",
                    &hold_board,
                    "pull_dispatch",
                    self.run_limit(),
                    &unknown,
                    now,
                );
            }
            self.running = Some(running);
        }
        let t3_load_unknown = self.load_unknown().is_some();
        self.sync_resource_ledger(now);

        let mut deferred_merge = self.merge_backoffs.has_active_wait(now);
        let mut merge_deferred_this_tick: BTreeMap<TaskId, Option<(Timestamp, i64)>> =
            BTreeMap::new();
        let mut deferred_review = self.review_waits.has_active_wait(now);
        let mut deferred_housekeeping = false;
        let mut claimed_any = false;
        let mut tcc_heartbeat_dirty = false;
        let has_llm_runner = !self.runners.is_empty();

        for _ in 0..PULL_CLAIMS_PER_TICK_MAX {
            if self.self_restart_draining(now) {
                break;
            }
            let free_units = self.free_units_for_claim();
            let needs_probe = self.dispatch_claim_supported.is_none()
                && self.ctx.config.dispatch_mode == DispatchMode::Auto;
            if free_units == 0 && !needs_probe {
                break;
            }
            let configured_repos = device::configured_repos(&self.ctx.config);
            let available_repos = self.available_repos(now);
            // When every configured `[projects]` repo on this device is in macOS TCC backoff
            // and no healthy `[board_projects]` checkout remains, withhold repo-dependent kinds
            // so an empty `req.repos` cannot fall back to a stale `device.repos` on the server
            // and re-claim failing work.
            let all_repos_backed_off = !configured_repos.is_empty()
                && available_repos.is_empty()
                && !self.has_healthy_board_project(now);

            let mut supported_kinds = Vec::new();
            if has_llm_runner && !t3_load_unknown && !all_repos_backed_off {
                for kind in [
                    RunKind::Worker,
                    RunKind::Review,
                    RunKind::Verdict,
                    RunKind::Triage,
                    RunKind::Goal,
                    RunKind::Curate,
                ] {
                    if kind == RunKind::Review && deferred_review {
                        continue;
                    }
                    if self.llm_spawn_backoffs.is_kind_held(kind, now) {
                        continue;
                    }
                    supported_kinds.push(kind);
                }
                supported_kinds.push(RunKind::Command);
            }
            if !deferred_merge && !all_repos_backed_off {
                supported_kinds.push(RunKind::Merge);
            }
            if !deferred_housekeeping {
                supported_kinds.push(RunKind::Housekeeping);
            }
            if supported_kinds.is_empty() {
                if !needs_probe {
                    break;
                }
                supported_kinds.push(RunKind::Housekeeping);
            }

            let req = DispatchClaimRequest {
                device: self.ctx.device.clone(),
                free_units: if t3_load_unknown && !needs_probe {
                    free_units.min(1)
                } else {
                    free_units
                },
                runners: device::claim_runners(&self.ctx.config, &self.runners.kinds()),
                capabilities: self.advertised_capabilities(),
                repos: available_repos,
                supported_kinds,
                quota_bucket: Some(self.ctx.config.quota_bucket()),
                max_claims: Some(1),
            };
            let resp = match self.client().dispatch_claim(&req).await {
                Ok(resp) => {
                    self.dispatch_claim_supported = Some(true);
                    resp
                }
                Err(err)
                    if is_heartbeat_unsupported(&err)
                        && self.ctx.config.dispatch_mode == DispatchMode::Auto =>
                {
                    self.dispatch_claim_supported = Some(false);
                    tracing::info!(
                        "server does not support POST /v1/dispatch/claim ({err:#}); falling back to legacy board-lease dispatch"
                    );
                    return Ok(false);
                }
                Err(err) => {
                    return Err(err.context("claiming work via POST /v1/dispatch/claim"));
                }
            };
            if resp.claimed.is_empty() {
                if !resp.holds_summary.is_empty() && !claimed_any {
                    tracing::debug!(
                        "dispatch/claim returned 0 runs (free_units={free_units}): {:?}",
                        resp.holds_summary
                    );
                }
                break;
            }
            claimed_any = true;
            let mut stop_pull_tick = false;
            let mut newly_tcc_backed_off = false;
            for claimed in resp.claimed {
                let exec = self
                    .execute_claimed_run(claimed, now, &mut merge_deferred_this_tick)
                    .await;
                deferred_merge |= exec.deferred_merge;
                deferred_review |= exec.deferred_review;
                deferred_housekeeping |= exec.deferred_housekeeping;
                newly_tcc_backed_off |= exec.tcc_backed_off;
                tcc_heartbeat_dirty |= exec.tcc_backed_off;
                stop_pull_tick |= exec.stop_pull_tick;
            }
            self.sync_resource_ledger(now);
            // When a newly triggered TCC backoff drops `available_repos` to empty while a
            // healthy `[board_projects]` remains, refresh the device heartbeat immediately
            // before the next claim iteration so `device.repos` on the server no longer
            // advertises the backed-off repo when `req.repos` is empty.
            if newly_tcc_backed_off
                && self.available_repos(now).is_empty()
                && !self.heartbeat_unsupported
            {
                let body = self.build_heartbeat_at(now);
                match self
                    .client()
                    .device_heartbeat(&self.ctx.device, &body)
                    .await
                {
                    Ok(_) => tcc_heartbeat_dirty = false,
                    Err(err) if is_heartbeat_unsupported(&err) => {
                        self.heartbeat_unsupported = true;
                        tcc_heartbeat_dirty = false;
                    }
                    Err(err) => {
                        tracing::warn!(
                            "refreshing device heartbeat after TCC repo backoff: {err:#}"
                        );
                    }
                }
            }
            if stop_pull_tick {
                break;
            }
        }
        if tcc_heartbeat_dirty && !self.heartbeat_unsupported {
            let body = self.build_heartbeat_at(now);
            if let Err(err) = self
                .client()
                .device_heartbeat(&self.ctx.device, &body)
                .await
            {
                if is_heartbeat_unsupported(&err) {
                    self.heartbeat_unsupported = true;
                } else {
                    tracing::warn!("refreshing device heartbeat after TCC repo backoff: {err:#}");
                }
            }
        }

        // Once pull mode is confirmed active, retire any carried-over legacy runs and release
        // any stale legacy 45m board coordinator leases held by this device so no board remains
        // pinned to one machine.
        if self.dispatch_claim_supported == Some(true) {
            if !pull_confirmed && (!self.state.workers.is_empty() || !self.state.judges.is_empty())
            {
                self.retire_legacy_runs(now).await;
            }
            if !pull_confirmed && let Some(unknown) = self.load_unknown() {
                let hold_board = boards
                    .first()
                    .map(|b| b.slug.clone())
                    .unwrap_or_else(|| self.ctx.config.feedback_board());
                self.record_hold(
                    "pull",
                    &hold_board,
                    "pull_dispatch",
                    self.run_limit(),
                    &unknown,
                    now,
                );
            }
            let dispatch_agent = self.dispatch_agent();
            for board in boards {
                if board.coordinator.as_ref() == Some(&dispatch_agent) {
                    self.release(&board.slug).await;
                }
            }
            self.state.owed.retain(|step| now - step.at() < WAKE_TTL);
            self.state
                .idle_notices
                .retain(|_, n| now - n.noticed < USAGE_STOP_KEPT);
        }

        if let Err(err) = self.poll_sticky_wakes(now).await {
            tracing::warn!("polling device sticky wakes: {err:#}");
        }
        if let Err(err) = self.flush_run_events(now).await {
            tracing::warn!("flushing live run events: {err:#}");
        }
        if !self.orphaned_transcripts_checked {
            let (_, complete) = self.reconcile_orphaned_transcripts_checked(now).await;
            self.orphaned_transcripts_checked = complete;
        }
        if self
            .last_token_backfill
            .is_none_or(|at| now - at >= TimeDelta::minutes(5))
        {
            self.last_token_backfill = Some(now);
            let _ = self.backfill_missing_run_tokens(now).await;
        }
        if let Err(err) = self.flush_telemetry_outbox(now).await {
            tracing::warn!("flushing telemetry outbox: {err:#}");
        }
        self.cleanup_ended_worktrees(now).await;
        self.state
            .merges
            .retain(|task, _| self.boards.contains_key(&task.board()));
        self.sync_resource_ledger(now);
        self.save_at(now)?;
        Ok(true)
    }

    /// Retires finished legacy `state.workers` and `state.judges` carried across an upgrade
    /// from legacy board-lease dispatch into pull mode so their `cost_units` and worktrees do
    /// not stay leaked in `daemon.db`.
    async fn retire_legacy_runs(&mut self, now: Timestamp) {
        let worker_tasks: Vec<TaskId> = self.state.workers.keys().cloned().collect();
        for task_id in worker_tasks {
            let Some(worker) = self.state.workers.get(&task_id).cloned() else {
                continue;
            };
            let Some(thread) = worker.thread.as_deref() else {
                if now - worker.started_at >= RUN_START_GRACE {
                    self.state.workers.remove(&task_id);
                    if let Some(wt_id) = worker.worktree_id.as_deref() {
                        self.cleanup_worktree(wt_id, now).await;
                    }
                }
                continue;
            };
            let runner_kind = worker.runner.as_deref().unwrap_or(self.runner.kind());
            let runner = match self.runners.resolve(Some(runner_kind)) {
                Ok(r) => r,
                Err(_) => Arc::clone(&self.runner),
            };
            let mut handle = RunHandle::new(runner.kind(), thread);
            if let Some(wd) = &worker.worktree_path {
                handle = handle.with_workdir(wd);
            }
            let ended = match tokio::time::timeout(self.timing.thread_check, runner.status(&handle))
                .await
            {
                Ok(Ok(status)) => {
                    !status.runnable || (!status.busy && status.runs.unwrap_or(0) > 0)
                }
                _ => false,
            };
            if ended {
                if let Some(run_id) = worker.run_id.as_deref() {
                    let _ =
                        self.db
                            .update_local_run_state(run_id, LocalRunState::Finished, None, now);
                } else {
                    let _ = self.db.finish_local_runs_for_task(
                        "worker",
                        &task_id,
                        LocalRunState::Finished,
                        now,
                    );
                }
                self.state.workers.remove(&task_id);
                if let Some(wt_id) = worker.worktree_id.as_deref() {
                    self.cleanup_worktree(wt_id, now).await;
                }
            }
        }
        let judges = self.state.judges.clone();
        let mut keep = Vec::with_capacity(judges.len());
        for judge in judges {
            let Some(thread) = judge.thread.as_deref() else {
                if now - judge.started_at < RUN_START_GRACE {
                    keep.push(judge);
                } else if let Some(wt_id) = judge.worktree_id.as_deref() {
                    self.cleanup_worktree(wt_id, now).await;
                }
                continue;
            };
            let runner_kind = judge.runner.as_deref().unwrap_or(self.runner.kind());
            let runner = match self.runners.resolve(Some(runner_kind)) {
                Ok(r) => r,
                Err(_) => Arc::clone(&self.runner),
            };
            let mut handle = RunHandle::new(runner.kind(), thread);
            if let Some(wd) = &judge.worktree_path {
                handle = handle.with_workdir(wd);
            }
            let ended = match tokio::time::timeout(self.timing.thread_check, runner.status(&handle))
                .await
            {
                Ok(Ok(status)) => {
                    !status.runnable || (!status.busy && status.runs.unwrap_or(0) > 0)
                }
                _ => false,
            };
            if ended {
                if let Some(run_id) = judge.run_id.as_deref() {
                    let _ =
                        self.db
                            .update_local_run_state(run_id, LocalRunState::Finished, None, now);
                } else {
                    let _ =
                        self.db
                            .finish_local_run_by_session(thread, LocalRunState::Finished, now);
                }
                if let Some(wt_id) = judge.worktree_id.as_deref() {
                    self.cleanup_worktree(wt_id, now).await;
                }
            } else {
                keep.push(judge);
            }
        }
        self.state.judges = keep;
    }

    /// Executes one `ClaimedRun` returned by `POST /v1/dispatch/claim` with write-before-act
    /// intent persistence in `daemon.db` (`local_runs` + `resource_ledger`).
    async fn execute_claimed_run(
        &mut self,
        claimed: ClaimedRun,
        now: Timestamp,
        merge_deferred_this_tick: &mut BTreeMap<TaskId, Option<(Timestamp, i64)>>,
    ) -> ClaimedExecution {
        let run_id = claimed.run.id.clone();
        let run_id_str = run_id.as_str().to_owned();
        let resource_entry_id = format!("run:{run_id}");
        let mut local_run = LocalRunRecord {
            run_id: run_id_str.clone(),
            task_id: claimed.task_id.clone(),
            board: claimed.board.clone(),
            kind: run_kind_str(claimed.run.kind).to_owned(),
            runner: claimed.run.runner.clone(),
            session_id: None,
            worktree_path: None,
            branch: None,
            cost_units: claimed.run.cost_units,
            pid: None,
            state: LocalRunState::Intent,
            lease_expires_at: claimed.run.lease_expires_at,
            created_at: now,
            updated_at: now,
            meta_json: serde_json::to_string(&claimed).ok(),
        };
        if let Err(err) = self.db.upsert_local_run(&local_run) {
            tracing::warn!("persisting intent for claimed run {run_id}: {err:#}");
            self.fail_claimed_run(
                &claimed,
                &format!("failed to persist local run intent: {err:#}"),
                now,
            )
            .await;
            return ClaimedExecution {
                stop_pull_tick: true,
                ..ClaimedExecution::default()
            };
        }
        let ledger_entry = ResourceEntry {
            entry_id: resource_entry_id.clone(),
            origin: ResourceOrigin::Managed,
            agent_id: None,
            run_id: Some(run_id_str.clone()),
            cost_units: claimed.run.cost_units,
            updated_at: now,
        };
        if let Err(err) = self.db.upsert_resource(&ledger_entry) {
            tracing::warn!("recording resource_ledger intent for run {run_id}: {err:#}");
        }

        match claimed.run.kind {
            RunKind::Merge => {
                self.execute_claimed_merge(&claimed, &mut local_run, now, merge_deferred_this_tick)
                    .await
            }
            RunKind::Housekeeping => {
                self.execute_claimed_housekeeping(&claimed, &mut local_run, now)
                    .await
            }
            RunKind::Worker
            | RunKind::Review
            | RunKind::Verdict
            | RunKind::Triage
            | RunKind::Goal
            | RunKind::Curate
            | RunKind::Command => {
                let mut observed_task_stamp: Option<(Timestamp, i64)> = None;
                match self
                    .spawn_claimed_llm_run(&claimed, &mut local_run, &mut observed_task_stamp, now)
                    .await
                {
                    Ok(exec) => exec,
                    Err(err) => {
                        tracing::warn!(
                            "spawning claimed {} run {run_id} on {}: {err:#}",
                            run_kind_str(claimed.run.kind),
                            claimed.board
                        );
                        if claimed.run.kind != RunKind::Command {
                            self.llm_spawn_backoffs.record_failure(
                                &claimed,
                                observed_task_stamp,
                                now,
                            );
                        }
                        self.fail_claimed_run(&claimed, &format!("{err:#}"), now)
                            .await;
                        ClaimedExecution {
                            stop_pull_tick: true,
                            ..ClaimedExecution::default()
                        }
                    }
                }
            }
            RunKind::Interactive | RunKind::Unknown => {
                self.fail_claimed_run(
                    &claimed,
                    "unsupported run kind returned by dispatch/claim",
                    now,
                )
                .await;
                ClaimedExecution::default()
            }
        }
    }

    async fn execute_claimed_merge(
        &mut self,
        claimed: &ClaimedRun,
        local_run: &mut LocalRunRecord,
        now: Timestamp,
        merge_deferred_this_tick: &mut BTreeMap<TaskId, Option<(Timestamp, i64)>>,
    ) -> ClaimedExecution {
        let run_id = &claimed.run.id;
        let Some(task_id) = claimed.task_id.as_ref() else {
            self.fail_claimed_run(claimed, "merge run has no task_id", now)
                .await;
            return ClaimedExecution {
                deferred_merge: true,
                ..ClaimedExecution::default()
            };
        };
        // Older servers without server-side per-task merge cooldown re-offer the exact same
        // task immediately inside the same reconcile tick with no intervening note/update.
        // If the task DID change mid-tick (new note or update), a new server legitimately
        // cleared cooldown and we execute it rather than cancelling or engaging backoff.
        if let Some(prev_stamp) = merge_deferred_this_tick.get(task_id).copied() {
            let current_stamp = self.client().task(task_id).await.ok().map(|d| {
                (
                    d.task.updated_at,
                    d.notes.iter().map(|n| n.id).max().unwrap_or(0),
                )
            });
            let unchanged = match (prev_stamp, current_stamp) {
                (Some(prev), Some(curr)) => prev == curr,
                _ => true,
            };
            if unchanged {
                let update = UpdateRun {
                    state: Some(RunState::Cancelled),
                    reason: Some(
                        "skipped duplicate merge claim within same tick (older server)".to_owned(),
                    ),
                    ..UpdateRun::default()
                };
                if let Err(err) = self.client().update_run(run_id, &update).await {
                    tracing::warn!("cancelling duplicate merge run {run_id}: {err:#}");
                }
                let _ = self.db.update_local_run_state(
                    run_id.as_str(),
                    LocalRunState::Cancelled,
                    None,
                    now,
                );
                let _ = self.db.remove_resource(&format!("run:{run_id}"));
                self.merge_backoffs
                    .record(task_id.clone(), now + PR_READ_EVERY, now);
                return ClaimedExecution {
                    deferred_merge: true,
                    ..ClaimedExecution::default()
                };
            }
        }
        let agent = match self.ensure_dispatch_registered().await {
            Ok(agent) => agent,
            Err(err) => {
                self.fail_claimed_run(
                    claimed,
                    &format!("registering dispatch agent: {err:#}"),
                    now,
                )
                .await;
                return ClaimedExecution {
                    deferred_merge: true,
                    ..ClaimedExecution::default()
                };
            }
        };
        if let Err(err) = self
            .client()
            .update_run(
                run_id,
                &UpdateRun {
                    state: Some(RunState::Running),
                    agent_id: Some(Some(agent.clone())),
                    lease_ttl_secs: Some(RUN_LEASE_TTL_SECS_DEFAULT),
                    ..UpdateRun::default()
                },
            )
            .await
        {
            self.fail_claimed_run(
                claimed,
                &format!("transitioning merge run to running: {err:#}"),
                now,
            )
            .await;
            return ClaimedExecution {
                deferred_merge: true,
                ..ClaimedExecution::default()
            };
        }
        local_run.runner = DETERMINISTIC_RUNNER_KIND.to_owned();
        local_run.state = LocalRunState::Running;
        local_run.updated_at = now;
        let _ = self.db.upsert_local_run(local_run);

        let board = match self.boards.get(&claimed.board).cloned() {
            Some(board) => board,
            None => match self.client().board(&claimed.board).await {
                Ok(board) => {
                    self.boards.insert(board.slug.clone(), board.clone());
                    board
                }
                Err(err) => {
                    self.fail_claimed_run(claimed, &format!("reading board: {err:#}"), now)
                        .await;
                    return ClaimedExecution {
                        deferred_merge: true,
                        ..ClaimedExecution::default()
                    };
                }
            },
        };
        let config_snapshot = self.ctx.config.clone();
        let home_root = self.ctx.home.root().to_path_buf();
        let agent_client = self.client().with_agent(Some(agent.clone()));

        // Long-running deterministic `after-merge-deploy` runs invoke
        // `deploy/upgrade.sh --yes --no-restart-local <sha>` which can take several minutes
        // (> `reconcile_budget` 90s). Spawn them off the reconcile loop with periodic server
        // lease renewal so they are never SIGKILLed mid-upgrade, finish `task_done` +
        // `PATCH /v1/runs/{id} state=succeeded` cleanly, and only THEN trigger a detached
        // graceful self-restart when the installed binary commit actually changed.
        if claimed
            .run
            .title
            .as_deref()
            .is_some_and(|t| t.starts_with("after-merge-deploy "))
        {
            let run_id_owned = run_id.clone();
            let train_tasks = if claimed.tasks.is_empty() {
                vec![task_id.clone()]
            } else {
                claimed.tasks.clone()
            };
            let agent_owned = agent.clone();
            let run_client = self.client().clone();
            let gh = Arc::clone(&self.gh);
            let deployer = TrackingDeployer::new(Arc::clone(&self.upgrade_deployer));
            let board_owned = board.clone();
            let prev_outcome = claimed.run.outcome.clone();
            let had_conflict_keys = !claimed.conflict_keys.is_empty();
            let handle = tokio::spawn(async move {
                let runner = DeterministicRunner::new();
                let mut local_merges = BTreeMap::new();
                let mut renew_interval = tokio::time::interval(Duration::from_secs(60));
                renew_interval.tick().await;
                let deploy_fut = runner.execute_deploy_train_with_deployer(
                    &board_owned,
                    &train_tasks,
                    &agent_owned,
                    &agent_client,
                    &*gh,
                    &deployer,
                    |task| {
                        project_for_task_config(&config_snapshot, &board_owned, Some(task))
                            .map(|(_, p)| p)
                            .unwrap_or_else(|| home_root.clone())
                    },
                    &mut local_merges,
                    now,
                );
                tokio::pin!(deploy_fut);
                let result = loop {
                    tokio::select! {
                        res = &mut deploy_fut => break res,
                        _ = renew_interval.tick() => {
                            let renew = UpdateRun {
                                lease_ttl_secs: Some(board_proto::RUN_LEASE_TTL_SECS_DEFAULT),
                                ..UpdateRun::default()
                            };
                            if let Err(err) = run_client.update_run(&run_id_owned, &renew).await {
                                tracing::warn!(
                                    "renewing lease for background after-merge-deploy run {run_id_owned}: {err:#}"
                                );
                            }
                        }
                    }
                };
                let check_self_restart = deployer.was_invoked();
                match result {
                    Ok(outcome) => {
                        finish_claimed_merge_run(
                            &run_client,
                            &run_id_owned,
                            &outcome,
                            prev_outcome.as_ref(),
                            had_conflict_keys,
                            now,
                        )
                        .await;
                        BgMergeResult {
                            local_state: LocalRunState::Finished,
                            check_self_restart,
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            "background after-merge-deploy run {run_id_owned} failed: {err:#}"
                        );
                        let update = UpdateRun {
                            state: Some(RunState::Failed),
                            reason: Some(format!("{err:#}")),
                            ..UpdateRun::default()
                        };
                        if let Err(uerr) = run_client.update_run(&run_id_owned, &update).await {
                            tracing::warn!(
                                "marking background merge run {run_id_owned} failed: {uerr:#}"
                            );
                        }
                        BgMergeResult {
                            local_state: LocalRunState::Failed,
                            check_self_restart,
                        }
                    }
                }
            });
            self.bg_merges
                .insert(run_id.clone(), (task_id.clone(), handle));
            return ClaimedExecution::default();
        }

        let deployer = TrackingDeployer::new(Arc::clone(&self.upgrade_deployer));
        let result = self
            .deterministic
            .execute_merge_with_deployer(
                &board,
                task_id,
                &agent,
                &agent_client,
                &*self.gh,
                &deployer,
                |task| {
                    project_for_task_config(&config_snapshot, &board, Some(task))
                        .map(|(_, p)| p)
                        .unwrap_or_else(|| home_root.clone())
                },
                &mut self.state.merges,
                now,
            )
            .await;

        let exec = ClaimedExecution::default();
        match result {
            Ok(outcome) => {
                // Record `MergeOutcome::Unclear` as `RunState::Succeeded` with structured
                // `{"status": "unclear", ...}` outcome (never `Failed`) so task-data errors
                // never trip `signal:runner:spawn_fail` alerts (`agent-board-699`).
                // Never pause `RunKind::Merge` globally on Deferred/Noop/Unclear: the server
                // enforces per-task cooldowns so other ready merges proceed immediately in the
                // same tick. Track `task_id` in `merge_deferred_this_tick` only as a local
                // fallback guard if an older server immediately re-offers the exact same
                // unchanged task.
                if outcome.is_deferred()
                    || matches!(
                        outcome,
                        MergeOutcome::Noop { .. } | MergeOutcome::Unclear { .. }
                    )
                {
                    let stamp = self.client().task(task_id).await.ok().map(|d| {
                        (
                            d.task.updated_at,
                            d.notes.iter().map(|n| n.id).max().unwrap_or(0),
                        )
                    });
                    merge_deferred_this_tick.insert(task_id.clone(), stamp);
                }
                finish_claimed_merge_run(
                    self.client(),
                    run_id,
                    &outcome,
                    claimed.run.outcome.as_ref(),
                    !claimed.conflict_keys.is_empty(),
                    now,
                )
                .await;
                let _ = self.db.update_local_run_state(
                    run_id.as_str(),
                    LocalRunState::Finished,
                    None,
                    now,
                );
                let _ = self.db.remove_resource(&format!("run:{run_id}"));
            }
            Err(err) => {
                let stamp = self.client().task(task_id).await.ok().map(|d| {
                    (
                        d.task.updated_at,
                        d.notes.iter().map(|n| n.id).max().unwrap_or(0),
                    )
                });
                merge_deferred_this_tick.insert(task_id.clone(), stamp);
                self.fail_claimed_run(claimed, &format!("{err:#}"), now)
                    .await;
            }
        }
        if deployer.was_invoked() {
            self.self_restart_needed = true;
            self.maybe_trigger_self_restart(now).await;
        }
        exec
    }

    async fn execute_claimed_housekeeping(
        &mut self,
        claimed: &ClaimedRun,
        local_run: &mut LocalRunRecord,
        now: Timestamp,
    ) -> ClaimedExecution {
        let run_id = &claimed.run.id;
        let Some(action) = claimed.housekeeping_action.as_ref() else {
            self.fail_claimed_run(claimed, "housekeeping run has no housekeeping_action", now)
                .await;
            return ClaimedExecution {
                deferred_housekeeping: true,
                ..ClaimedExecution::default()
            };
        };
        let agent = match self.ensure_dispatch_registered().await {
            Ok(agent) => agent,
            Err(err) => {
                self.fail_claimed_run(
                    claimed,
                    &format!("registering dispatch agent: {err:#}"),
                    now,
                )
                .await;
                return ClaimedExecution {
                    deferred_housekeeping: true,
                    ..ClaimedExecution::default()
                };
            }
        };
        if let Err(err) = self
            .transition_run_starting_to_running(run_id, Some(&agent), None, None)
            .await
        {
            self.fail_claimed_run(
                claimed,
                &format!("transitioning housekeeping run to running: {err:#}"),
                now,
            )
            .await;
            return ClaimedExecution {
                deferred_housekeeping: true,
                ..ClaimedExecution::default()
            };
        }
        local_run.runner = DETERMINISTIC_RUNNER_KIND.to_owned();
        local_run.state = LocalRunState::Running;
        local_run.updated_at = now;
        let _ = self.db.upsert_local_run(local_run);

        let agent_client = self.client().with_agent(Some(agent.clone()));
        let result = self
            .deterministic
            .execute_housekeeping(action, &agent, &agent_client, &self.state.workers)
            .await;
        let mut exec = ClaimedExecution::default();
        match result {
            Ok(outcome) => {
                if let HousekeepingOutcome::Opened { ref task_id, .. } = outcome {
                    if let Some(worker) = self.state.workers.remove(task_id)
                        && let Some(wt_id) = worker.worktree_id.as_deref()
                    {
                        self.cleanup_worktree(wt_id, now).await;
                    }
                    self.state.worker_skips.remove(task_id);
                    self.state.worker_retry.remove(task_id);
                    self.state.idle_notices.remove(task_id);
                } else {
                    exec.deferred_housekeeping = true;
                }
                let update = UpdateRun {
                    state: Some(RunState::Succeeded),
                    outcome: Some(Some(outcome.to_outcome_json())),
                    ..UpdateRun::default()
                };
                if let Err(err) = self.client().update_run(run_id, &update).await {
                    tracing::warn!("marking housekeeping run {run_id} succeeded: {err:#}");
                }
                let _ = self.db.update_local_run_state(
                    run_id.as_str(),
                    LocalRunState::Finished,
                    None,
                    now,
                );
                let _ = self.db.remove_resource(&format!("run:{run_id}"));
            }
            Err(err) => {
                exec.deferred_housekeeping = true;
                self.fail_claimed_run(claimed, &format!("{err:#}"), now)
                    .await;
            }
        }
        exec
    }

    async fn spawn_claimed_llm_run(
        &mut self,
        claimed: &ClaimedRun,
        local_run: &mut LocalRunRecord,
        observed_task_stamp: &mut Option<(Timestamp, i64)>,
        now: Timestamp,
    ) -> Result<ClaimedExecution> {
        let run_id = &claimed.run.id;
        let runner = self.runners.resolve(Some(&claimed.run.runner))?;
        let runner_kind = runner.kind().to_owned();
        let board = match self.boards.get(&claimed.board).cloned() {
            Some(board) => board,
            None => {
                let board = self.client().board(&claimed.board).await?;
                self.boards.insert(board.slug.clone(), board.clone());
                board
            }
        };
        let task_detail = match &claimed.task_id {
            Some(tid) => match self.client().task(tid).await {
                Ok(detail) => Some(detail),
                Err(err)
                    if claimed.run.kind == RunKind::Worker
                        && error_code(&err) == Some(ErrorCode::NotFound) =>
                {
                    let cause = format!("task {tid} not found before worker start");
                    self.cancel_stale_claimed_worker(
                        claimed,
                        &cause,
                        serde_json::json!({
                            "run_id": run_id,
                            "task": tid,
                            "error": "not_found",
                        }),
                        now,
                    )
                    .await;
                    return Ok(ClaimedExecution::default());
                }
                Err(err) => return Err(err),
            },
            None => None,
        };
        let current_stamp = task_detail.as_ref().map(|d| {
            (
                d.task.updated_at,
                d.notes.iter().map(|n| n.id).max().unwrap_or(0),
            )
        });
        *observed_task_stamp = current_stamp;
        // Older servers without server-side fast spawn-failure backoff re-offer the exact same
        // unchanged target inside its backoff window. Cancel the duplicate without adding another
        // Failed run and hold the kind locally until expiry or a task update.
        if claimed.run.kind != RunKind::Command
            && let Some((until, attempts)) =
                self.llm_spawn_backoffs
                    .duplicate_in_backoff(claimed, current_stamp, now)
        {
            let reason = clip_run_reason(&format!(
                "skipped duplicate {} claim during spawn-failure backoff until {} after {attempts} fast failure(s) (older server)",
                run_kind_str(claimed.run.kind),
                until.to_rfc3339()
            ));
            self.record_drop(
                "llm_spawn_backoff_cancelled",
                Some(&claimed.board),
                claimed.task_id.as_ref(),
                None,
                &reason,
                serde_json::json!({
                    "run_id": run_id,
                    "kind": claimed.run.kind,
                    "runner": claimed.run.runner,
                    "attempts": attempts,
                    "backoff_until": until,
                }),
                now,
            );
            if let Err(err) = self
                .client()
                .update_run(
                    run_id,
                    &UpdateRun {
                        state: Some(RunState::Cancelled),
                        reason: Some(reason),
                        ..UpdateRun::default()
                    },
                )
                .await
            {
                tracing::warn!("cancelling backed-off LLM run {run_id} on server: {err:#}");
            }
            let _ = self.db.update_local_run_state(
                run_id.as_str(),
                LocalRunState::Cancelled,
                None,
                now,
            );
            let _ = self.db.remove_resource(&format!("run:{run_id}"));
            return Ok(ClaimedExecution {
                stop_pull_tick: true,
                ..ClaimedExecution::default()
            });
        }
        if claimed.run.kind == RunKind::Worker
            && let Some(detail) = task_detail.as_ref()
            && !worker_eligible(&detail.task)
        {
            let cause = worker_ineligible_cause(&detail.task);
            self.cancel_stale_claimed_worker(
                claimed,
                &cause,
                worker_ineligible_subject(&detail.task, Some(run_id)),
                now,
            )
            .await;
            return Ok(ClaimedExecution::default());
        }
        let (repo_key, project) =
            self.host_project_for_run(&board, task_detail.as_ref().map(|d| &d.task))?;
        let repo_keys = repo_keys_for_run(
            repo_key.as_deref(),
            &board,
            task_detail.as_ref().map(|d| &d.task),
        );
        if let Some(active) = self
            .tcc_repo_backoffs
            .active_for(&repo_keys, &project, now)
            .cloned()
        {
            let reason = clip_run_reason(&format!(
                "repo backed off on {} until {} after macOS TCC privacy refusal on {}: {}",
                self.ctx.device,
                active.until.to_rfc3339(),
                active.project.display(),
                active.error
            ));
            // Record before cancelling (Trail Rule).
            self.record_drop(
                "tcc_repo_backoff_cancelled",
                Some(&claimed.board),
                claimed.task_id.as_ref(),
                None,
                &reason,
                serde_json::json!({
                    "run_id": run_id,
                    "kind": claimed.run.kind,
                    "repos": repo_keys,
                    "project": project,
                    "backoff_until": active.until,
                    "error": active.error,
                }),
                now,
            );
            tracing::info!(
                "{}: {reason}; cancelling claimed {} run {run_id}",
                claimed.board,
                run_kind_str(claimed.run.kind)
            );
            if let Err(err) = self
                .client()
                .update_run(
                    run_id,
                    &UpdateRun {
                        state: Some(RunState::Cancelled),
                        reason: Some(reason),
                        ..UpdateRun::default()
                    },
                )
                .await
            {
                tracing::warn!("cancelling TCC-backed-off run {run_id} on server: {err:#}");
            }
            let _ = self.db.update_local_run_state(
                run_id.as_str(),
                LocalRunState::Cancelled,
                None,
                now,
            );
            let _ = self.db.remove_resource(&format!("run:{run_id}"));
            return Ok(ClaimedExecution {
                stop_pull_tick: true,
                ..ClaimedExecution::default()
            });
        }

        match self
            .spawn_claimed_llm_run_for_project(
                claimed,
                local_run,
                &board,
                task_detail.as_ref(),
                runner,
                runner_kind,
                repo_key,
                &project,
                now,
            )
            .await
        {
            Ok((exec, git_verified)) => {
                if git_verified {
                    self.clear_tcc_repo_backoff(&repo_keys, &project);
                }
                Ok(exec)
            }
            Err(err) => {
                if let Some(refusal) = crate::worktree::privacy_refusal(&err) {
                    let (until, trail_path) = self.record_tcc_repo_backoff(
                        &board.slug,
                        claimed.task_id.as_ref(),
                        Some(run_id),
                        &repo_keys,
                        &project,
                        &refusal.detail,
                        now,
                    );
                    let trail_note = if trail_path.is_some() {
                        "; trail: state/daemon-drops.jsonl"
                    } else {
                        ""
                    };
                    let reason = format!(
                        "{err:#} (backed off on {} until {}{trail_note})",
                        self.ctx.device,
                        until.to_rfc3339()
                    );
                    self.fail_claimed_run(claimed, &reason, now).await;
                    return Ok(ClaimedExecution {
                        tcc_backed_off: true,
                        ..ClaimedExecution::default()
                    });
                }
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_claimed_llm_run_for_project(
        &mut self,
        claimed: &ClaimedRun,
        local_run: &mut LocalRunRecord,
        board: &Board,
        task_detail: Option<&board_proto::TaskDetail>,
        runner: Arc<dyn Runner>,
        runner_kind: String,
        repo_key: Option<String>,
        project: &Path,
        now: Timestamp,
    ) -> Result<(ClaimedExecution, bool)> {
        let run_id = &claimed.run.id;
        let project = project.to_path_buf();

        let mut after_merge_review = false;
        let include_t3_settle = runner_kind == "t3";
        let (worktree_spec, prompt, model, title) = match claimed.run.kind {
            RunKind::Worker => {
                let task_id = claimed
                    .task_id
                    .as_ref()
                    .context("worker run missing task_id")?;
                let base_branch = self
                    .ctx
                    .config
                    .base_branch(&board.slug, repo_key.as_deref());
                let wt =
                    crate::worktree::worker_worktree_with_base(&project, task_id, base_branch, now)
                        .await
                        .context("preparing worker worktree")?;
                let title = claimed.run.title.clone().unwrap_or_else(|| {
                    task_detail
                        .as_ref()
                        .map(|d| format!("{} {}", d.task.id, d.task.title))
                        .unwrap_or_else(|| format!("work {task_id}"))
                });
                let brief = match task_detail.as_ref() {
                    Some(detail) => {
                        let base = crate::brief::worker_brief_for_task_with_base(
                            &board.slug,
                            &detail.task,
                            crate::brief::WORKER_CLOSER,
                            include_t3_settle,
                            base_branch,
                        );
                        let prepacked = prepack::format_worker_context(
                            detail,
                            wt.as_ref().map(|w| w.branch.as_str()),
                            repo_key.as_deref(),
                        );
                        crate::brief::append_prepacked_context(base, &prepacked)
                    }
                    None => {
                        let mut base = crate::brief::worker_brief_with_options(
                            &board.slug,
                            task_id,
                            crate::brief::WORKER_CLOSER,
                            crate::brief::WorkerBriefOptions {
                                include_climbs: false,
                                include_revisits: false,
                                include_t3_settle,
                            },
                        );
                        crate::brief::append_base_branch_assignment(&mut base, base_branch);
                        base
                    }
                };
                (wt, brief, self.ctx.config.thread_model.clone(), title)
            }
            RunKind::Review => {
                let detail = task_detail
                    .as_ref()
                    .context("review run missing task detail")?;
                let review_runs = match self
                    .client()
                    .list_runs(&board_proto::RunFilter {
                        task: Some(detail.task.id.clone()),
                        kind: Some(RunKind::Review),
                        origin: Some(board_proto::RunOrigin::Managed),
                        limit: Some(50),
                        ..board_proto::RunFilter::default()
                    })
                    .await
                {
                    Ok(runs) => runs,
                    Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => Vec::new(),
                    Err(err) => {
                        return Err(
                            err.context(format!("listing review runs for {}", detail.task.id))
                        );
                    }
                };
                let after_merge_pass = {
                    let from_standing =
                        match crate::deterministic_runner::standing_review(detail, &review_runs) {
                            crate::deterministic_runner::StandingReview::Passed(pass)
                                if !judges::after_merge_checks(&pass.text).is_empty()
                                    && judges::has_after_merge_ready_note_after(
                                        detail, pass.id,
                                    ) =>
                            {
                                Some(pass)
                            }
                            _ => None,
                        };
                    // Also trust a server-dispatched `after-merge ...` review when the
                    // pre-merge pass was authorized via legacy `judges`/`board_updated`
                    // records without a `JUDGE_NOTE` (still verified by the dispatcher's
                    // own `AFTER_MERGE_READY_NOTE` after that pass).
                    from_standing.or_else(|| {
                        let server_marked = claimed
                            .run
                            .title
                            .as_deref()
                            .is_some_and(|t| t.starts_with("after-merge "));
                        if !server_marked {
                            return None;
                        }
                        judges::pending_after_merge_pass(detail)
                    })
                };
                let tasks = if claimed.tasks.is_empty() {
                    vec![detail.task.id.clone()]
                } else {
                    claimed.tasks.clone()
                };
                if let Some(pass) = after_merge_pass {
                    after_merge_review = true;
                    let info = judges::after_merge_ready_info(detail, pass.id);
                    let pr_url = info.as_ref().and_then(|i| i.pr_url.clone()).or_else(|| {
                        match judges::passed_pr(detail, pass, &board.repos) {
                            judges::PassedPr::Url(url) => Some(url),
                            _ => None,
                        }
                    });
                    let base_ref = info.as_ref().and_then(|i| i.base_ref.as_deref());
                    let merge_commit = info.as_ref().and_then(|i| i.merge_commit.as_deref());
                    let default_base = self
                        .ctx
                        .config
                        .base_branch(&board.slug, repo_key.as_deref());
                    let (wt, worktree_note) = self
                        .prepare_after_merge_worktree(
                            &detail.task.id,
                            &project,
                            pr_url.as_deref(),
                            base_ref,
                            merge_commit,
                            default_base,
                            now,
                        )
                        .await?;
                    let brief = crate::brief::judge_brief_with_options(
                        crate::brief::JudgeKind::AfterMerge,
                        &board.slug,
                        &tasks,
                        worktree_note.as_deref(),
                        include_t3_settle,
                    );
                    let mut prepacked = prepack::format_review_context(detail);
                    for extra_id in tasks.iter().filter(|id| *id != &detail.task.id) {
                        if let Ok(extra_detail) = self.client().task(extra_id).await {
                            prepacked.push_str("\n\n---\n\n");
                            prepacked.push_str(&prepack::format_review_context(&extra_detail));
                        }
                    }
                    let brief = crate::brief::append_prepacked_context(brief, &prepacked);
                    let title = claimed.run.title.clone().unwrap_or_else(|| {
                        format!("after-merge {} {}", detail.task.id, detail.task.title)
                    });
                    let effort = classify_review_effort(&detail.task, true, &[], false);
                    let model = self.ctx.config.model_for_review(effort, &runner_kind);
                    tracing::debug!(
                        "{}: after-merge review effort={effort:?} runner={runner_kind}",
                        detail.task.id
                    );
                    (wt, brief, model, title)
                } else {
                    let observed_at = detail
                        .notes
                        .iter()
                        .map(|n| n.at)
                        .chain(std::iter::once(detail.task.updated_at))
                        .max()
                        .unwrap_or(detail.task.updated_at);
                    let (wt, worktree_note, pr_view) = match self
                        .prepare_claimed_review_worktree(
                            board,
                            detail,
                            repo_key.as_deref(),
                            &project,
                            now,
                        )
                        .await?
                    {
                        ReviewPrep::Ready(wt, note, pr) => (wt, note, pr),
                        ReviewPrep::WaitForPr(until) => {
                            let reason = format!(
                                "waiting for PR or branch push for {} until {}",
                                detail.task.id,
                                until.to_rfc3339()
                            );
                            tracing::info!("{}: {reason}", detail.task.id);
                            let _ = self
                                .client()
                                .update_run(
                                    run_id,
                                    &UpdateRun {
                                        state: Some(RunState::Cancelled),
                                        reason: Some(reason),
                                        ..UpdateRun::default()
                                    },
                                )
                                .await;
                            let _ = self.db.update_local_run_state(
                                run_id.as_str(),
                                LocalRunState::Cancelled,
                                None,
                                now,
                            );
                            let _ = self.db.remove_resource(&format!("run:{run_id}"));
                            self.review_waits
                                .record(detail.task.id.clone(), until, observed_at);
                            return Ok((
                                ClaimedExecution {
                                    deferred_review: true,
                                    ..ClaimedExecution::default()
                                },
                                true,
                            ));
                        }
                    };
                    let brief = crate::brief::judge_brief_with_options(
                        crate::brief::JudgeKind::Review,
                        &board.slug,
                        &tasks,
                        worktree_note.as_deref(),
                        include_t3_settle,
                    );
                    let prepacked = prepack::build_review_context_with_files(
                        detail,
                        pr_view.as_deref(),
                        &project,
                        wt.as_ref().map(|w| w.base.as_str()),
                    )
                    .await;
                    let effort = classify_review_effort(
                        &detail.task,
                        false,
                        &prepacked.changed_files,
                        prepacked.files_truncated,
                    );
                    let model = self.ctx.config.model_for_review(effort, &runner_kind);
                    tracing::debug!(
                        "{}: pre-merge review effort={effort:?} size={:?} files={} truncated={} runner={runner_kind}",
                        detail.task.id,
                        detail.task.size,
                        prepacked.changed_files.len(),
                        prepacked.files_truncated
                    );
                    let brief = crate::brief::append_prepacked_context_with_limit(
                        brief,
                        &prepacked.text,
                        crate::brief::REVIEW_PREPACKED_CONTEXT_CHARS_MAX,
                    );
                    let title = claimed.run.title.clone().unwrap_or_else(|| {
                        format!("review {} {}", detail.task.id, detail.task.title)
                    });
                    (wt, brief, model, title)
                }
            }
            RunKind::Verdict | RunKind::Triage | RunKind::Goal => {
                let (judge_kind, brief_kind) = match claimed.run.kind {
                    RunKind::Verdict => (JudgeKind::Verdict, crate::brief::JudgeKind::Verdict),
                    RunKind::Triage => (JudgeKind::Triage, crate::brief::JudgeKind::Triage),
                    RunKind::Goal => (JudgeKind::Goal, crate::brief::JudgeKind::Goal),
                    _ => unreachable!(),
                };
                let target_tasks: Vec<TaskId> = if claimed.tasks.is_empty() {
                    claimed.task_id.iter().cloned().collect()
                } else {
                    claimed.tasks.clone()
                };
                let brief = crate::brief::judge_brief_with_options(
                    brief_kind,
                    &board.slug,
                    &target_tasks,
                    None,
                    include_t3_settle,
                );
                let prepacked = match claimed.run.kind {
                    RunKind::Triage => {
                        prepack::build_triage_context(
                            self.client(),
                            &board.slug,
                            &target_tasks,
                            &project,
                        )
                        .await
                    }
                    RunKind::Verdict => {
                        prepack::build_verdict_context(
                            self.client(),
                            &board.slug,
                            &board.goal,
                            &target_tasks,
                        )
                        .await
                    }
                    RunKind::Goal => {
                        prepack::build_goal_context(self.client(), board, target_tasks.first(), now)
                            .await
                    }
                    _ => String::new(),
                };
                let brief = crate::brief::append_prepacked_context(brief, &prepacked);
                let title = claimed
                    .run
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("{} {}", brief_kind.as_str(), board.slug));
                (
                    None,
                    brief,
                    self.ctx.config.model_for_judge(judge_kind).cloned(),
                    title,
                )
            }
            RunKind::Curate => {
                let brief = crate::brief::judge_brief_with_options(
                    crate::brief::JudgeKind::Curate,
                    &board.slug,
                    &[],
                    None,
                    include_t3_settle,
                );
                let prepacked =
                    prepack::build_curate_context(self.client(), board, &project, now).await;
                let brief = crate::brief::append_prepacked_context_with_limit(
                    brief,
                    &prepacked,
                    crate::brief::CURATE_PREPACKED_CONTEXT_CHARS_MAX,
                );
                let title = claimed
                    .run
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("curate {}", board.slug));
                (
                    None,
                    brief,
                    self.ctx.config.model_for_curate().cloned(),
                    title,
                )
            }
            RunKind::Command => {
                let title = claimed
                    .run
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("command {run_id}"));
                let prompt = title.clone();
                (None, prompt, self.ctx.config.thread_model.clone(), title)
            }
            _ => anyhow::bail!("unsupported LLM run kind {:?}", claimed.run.kind),
        };

        let use_daemon_worktree = runner_kind != "t3";
        let mut worktree_id: Option<String> = None;
        let mut worktree_path: Option<PathBuf> = None;
        let mut branch: Option<String> = worktree_spec.as_ref().map(|w| w.branch.clone());

        let git_verified = worktree_spec.is_some();
        let (workdir, spec_worktree) = if use_daemon_worktree && let Some(spec) = &worktree_spec {
            let managed = self
                .worktrees
                .create(&project, repo_key.as_deref(), run_id.as_str(), spec, now)
                .await
                .context("creating daemon-managed worktree")?;
            worktree_id = Some(managed.id);
            worktree_path = Some(managed.path.clone());
            branch = Some(managed.branch);
            (managed.path, None)
        } else {
            (project.clone(), worktree_spec)
        };

        // Transition local + server state to `starting` before spawning.
        local_run.runner = runner_kind.clone();
        local_run.worktree_path = worktree_path.clone().or_else(|| Some(workdir.clone()));
        local_run.branch = branch.clone();
        local_run.state = LocalRunState::Starting;
        local_run.updated_at = now;
        self.db.upsert_local_run(local_run)?;

        // Pre-assign a unique `antigravity:<run_id>` identity for ACP runs BEFORE spawning so
        // `AcpRunner` injects `BOARD_AGENT` into the ACP process environment (which `board mcp`
        // inherits at startup before `session/new`), and bind it on the server in `Starting` so
        // Call #1 already holds live managed run rights and never creates a duplicate interactive run.
        let preassigned_agent_id = claimed.run.agent_id.clone().or_else(|| {
            (runner_kind == "acp")
                .then(|| format!("antigravity:{run_id}").parse::<AgentId>().ok())
                .flatten()
        });

        let cwd_str = workdir.to_string_lossy().into_owned();
        if let Err(err) = self
            .client()
            .update_run(
                run_id,
                &UpdateRun {
                    state: Some(RunState::Starting),
                    agent_id: preassigned_agent_id.clone().map(Some),
                    worktree_branch: branch.clone().map(Some),
                    cwd: Some(Some(cwd_str.clone())),
                    ..UpdateRun::default()
                },
            )
            .await
        {
            if let Some(wt_id) = worktree_id.as_deref() {
                self.cleanup_worktree(wt_id, now).await;
            }
            return Err(err.context("marking claimed run starting"));
        }

        let mut metadata = BTreeMap::from([
            ("board".to_owned(), claimed.board.to_string()),
            ("kind".to_owned(), run_kind_str(claimed.run.kind).to_owned()),
            ("run_id".to_owned(), run_id.to_string()),
        ]);
        if let Some(tid) = &claimed.task_id {
            metadata.insert("task".to_owned(), tid.to_string());
        }
        if let Some(agent_id) = &preassigned_agent_id {
            metadata.insert("agent_id".to_owned(), agent_id.to_string());
        }
        let spec = RunSpec {
            run_id: run_id.to_string(),
            title: title.clone(),
            prompt,
            model,
            worktree: spec_worktree,
            metadata,
        };
        let handle = match runner.spawn(&spec, &workdir).await {
            Ok(handle) => handle,
            Err(err) => {
                if let Some(wt_id) = worktree_id.as_deref() {
                    self.cleanup_worktree(wt_id, now).await;
                }
                return Err(err.context(format!("spawning {runner_kind} runner for {run_id}")));
            }
        };
        let agent_id = match handle.resolve_agent_id() {
            Ok(id) => id,
            Err(err) => {
                self.cancel_and_maybe_cleanup_after_spawn(
                    &*runner,
                    &handle,
                    worktree_id.as_deref(),
                    claimed,
                    "invalid agent id",
                    now,
                )
                .await;
                return Err(err);
            }
        };

        let updated = match self
            .client()
            .update_run(
                run_id,
                &UpdateRun {
                    state: Some(RunState::Running),
                    agent_id: Some(Some(agent_id.clone())),
                    worktree_branch: branch.clone().map(Some),
                    cwd: Some(Some(cwd_str)),
                    lease_ttl_secs: Some(RUN_LEASE_TTL_SECS_DEFAULT),
                    ..UpdateRun::default()
                },
            )
            .await
            .context("marking claimed run running")
        {
            Ok(updated) => updated,
            Err(err) => {
                self.cancel_and_maybe_cleanup_after_spawn(
                    &*runner,
                    &handle,
                    worktree_id.as_deref(),
                    claimed,
                    "marking claimed run running failed",
                    now,
                )
                .await;
                return Err(err);
            }
        };

        let managed = ManagedRun {
            run_id: run_id.clone(),
            board: claimed.board.clone(),
            kind: claimed.run.kind,
            task_id: claimed.task_id.clone(),
            tasks: claimed.tasks.clone(),
            runner: handle.runner.clone(),
            native_id: Some(handle.native_id.clone()),
            agent_id: Some(agent_id.clone()),
            worktree_id: worktree_id.clone(),
            worktree_path: handle.workdir.clone().or(worktree_path).or(Some(workdir)),
            branch: branch.clone(),
            cost_units: claimed.run.cost_units,
            title,
            started_at: now,
            last_renewed_at: Some(now),
            lease_expires_at: updated.lease_expires_at,
            noted: false,
            waiting_external: false,
            last_event_seq: 0,
        };
        local_run.runner = handle.runner.clone();
        local_run.session_id = Some(handle.native_id.clone());
        local_run.state = LocalRunState::Running;
        local_run.lease_expires_at = updated.lease_expires_at;
        local_run.updated_at = now;
        local_run.meta_json = serde_json::to_string(&managed).ok();
        if let Err(err) = self.db.upsert_local_run(local_run).and_then(|()| {
            self.state.managed_runs.insert(run_id.clone(), managed);
            self.sync_resource_ledger(now);
            self.save_at(now)
        }) {
            self.state.managed_runs.remove(run_id);
            self.cancel_and_maybe_cleanup_after_spawn(
                &*runner,
                &handle,
                worktree_id.as_deref(),
                claimed,
                "persisting running state failed",
                now,
            )
            .await;
            return Err(err);
        }

        // Best-effort start note on every assigned task for human, cross-tool, and multi-task
        // review-train reviewer authorization (`is_registered_reviewer` / `standing_review`).
        let note_targets: Vec<TaskId> =
            if claimed.run.kind == RunKind::Review && !claimed.tasks.is_empty() {
                claimed.tasks.clone()
            } else {
                claimed.task_id.iter().cloned().collect()
            };
        if !note_targets.is_empty()
            && matches!(claimed.run.kind, RunKind::Worker | RunKind::Review)
            && let Ok(dispatch_agent) = self.ensure_dispatch_registered().await
        {
            let link = if handle.runner == "t3" {
                match self.t3.thread_link(&handle.native_id).await {
                    Ok(Some(link)) => link,
                    _ => format!("thread {}", handle.native_id),
                }
            } else {
                format!("{} run {}", handle.runner, handle.native_id)
            };
            let text = match claimed.run.kind {
                RunKind::Worker => {
                    let branch_part = branch
                        .as_ref()
                        .map(|b| format!(" on branch {b}"))
                        .unwrap_or_default();
                    format!("{WORKER_NOTE_PREFIX}{branch_part}: {link}")
                }
                RunKind::Review => {
                    if after_merge_review {
                        format!(
                            "{} {link} (after-merge) (agent {agent_id})",
                            judges::JUDGE_NOTE
                        )
                    } else {
                        format!("{} {link} (agent {agent_id})", judges::JUDGE_NOTE)
                    }
                }
                _ => String::new(),
            };
            if !text.is_empty() {
                let agent_client = self.client().with_agent(Some(dispatch_agent.clone()));
                let mut any_noted = false;
                for tid in &note_targets {
                    if add_note(
                        &agent_client,
                        tid,
                        &dispatch_agent,
                        NoteKind::Note,
                        text.clone(),
                    )
                    .await
                    .is_ok()
                    {
                        any_noted = true;
                    }
                }
                if any_noted && let Some(entry) = self.state.managed_runs.get_mut(run_id) {
                    entry.noted = true;
                }
            }
        }
        tracing::info!(
            "pull-started {} run {run_id} ({}:{}) on {}",
            run_kind_str(claimed.run.kind),
            handle.runner,
            handle.native_id,
            claimed.board
        );
        Ok((ClaimedExecution::default(), git_verified))
    }

    async fn cancel_and_maybe_cleanup_after_spawn(
        &mut self,
        runner: &dyn Runner,
        handle: &RunHandle,
        worktree_id: Option<&str>,
        claimed: &ClaimedRun,
        reason: &str,
        now: Timestamp,
    ) {
        match runner.cancel(handle, reason).await {
            Ok(()) => {
                if let Some(wt_id) = worktree_id {
                    self.cleanup_worktree(wt_id, now).await;
                }
            }
            Err(cancel_err) => {
                tracing::warn!(
                    "post-spawn cancel of {} ({}) failed ({cancel_err:#}); preserving worktree {:?} so the live agent is not disrupted",
                    claimed.run.id,
                    handle.native_id,
                    worktree_id
                );
                self.record_drop(
                    "pull_post_spawn_cancel_failed",
                    Some(&claimed.board),
                    claimed.task_id.as_ref(),
                    handle.agent_id.as_ref(),
                    &format!(
                        "could not cancel spawned {} session {} after {reason}: {cancel_err:#}; worktree preserved",
                        handle.runner, handle.native_id
                    ),
                    serde_json::json!({
                        "run_id": claimed.run.id,
                        "native_id": handle.native_id,
                        "worktree_id": worktree_id,
                        "reason": reason,
                    }),
                    now,
                );
            }
        }
    }

    async fn prepare_claimed_review_worktree(
        &self,
        board: &Board,
        detail: &board_proto::TaskDetail,
        repo_key: Option<&str>,
        project: &Path,
        now: Timestamp,
    ) -> Result<ReviewPrep> {
        use crate::worktree::{ReviewBase, ReviewTarget};
        let task = &detail.task.id;
        let no_code = judges::noted_pr_none(detail, &board.repos);
        let pr = if no_code {
            None
        } else {
            judges::noted_pr(detail, &board.repos)
        };
        let worker_branch = if pr.is_none() && !no_code {
            match judges::noted_worker_branch(detail) {
                Some(branch) => branch,
                None => Some(crate::worktree::worker_branch(task)),
            }
        } else {
            None
        };
        let (pr_ref, pr_view) = if let Some(pr_url) = &pr {
            match self.gh.view(project, pr_url).await {
                Ok(view) => {
                    let ref_name = if view.state == PrState::Merged && !view.base_ref.is_empty() {
                        Some(view.base_ref.clone())
                    } else if !view.cross_repository && !view.head_ref.is_empty() {
                        Some(view.head_ref.clone())
                    } else {
                        None
                    };
                    (ref_name, Some(view))
                }
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        };
        let target = if let Some(branch) = pr_ref.as_deref() {
            ReviewTarget::PullRequest(branch)
        } else if pr.is_none() && !no_code {
            match worker_branch.as_deref() {
                Some(branch) => ReviewTarget::TaskBranch(branch),
                None => ReviewTarget::None,
            }
        } else {
            ReviewTarget::None
        };
        let base_branch = self.ctx.config.base_branch(&board.slug, repo_key);
        let prepared =
            crate::worktree::review_worktree_with_base(project, task, target, base_branch, now)
                .await
                .context("preparing review judge worktree")?;
        let pr_view = pr_view.map(Box::new);
        Ok(match prepared {
            None => ReviewPrep::Ready(None, None, pr_view),
            Some(prepared) => match prepared.source {
                ReviewBase::PullRequest => {
                    ReviewPrep::Ready(Some(prepared.worktree), None, pr_view)
                }
                ReviewBase::TaskBranch => {
                    let note = crate::brief::review_task_branch_note(&prepared.worktree.base);
                    ReviewPrep::Ready(Some(prepared.worktree), Some(note), pr_view)
                }
                ReviewBase::DefaultBase => {
                    let parked_at = judges::parked_in_review_at(detail);
                    if !no_code && pr.is_none() && judges::within_pr_wait(parked_at, now) {
                        return Ok(ReviewPrep::WaitForPr(parked_at + judges::REVIEW_PR_WAIT));
                    }
                    let note =
                        crate::brief::review_default_base_note(&prepared.worktree.base, no_code);
                    ReviewPrep::Ready(Some(prepared.worktree), Some(note), pr_view)
                }
            },
        })
    }

    async fn transition_run_starting_to_running(
        &self,
        run_id: &RunId,
        agent: Option<&AgentId>,
        branch: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<board_proto::Run> {
        self.client()
            .update_run(
                run_id,
                &UpdateRun {
                    state: Some(RunState::Starting),
                    agent_id: agent.cloned().map(Some),
                    worktree_branch: branch.map(|b| Some(b.to_owned())),
                    cwd: cwd.map(|c| Some(c.to_owned())),
                    ..UpdateRun::default()
                },
            )
            .await?;
        self.client()
            .update_run(
                run_id,
                &UpdateRun {
                    state: Some(RunState::Running),
                    agent_id: agent.cloned().map(Some),
                    lease_ttl_secs: Some(RUN_LEASE_TTL_SECS_DEFAULT),
                    ..UpdateRun::default()
                },
            )
            .await
    }

    async fn cancel_stale_claimed_worker(
        &mut self,
        claimed: &ClaimedRun,
        cause: &str,
        subject: serde_json::Value,
        now: Timestamp,
    ) {
        let run_id = &claimed.run.id;
        let clipped = clip_run_reason(cause);
        self.record_drop(
            "worker_skipped_not_ready",
            Some(&claimed.board),
            claimed.task_id.as_ref(),
            None,
            &clipped,
            subject,
            now,
        );
        tracing::info!(
            "{}: {clipped}; cancelling claimed worker run {run_id}",
            claimed.board
        );
        if let Err(err) = self
            .client()
            .update_run(
                run_id,
                &UpdateRun {
                    state: Some(RunState::Cancelled),
                    reason: Some(clipped),
                    ..UpdateRun::default()
                },
            )
            .await
        {
            tracing::warn!("cancelling stale worker run {run_id} on server: {err:#}");
        }
        let _ =
            self.db
                .update_local_run_state(run_id.as_str(), LocalRunState::Cancelled, None, now);
        let _ = self.db.remove_resource(&format!("run:{run_id}"));
    }

    async fn fail_claimed_run(&mut self, claimed: &ClaimedRun, reason: &str, now: Timestamp) {
        let run_id = &claimed.run.id;
        let clipped = clip_run_reason(reason);
        self.record_drop(
            "pull_run_failed",
            Some(&claimed.board),
            claimed.task_id.as_ref(),
            None,
            &clipped,
            serde_json::json!({
                "run_id": run_id,
                "kind": claimed.run.kind,
                "runner": claimed.run.runner,
            }),
            now,
        );
        if let Err(err) = self
            .client()
            .update_run(
                run_id,
                &UpdateRun {
                    state: Some(RunState::Failed),
                    reason: Some(clipped),
                    ..UpdateRun::default()
                },
            )
            .await
        {
            tracing::warn!("marking failed run {run_id} on server: {err:#}");
        }
        let _ = self
            .db
            .update_local_run_state(run_id.as_str(), LocalRunState::Failed, None, now);
        let _ = self.db.remove_resource(&format!("run:{run_id}"));
        self.state.managed_runs.remove(run_id);
        if self.enqueue_transcript_archive(run_id, Some(claimed.board.clone()), now) {
            let _ = self.flush_telemetry_outbox(now).await;
        }
    }

    /// Returns `true` while the daemon is inside the brief post-restart drain window waiting
    /// for launchd/systemd to send `SIGTERM`. If no signal arrives within
    /// [`SELF_RESTART_DRAIN_SECS`] (e.g., when run manually outside a service manager), clears
    /// the drain window and resumes normal dispatch.
    fn self_restart_draining(&mut self, now: Timestamp) -> bool {
        let Some(triggered_at) = self.self_restart_triggered_at else {
            return false;
        };
        if now - triggered_at < TimeDelta::seconds(SELF_RESTART_DRAIN_SECS) {
            return true;
        }
        tracing::warn!(
            "detached board watch self-restart did not terminate this process within {SELF_RESTART_DRAIN_SECS}s; resuming dispatch (restart board watch manually to run the upgraded binary)"
        );
        self.self_restart_triggered_at = None;
        false
    }

    /// When a deterministic after-merge deploy has finished closing tasks and updating its
    /// server run, probes the installed `board` binary on disk and triggers a detached
    /// graceful self-restart ONLY if the installed binary commit changed from this running
    /// process's build commit and no other background deploys remain in flight.
    async fn maybe_trigger_self_restart(&mut self, now: Timestamp) {
        if !self.self_restart_needed || !self.bg_merges.is_empty() {
            return;
        }
        if self.self_restart_draining(now) {
            return;
        }
        self.self_restart_needed = false;
        let running = self.self_restarter.running_commit().to_owned();
        let installed = self.self_restarter.installed_commit().await;
        if !should_self_restart(&running, installed.as_deref()) {
            tracing::debug!(
                "after-merge-deploy finished without binary change (running={running}, installed={}); skipping self-restart",
                installed.as_deref().unwrap_or("unknown")
            );
            return;
        }
        let installed = installed.unwrap_or_default();
        if let Err(err) = self.save() {
            tracing::warn!("saving daemon state before self-restart: {err:#}");
        }
        tracing::info!(
            "installed board commit {installed} differs from running {running} after deterministic deploy; triggering detached graceful self-restart"
        );
        match self.self_restarter.trigger_restart(&running, &installed) {
            Ok(()) => {
                self.self_restart_triggered_at = Some(now);
            }
            Err(err) => {
                tracing::warn!(
                    "triggering detached graceful self-restart for commit {installed} failed: {err:#}; restart board watch manually"
                );
            }
        }
    }

    /// Checks every active pull-claimed [`ManagedRun`] via [`Runner::status`]:
    /// - still live (`starting | running | waiting_external | idle` and `runnable`): renews
    ///   its server lease (`lease_ttl_secs = 180`) via `PATCH /v1/runs/{id}`;
    /// - ended/failed/cancelled/gone: transitions the server `Run` to `succeeded`/`failed`/
    ///   `cancelled`, updates `daemon.db`, frees `resource_ledger`, and cleans up worktrees.
    pub async fn renew_runs(&mut self, now: Timestamp) -> Result<()> {
        // Reap any finished background deterministic after-merge deploy tasks first.
        let finished_bg: Vec<RunId> = self
            .bg_merges
            .iter()
            .filter(|(_, (_, handle))| handle.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for run_id in finished_bg {
            if let Some((task_id, handle)) = self.bg_merges.remove(&run_id) {
                let bg = handle.await.unwrap_or(BgMergeResult {
                    local_state: LocalRunState::Failed,
                    check_self_restart: false,
                });
                let _ = self
                    .db
                    .update_local_run_state(run_id.as_str(), bg.local_state, None, now);
                let _ = self.db.remove_resource(&format!("run:{run_id}"));
                self.state.merges.remove(&task_id);
                if bg.check_self_restart {
                    self.self_restart_needed = true;
                }
            }
        }
        self.maybe_trigger_self_restart(now).await;

        let run_ids: Vec<RunId> = self.state.managed_runs.keys().cloned().collect();
        for run_id in run_ids {
            let Some(mut run) = self.state.managed_runs.get(&run_id).cloned() else {
                continue;
            };
            let Some(native_id) = run.native_id.clone() else {
                continue;
            };
            let runner = match self.runners.resolve(Some(&run.runner)) {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!("monitoring run {run_id}: {err:#}");
                    continue;
                }
            };
            let mut handle = RunHandle::new(&run.runner, &native_id);
            if let Some(wd) = &run.worktree_path {
                handle = handle.with_workdir(wd);
            }
            if let Some(aid) = &run.agent_id {
                handle = handle.with_agent_id(aid.clone());
            }
            let status = match tokio::time::timeout(
                self.timing.thread_check,
                runner.status(&handle),
            )
            .await
            {
                Ok(Ok(status)) => status,
                Ok(Err(err)) => {
                    tracing::warn!("checking status of managed run {run_id}: {err:#}");
                    continue;
                }
                Err(_) => {
                    tracing::warn!(
                        "checking status of managed run {run_id} timed out after {}s",
                        self.timing.thread_check.as_secs()
                    );
                    continue;
                }
            };

            let (keep_live, next_server_state) = match status.state {
                RunnerState::Starting | RunnerState::Running if status.runnable => {
                    let state_update = if run.waiting_external {
                        Some(RunState::Running)
                    } else {
                        None
                    };
                    (true, state_update)
                }
                RunnerState::WaitingExternal if status.runnable => {
                    let state_update = if !run.waiting_external {
                        Some(RunState::WaitingExternal)
                    } else {
                        None
                    };
                    (true, state_update)
                }
                RunnerState::Idle if status.runnable => {
                    if status.busy {
                        let state_update = if run.waiting_external {
                            Some(RunState::Running)
                        } else {
                            None
                        };
                        (true, state_update)
                    } else if run.kind == RunKind::Worker
                        && let Some(tid) = run.task_id.as_ref()
                        && let Some(aid) = run.agent_id.as_ref()
                        && self.is_task_blocked_for_agent(tid, aid).await
                    {
                        let state_update = if !run.waiting_external {
                            Some(RunState::WaitingExternal)
                        } else {
                            None
                        };
                        (true, state_update)
                    } else if status.runs.unwrap_or(0) == 0
                        && now - run.started_at < RUN_START_GRACE
                    {
                        (true, None)
                    } else {
                        (false, None)
                    }
                }
                _ => (false, None),
            };
            if keep_live {
                match self
                    .client()
                    .update_run(
                        &run_id,
                        &UpdateRun {
                            state: next_server_state,
                            lease_ttl_secs: Some(RUN_LEASE_TTL_SECS_DEFAULT),
                            ..UpdateRun::default()
                        },
                    )
                    .await
                {
                    Ok(updated) => {
                        run.waiting_external = updated.state == RunState::WaitingExternal;
                        run.last_renewed_at = Some(now);
                        run.lease_expires_at = updated.lease_expires_at;
                        let _ = self.db.update_local_run_lease(
                            run_id.as_str(),
                            updated.lease_expires_at,
                            now,
                        );
                        self.state.managed_runs.insert(run_id, run);
                    }
                    Err(err)
                        if matches!(
                            error_code(&err),
                            Some(ErrorCode::InvalidTransition | ErrorCode::NotFound)
                        ) =>
                    {
                        tracing::info!(
                            "managed run {run_id} is already terminal on server ({err:#}); cleaning up locally"
                        );
                        self.record_drop(
                            "managed_run_server_terminal",
                            Some(&run.board),
                            run.task_id.as_ref(),
                            run.agent_id.as_ref(),
                            &format!("server already closed run {run_id}: {err:#}"),
                            serde_json::to_value(&run).unwrap_or_default(),
                            now,
                        );
                        if error_code(&err) == Some(ErrorCode::InvalidTransition) {
                            let collected = match runner
                                .collect_token_usage(&handle)
                                .await
                                .filter(RunTokenUsage::is_non_zero)
                            {
                                Some(u) => Some(u),
                                None => {
                                    self.collect_local_run_tokens(
                                        &run_id,
                                        &run.runner,
                                        run.native_id.as_deref(),
                                        run.agent_id.as_ref(),
                                        Some(run.started_at),
                                        Some(now),
                                    )
                                    .await
                                }
                            };
                            if let Some(usage) = collected {
                                self.patch_missing_run_tokens(&run_id, None, usage).await;
                            }
                        }
                        runner.close(&handle).await;
                        let _ = self.db.update_local_run_runtime(
                            run_id.as_str(),
                            LocalRunState::Cancelled,
                            None,
                            None,
                            true,
                            now,
                        );
                        let _ = self.db.remove_resource(&format!("run:{run_id}"));
                        self.state.managed_runs.remove(&run_id);
                        let _ =
                            self.enqueue_transcript_archive(&run_id, Some(run.board.clone()), now);
                        if let Some(wt_id) = run.worktree_id.as_deref() {
                            self.cleanup_worktree(wt_id, now).await;
                        }
                    }
                    Err(err) => {
                        tracing::warn!("renewing lease for managed run {run_id}: {err:#}");
                    }
                }
            } else {
                // Flush any trailing live events before closing the run.
                let _ = self.flush_run_events_for(&run_id, now).await;
                let (server_state, local_state, reason) = match status.state {
                    RunnerState::Succeeded => (RunState::Succeeded, LocalRunState::Finished, None),
                    RunnerState::Failed => (
                        RunState::Failed,
                        LocalRunState::Failed,
                        Some("runner reported failed state".to_owned()),
                    ),
                    RunnerState::Cancelled => (
                        RunState::Cancelled,
                        LocalRunState::Cancelled,
                        Some("runner reported cancelled state".to_owned()),
                    ),
                    RunnerState::Gone
                    | RunnerState::Starting
                    | RunnerState::Running
                    | RunnerState::WaitingExternal
                    | RunnerState::Idle
                    | RunnerState::Unknown => {
                        if status.runs.unwrap_or(0) > 0 {
                            (RunState::Succeeded, LocalRunState::Finished, None)
                        } else {
                            (
                                RunState::Failed,
                                LocalRunState::Failed,
                                Some("runner session ended before running".to_owned()),
                            )
                        }
                    }
                };
                let outcome = runner
                    .collect_token_usage(&handle)
                    .await
                    .filter(RunTokenUsage::is_non_zero)
                    .map(|usage| Some(usage.to_outcome_json()));
                runner.close(&handle).await;
                if let Err(err) = self
                    .client()
                    .update_run(
                        &run_id,
                        &UpdateRun {
                            state: Some(server_state),
                            reason,
                            outcome,
                            ..UpdateRun::default()
                        },
                    )
                    .await
                    && !matches!(
                        error_code(&err),
                        Some(ErrorCode::InvalidTransition | ErrorCode::NotFound)
                    )
                {
                    tracing::warn!(
                        "updating ended managed run {run_id} to {server_state:?}: {err:#}"
                    );
                }
                if server_state == RunState::Succeeded {
                    self.llm_spawn_backoffs.clear_runner(&run.runner);
                }
                let _ = self.db.update_local_run_runtime(
                    run_id.as_str(),
                    local_state,
                    None,
                    None,
                    true,
                    now,
                );
                let _ = self.db.remove_resource(&format!("run:{run_id}"));
                self.state.managed_runs.remove(&run_id);
                let _ = self.enqueue_transcript_archive(&run_id, Some(run.board.clone()), now);
                if let Some(wt_id) = run.worktree_id.as_deref() {
                    self.cleanup_worktree(wt_id, now).await;
                }
            }
        }
        self.sync_resource_ledger(now);
        self.cleanup_ended_worktrees(now).await;
        let _ = self.flush_telemetry_outbox(now).await;
        let _ = self.save_at(now);
        Ok(())
    }

    /// Reconciles non-terminal rows in `daemon.db`'s `local_runs` table (`intent`, `starting`,
    /// `running`) that are not actively tracked in memory (`managed_runs`, `bg_merges`,
    /// `workers`, `judges`) against the server's run state and local handles.
    ///
    /// Runs on startup and each reconcile pass so rows left behind by daemon restarts, crash
    /// windows between write-before-act intent persistence and spawn, or server-side orphaning
    /// are cleanly marked terminal (`finished` / `failed` / `cancelled` / `orphaned`) with a
    /// Trail Rule drop record and never linger as ghost rows in `daemon.db`.
    pub async fn reconcile_stale_local_runs(&mut self, now: Timestamp) -> usize {
        let active_rows = match self.db.list_active_local_runs() {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!("listing active local_runs for reconciliation: {err:#}");
                return 0;
            }
        };
        if active_rows.is_empty() {
            return 0;
        }
        let mut reconciled = 0usize;
        for rec in active_rows {
            let parsed_run_id = rec.run_id.parse::<RunId>().ok();
            let tracked_in_memory = parsed_run_id.as_ref().is_some_and(|rid| {
                self.state.managed_runs.contains_key(rid) || self.bg_merges.contains_key(rid)
            }) || self.state.workers.values().any(|w| {
                w.run_id.as_deref() == Some(rec.run_id.as_str())
                    || (rec.session_id.is_some() && w.thread == rec.session_id)
            }) || self.state.judges.iter().any(|j| {
                j.run_id.as_deref() == Some(rec.run_id.as_str())
                    || (rec.session_id.is_some() && j.thread == rec.session_id)
            });
            if tracked_in_memory {
                continue;
            }

            let choose_valid_terminal = |from: RunState, desired: RunState| -> Option<RunState> {
                [desired, RunState::Failed, RunState::Cancelled]
                    .into_iter()
                    .find(|&target| from.can_transition_to(target))
            };

            let mut server_run_for_tokens: Option<board_proto::Run> = None;
            let decision: Option<(Option<RunState>, LocalRunState, String, bool)> = if let Some(
                ref run_id,
            ) =
                parsed_run_id
            {
                match self.client().get_run(run_id).await {
                    Ok(server_run) if server_run.state.is_terminal() => {
                        let local_state = match server_run.state {
                            RunState::Succeeded => LocalRunState::Finished,
                            RunState::Cancelled | RunState::Preempted => LocalRunState::Cancelled,
                            RunState::Orphaned => LocalRunState::Orphaned,
                            _ => LocalRunState::Failed,
                        };
                        let teardown_acp =
                            rec.runner == "acp" && crate::acp_runner::process_group_alive(rec.pid);
                        let srv_state = server_run.state;
                        server_run_for_tokens = Some(server_run);
                        Some((
                            None,
                            local_state,
                            format!(
                                "server run {run_id} is already terminal ({srv_state:?}); reconciling stale local_runs row (was {:?})",
                                rec.state
                            ),
                            teardown_acp,
                        ))
                    }
                    Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                        if rec.runner == "acp" && crate::acp_runner::process_group_alive(rec.pid) {
                            None
                        } else {
                            Some((
                                None,
                                LocalRunState::Orphaned,
                                format!(
                                    "server run {run_id} not found (404); reconciling stale local_runs row (was {:?})",
                                    rec.state
                                ),
                                false,
                            ))
                        }
                    }
                    Ok(server_run) => {
                        let srv_state = server_run.state;
                        let srv_agent_id = server_run.agent_id.clone();
                        server_run_for_tokens = Some(server_run);
                        // Server still considers the run live; check if its local handle is
                        // definitively gone (deterministic run not in bg_merges, pre-spawn
                        // intent without session_id, dead ACP pid, or ended T3 thread).
                        if rec.runner == DETERMINISTIC_RUNNER_KIND {
                            if srv_state == RunState::WaitingExternal {
                                // A parked `waiting_external` deterministic merge run has no
                                // local in-process handle by design; reconcile only the stale
                                // `local_runs` row without failing the server run.
                                Some((
                                    None,
                                    LocalRunState::Finished,
                                    format!(
                                        "deterministic run {run_id} is parked in waiting_external on server; clearing stale local_runs row (was {:?})",
                                        rec.state
                                    ),
                                    false,
                                ))
                            } else {
                                Some((
                                    choose_valid_terminal(srv_state, RunState::Failed),
                                    LocalRunState::Orphaned,
                                    format!(
                                        "deterministic run {run_id} has no live in-process handle across restart (server {srv_state:?}, local {:?})",
                                        rec.state
                                    ),
                                    false,
                                ))
                            }
                        } else if rec.state == LocalRunState::Intent || rec.session_id.is_none() {
                            Some((
                                choose_valid_terminal(srv_state, RunState::Failed),
                                LocalRunState::Orphaned,
                                format!(
                                    "untracked {} run {run_id} stuck in {:?} without session handle across restart (server {srv_state:?})",
                                    rec.runner, rec.state
                                ),
                                false,
                            ))
                        } else if rec.runner == "acp" {
                            if crate::acp_runner::process_group_alive(rec.pid) {
                                None
                            } else {
                                Some((
                                    choose_valid_terminal(srv_state, RunState::Failed),
                                    LocalRunState::Orphaned,
                                    format!(
                                        "untracked ACP run {run_id} process {:?} is no longer alive (server {srv_state:?})",
                                        rec.pid
                                    ),
                                    false,
                                ))
                            }
                        } else if let Some(session_id) = rec.session_id.as_deref()
                            && let Ok(runner) = self.runners.resolve(Some(&rec.runner))
                        {
                            let mut handle = RunHandle::new(&rec.runner, session_id);
                            if let Some(wd) = &rec.worktree_path {
                                handle = handle.with_workdir(wd);
                            }
                            match tokio::time::timeout(
                                self.timing.thread_check,
                                runner.status(&handle),
                            )
                            .await
                            {
                                Ok(Ok(status)) => {
                                    let idle_waiting_external = srv_state
                                        == RunState::WaitingExternal
                                        || (rec.kind == "worker"
                                            && status.runnable
                                            && !status.busy
                                            && rec.task_id.as_ref().is_some()
                                            && srv_agent_id.is_some());
                                    let blocked_worker = if idle_waiting_external
                                        && srv_state != RunState::WaitingExternal
                                        && let Some(tid) = rec.task_id.as_ref()
                                        && let Some(aid) = srv_agent_id.as_ref()
                                    {
                                        self.is_task_blocked_for_agent(tid, aid).await
                                    } else {
                                        srv_state == RunState::WaitingExternal
                                    };
                                    let ended = !status.runnable
                                        || (!status.busy
                                            && status.runs.unwrap_or(0) > 0
                                            && !blocked_worker);
                                    if ended {
                                        let (desired_srv, loc) = match status.state {
                                            RunnerState::Cancelled => {
                                                (RunState::Cancelled, LocalRunState::Cancelled)
                                            }
                                            RunnerState::Failed => {
                                                (RunState::Failed, LocalRunState::Failed)
                                            }
                                            _ if status.runs.unwrap_or(0) > 0 => {
                                                (RunState::Succeeded, LocalRunState::Finished)
                                            }
                                            _ => (RunState::Failed, LocalRunState::Orphaned),
                                        };
                                        Some((
                                            choose_valid_terminal(srv_state, desired_srv),
                                            loc,
                                            format!(
                                                "untracked {} session {session_id} for {run_id} ended (runner state={:?}, runs={:?})",
                                                rec.runner, status.state, status.runs
                                            ),
                                            false,
                                        ))
                                    } else {
                                        None
                                    }
                                }
                                _ => None,
                            }
                        } else {
                            None
                        }
                    }
                    Err(err) => {
                        tracing::debug!(
                            "checking stale local_runs row {} against server: {err:#}",
                            rec.run_id
                        );
                        None
                    }
                }
            } else if rec.runner == DETERMINISTIC_RUNNER_KIND
                || rec.state == LocalRunState::Intent
                || rec.session_id.is_none()
                || (rec.runner == "acp" && !crate::acp_runner::process_group_alive(rec.pid))
            {
                Some((
                    None,
                    LocalRunState::Orphaned,
                    format!(
                        "untracked local-only {} run {} has no active handle (was {:?})",
                        rec.runner, rec.run_id, rec.state
                    ),
                    false,
                ))
            } else if let Some(session_id) = rec.session_id.as_deref()
                && let Ok(runner) = self.runners.resolve(Some(&rec.runner))
            {
                let mut handle = RunHandle::new(&rec.runner, session_id);
                if let Some(wd) = &rec.worktree_path {
                    handle = handle.with_workdir(wd);
                }
                match tokio::time::timeout(self.timing.thread_check, runner.status(&handle)).await {
                    Ok(Ok(status))
                        if !status.runnable || (!status.busy && status.runs.unwrap_or(0) > 0) =>
                    {
                        let loc = if status.runs.unwrap_or(0) > 0
                            && !matches!(status.state, RunnerState::Failed | RunnerState::Cancelled)
                        {
                            LocalRunState::Finished
                        } else if status.state == RunnerState::Cancelled {
                            LocalRunState::Cancelled
                        } else {
                            LocalRunState::Orphaned
                        };
                        Some((
                            None,
                            loc,
                            format!(
                                "untracked local {} session {session_id} ({}) ended (runner state={:?})",
                                rec.runner, rec.run_id, status.state
                            ),
                            false,
                        ))
                    }
                    _ => None,
                }
            } else {
                None
            };

            let Some((server_target, local_state, cause, teardown_acp)) = decision else {
                continue;
            };
            // Trail Rule: record reconciliation before mutating local_runs or server state.
            self.record_drop(
                "reconcile_stale_local_run",
                Some(&rec.board),
                rec.task_id.as_ref(),
                None,
                &cause,
                serde_json::to_value(&rec).unwrap_or_default(),
                now,
            );
            if teardown_acp {
                crate::acp_runner::terminate_detached_process_group(
                    rec.pid,
                    Duration::from_secs(2),
                )
                .await;
            }
            let backfill_tokens = if let Some(ref run_id) = parsed_run_id
                && rec.runner != DETERMINISTIC_RUNNER_KIND
                && !outcome_has_tokens(
                    server_run_for_tokens
                        .as_ref()
                        .and_then(|r| r.outcome.as_ref()),
                ) {
                let runner_tokens = if let Some(session_id) = rec.session_id.as_deref()
                    && let Ok(runner) = self.runners.resolve(Some(&rec.runner))
                {
                    let mut handle = RunHandle::new(&rec.runner, session_id);
                    if let Some(wd) = &rec.worktree_path {
                        handle = handle.with_workdir(wd);
                    }
                    runner
                        .collect_token_usage(&handle)
                        .await
                        .filter(RunTokenUsage::is_non_zero)
                } else {
                    None
                };
                match runner_tokens {
                    Some(u) => Some(u),
                    None => {
                        self.collect_local_run_tokens(
                            run_id,
                            &rec.runner,
                            rec.session_id.as_deref(),
                            server_run_for_tokens
                                .as_ref()
                                .and_then(|r| r.agent_id.as_ref()),
                            server_run_for_tokens
                                .as_ref()
                                .and_then(|r| r.started_at.or(Some(r.created_at))),
                            server_run_for_tokens.as_ref().and_then(|r| r.ended_at),
                        )
                        .await
                    }
                }
            } else {
                None
            };
            if let (Some(run_id), Some(srv_state)) = (parsed_run_id.as_ref(), server_target) {
                let outcome = backfill_tokens.map(|usage| {
                    Some(
                        usage.merge_into_outcome_json(
                            server_run_for_tokens
                                .as_ref()
                                .and_then(|r| r.outcome.as_ref()),
                        ),
                    )
                });
                if let Err(err) = self
                    .client()
                    .update_run(
                        run_id,
                        &UpdateRun {
                            state: Some(srv_state),
                            reason: Some(clip_run_reason(&cause)),
                            outcome,
                            ..UpdateRun::default()
                        },
                    )
                    .await
                    && !matches!(
                        error_code(&err),
                        Some(ErrorCode::InvalidTransition | ErrorCode::NotFound)
                    )
                {
                    tracing::warn!(
                        "updating stale server run {run_id} to {srv_state:?} during local_runs reconcile: {err:#}; will retry on next reconcile"
                    );
                    continue;
                }
            } else if let (Some(run_id), Some(usage)) = (parsed_run_id.as_ref(), backfill_tokens) {
                self.patch_missing_run_tokens(
                    run_id,
                    server_run_for_tokens
                        .as_ref()
                        .and_then(|r| r.outcome.as_ref()),
                    usage,
                )
                .await;
            }
            if let Err(err) =
                self.db
                    .update_local_run_runtime(&rec.run_id, local_state, None, None, true, now)
            {
                tracing::warn!(
                    "marking stale local_runs row {} as {:?}: {err:#}",
                    rec.run_id,
                    local_state
                );
                continue;
            }
            let _ = self.db.remove_resource(&format!("run:{}", rec.run_id));
            tracing::info!(
                "reconciled stale local_runs row {} ({} / {}) -> {:?}: {cause}",
                rec.run_id,
                rec.kind,
                rec.runner,
                local_state
            );
            reconciled += 1;
        }
        reconciled
    }

    /// Polls `GET /v1/devices/{name}/wakes`, verifies any previously timed-out sticky wakes
    /// in `daemon.db`'s `unsure_sends` (acknowledging ones confirmed delivered, allowing at
    /// most one resend for ones confirmed missing), delivers new pending sticky wakes via
    /// [`Runner::send`] (persisting `unsure_sends` BEFORE sending), and acknowledges delivered
    /// wakes via `POST /v1/wakes/{id}/ack`.
    pub async fn poll_sticky_wakes(&mut self, now: Timestamp) -> Result<usize> {
        if self.wakes_unsupported {
            return Ok(0);
        }
        let mut delivered = self.verify_unsure_sticky_wakes(now).await;
        let wakes = match self.client().list_device_wakes(&self.ctx.device).await {
            Ok(wakes) => wakes,
            Err(err) if is_heartbeat_unsupported(&err) => {
                self.wakes_unsupported = true;
                return Ok(delivered);
            }
            Err(err) => return Err(err.context("listing device sticky wakes")),
        };
        for wake in wakes {
            if wake.kind == "cancel" {
                if self.execute_cancel_wake(&wake, now).await {
                    delivered += 1;
                }
                continue;
            }
            if self.is_sticky_wake_unsure(wake.id) || self.sticky_exhausted.contains(&wake.id) {
                continue;
            }
            let Some((runner, handle, agent_id)) = self.resolve_sticky_wake_target(&wake) else {
                tracing::debug!(
                    "sticky wake #{} ({}) has no live local runner target; leaving pending",
                    wake.id,
                    wake.kind
                );
                continue;
            };
            let base_msg = format_sticky_wake_message(&wake);
            let digest = wake_digest(&base_msg, now);
            let marked_msg = format!("{base_msg}\n{}", wake_marker(&digest));
            let board_slug: BoardSlug = wake
                .payload
                .get("board")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .or_else(|| {
                    wake.run_id
                        .as_ref()
                        .and_then(|id| self.state.managed_runs.get(id))
                        .map(|r| r.board.clone())
                })
                .unwrap_or_else(|| self.ctx.config.feedback_board());

            let is_resend = self.sticky_resent.contains(&wake.id);
            let wake_record = Wake {
                board: board_slug,
                about: WakeAbout::Sticky {
                    wake_id: wake.id,
                    wake_kind: wake.kind.clone(),
                },
                queued_at: now,
                forwarded: false,
                resent: is_resend,
            };
            // Write-before-send: durably record digest in daemon.db's `unsure_sends` BEFORE
            // calling `Runner::send` so a crash mid-send leaves a durable trail.
            self.state
                .unsure_wakes
                .entry(agent_id.clone())
                .or_default()
                .messages
                .push(UnsureMessage {
                    digest: digest.clone(),
                    wakes: vec![wake_record],
                    since: now,
                });
            if let Err(err) = self.save_at(now) {
                self.remove_unsure_digest(&agent_id, &digest);
                tracing::warn!(
                    "sticky wake #{}: persisting unsure_sends before send failed: {err:#}",
                    wake.id
                );
                continue;
            }

            let steer = SteerMessage::new(marked_msg)
                .with_digest(digest.clone())
                .with_mode(sticky_wake_send_mode(&wake.kind));
            match tokio::time::timeout(WAKE_SEND_TIMEOUT, runner.send(&handle, &steer)).await {
                Ok(Ok(())) => {
                    self.remove_unsure_digest(&agent_id, &digest);
                    self.sticky_resent.remove(&wake.id);
                    let _ = self.save_at(now);
                    if let Err(err) = self.client().ack_wake(wake.id).await {
                        tracing::warn!("acking delivered sticky wake #{}: {err:#}", wake.id);
                    } else {
                        delivered += 1;
                    }
                }
                Ok(Err(err)) if err.downcast_ref::<TimedOut>().is_some() => {
                    if is_resend {
                        self.sticky_exhausted.insert(wake.id);
                    }
                    tracing::warn!(
                        "sticky wake #{} to {agent_id} timed out ({err:#}); kept in unsure_sends without ack",
                        wake.id
                    );
                }
                Err(_) => {
                    if is_resend {
                        self.sticky_exhausted.insert(wake.id);
                    }
                    tracing::warn!(
                        "sticky wake #{} to {agent_id} timed out after {}s; kept in unsure_sends without ack",
                        wake.id,
                        WAKE_SEND_TIMEOUT.as_secs()
                    );
                }
                Ok(Err(err)) => {
                    self.remove_unsure_digest(&agent_id, &digest);
                    let _ = self.save_at(now);
                    self.record_drop(
                        "sticky_wake_send_failed",
                        None,
                        None,
                        Some(&agent_id),
                        &format!("delivering sticky wake #{} failed: {err:#}", wake.id),
                        serde_json::to_value(&wake).unwrap_or_default(),
                        now,
                    );
                    tracing::warn!("delivering sticky wake #{} to {agent_id}: {err:#}", wake.id);
                }
            }
        }
        Ok(delivered)
    }

    async fn is_task_blocked_for_agent(&self, task_id: &TaskId, agent_id: &AgentId) -> bool {
        match self.client().task(task_id).await {
            Ok(detail) => {
                detail.task.status == TaskStatus::Blocked
                    && detail.task.owner.as_ref() == Some(agent_id)
            }
            Err(_) => false,
        }
    }

    fn is_sticky_wake_unsure(&self, wake_id: i64) -> bool {
        self.state.unsure_wakes.values().any(|u| {
            u.messages.iter().any(|m| {
                m.wakes.iter().any(
                    |w| matches!(w.about, WakeAbout::Sticky { wake_id: id, .. } if id == wake_id),
                )
            })
        })
    }

    /// Checks previously timed-out `WakeAbout::Sticky` entries in `state.unsure_wakes` once
    /// `SEND_CHECK_AFTER` has elapsed, using each agent's actual runner:
    /// - confirmed delivered -> calls `POST /v1/wakes/{id}/ack` and removes from `unsure_wakes`;
    /// - confirmed missing -> removes from `unsure_wakes` and marks for at most one resend;
    ///   if already resent once, records a Trail Rule drop and adds to `sticky_exhausted`.
    async fn verify_unsure_sticky_wakes(&mut self, now: Timestamp) -> usize {
        let sticky_agents: Vec<AgentId> = self
            .state
            .unsure_wakes
            .iter()
            .filter(|(_, u)| {
                u.read_due(now)
                    && u.messages.iter().any(|m| {
                        m.wakes
                            .iter()
                            .any(|w| matches!(w.about, WakeAbout::Sticky { .. }))
                    })
            })
            .map(|(a, _)| a.clone())
            .collect();
        let mut acked = 0usize;
        for agent in sticky_agents {
            let Some(unsure_snapshot) = self.state.unsure_wakes.get(&agent).cloned() else {
                continue;
            };
            let sticky_only = UnsureWakes {
                messages: unsure_snapshot
                    .messages
                    .into_iter()
                    .filter(|m| {
                        m.wakes
                            .iter()
                            .any(|w| matches!(w.about, WakeAbout::Sticky { .. }))
                    })
                    .collect(),
                read_after: unsure_snapshot.read_after,
                failed_delay_secs: unsure_snapshot.failed_delay_secs,
            };
            if sticky_only.messages.is_empty() {
                continue;
            }
            match self.read_messages(&agent).await {
                Ok(recent) => {
                    let sorted = sort_unsure(&sticky_only, recent.as_ref(), now);
                    let mut to_ack = Vec::new();
                    let mut exhausted = Vec::new();
                    if let Some(unsure) = self.state.unsure_wakes.get_mut(&agent) {
                        unsure.failed_delay_secs = None;
                        unsure.read_after = sorted.waiting.then(|| now + SEND_CHECK_AFTER);
                        for msg in &unsure.messages {
                            if sorted.arrived.contains(&msg.digest) {
                                for w in &msg.wakes {
                                    if let WakeAbout::Sticky { wake_id, .. } = w.about {
                                        to_ack.push(wake_id);
                                    }
                                }
                            } else if sorted.missing.iter().any(|x| x.digest == msg.digest) {
                                for w in &msg.wakes {
                                    if let WakeAbout::Sticky { wake_id, .. } = w.about {
                                        if w.resent {
                                            exhausted.push((wake_id, msg.clone()));
                                        } else {
                                            self.sticky_resent.insert(wake_id);
                                        }
                                    }
                                }
                            }
                        }
                        unsure.messages.retain(|m| {
                            !sorted.arrived.contains(&m.digest)
                                && !sorted.missing.iter().any(|x| x.digest == m.digest)
                        });
                        if unsure.messages.is_empty() {
                            self.state.unsure_wakes.remove(&agent);
                        }
                    }
                    for wake_id in to_ack {
                        self.sticky_resent.remove(&wake_id);
                        self.sticky_exhausted.remove(&wake_id);
                        if let Err(err) = self.client().ack_wake(wake_id).await {
                            tracing::warn!(
                                "acking confirmed timed-out sticky wake #{wake_id}: {err:#}"
                            );
                        } else {
                            acked += 1;
                        }
                    }
                    for (wake_id, msg) in exhausted {
                        self.sticky_exhausted.insert(wake_id);
                        self.record_drop(
                            "sticky_wake_resend_missing",
                            msg.wakes.first().map(|w| &w.board),
                            None,
                            Some(&agent),
                            &format!(
                                "sticky wake #{wake_id} timed out and was missing after retry; not resending again"
                            ),
                            serde_json::to_value(&msg).unwrap_or_default(),
                            now,
                        );
                    }
                    let _ = self.save_at(now);
                }
                Err(err) => {
                    if let Some(unsure) = self.state.unsure_wakes.get_mut(&agent) {
                        let last = unsure.failed_delay_secs.map(TimeDelta::seconds);
                        let delay = next_delay(last, WAKE_RETRY_FIRST, WAKE_RETRY_MAX);
                        tracing::warn!(
                            "verifying timed-out sticky wake for {agent}: {err:#}; trying again in {}s",
                            delay.num_seconds()
                        );
                        unsure.failed_delay_secs = Some(delay.num_seconds());
                        unsure.read_after = Some(now + delay);
                    }
                }
            }
        }
        acked
    }

    fn resolve_sticky_wake_target(
        &self,
        wake: &StickyWake,
    ) -> Option<(Arc<dyn Runner>, RunHandle, AgentId)> {
        let managed = wake
            .run_id
            .as_ref()
            .and_then(|id| self.state.managed_runs.get(id))
            .or_else(|| {
                wake.agent_id.as_ref().and_then(|aid| {
                    self.state
                        .managed_runs
                        .values()
                        .find(|r| r.agent_id.as_ref() == Some(aid))
                })
            });
        if let Some(run) = managed
            && let Some(native_id) = &run.native_id
            && let Ok(runner) = self.runners.resolve(Some(&run.runner))
        {
            let mut handle = RunHandle::new(&run.runner, native_id);
            if let Some(wd) = &run.worktree_path {
                handle = handle.with_workdir(wd);
            }
            let agent_id = run
                .agent_id
                .clone()
                .or_else(|| wake.agent_id.clone())
                .or_else(|| handle.resolve_agent_id().ok())?;
            handle = handle.with_agent_id(agent_id.clone());
            return Some((runner, handle, agent_id));
        }
        let agent_id = wake.agent_id.clone()?;
        let runner_kind = match agent_id.source() {
            AgentSource::T3 => "t3",
            AgentSource::Antigravity => "acp",
            _ => self.runner.kind(),
        };
        let runner = self
            .runners
            .resolve(Some(runner_kind))
            .or_else(|_| self.runners.resolve(None))
            .ok()?;
        let handle =
            RunHandle::new(runner.kind(), agent_id.native()).with_agent_id(agent_id.clone());
        Some((runner, handle, agent_id))
    }

    fn remove_unsure_digest(&mut self, agent: &AgentId, digest: &str) {
        if let Some(unsure) = self.state.unsure_wakes.get_mut(agent) {
            unsure.messages.retain(|m| m.digest != digest);
            if unsure.messages.is_empty() {
                self.state.unsure_wakes.remove(agent);
            }
        }
    }

    /// Executes a `kind = "cancel"` sticky wake: flushes final events, calls
    /// [`Runner::cancel`], transitions the server `Run` to [`RunState::Cancelled`], cleans up
    /// `resource_ledger` and worktree, enqueues transcript archival, and acknowledges the wake.
    async fn execute_cancel_wake(&mut self, wake: &StickyWake, now: Timestamp) -> bool {
        let reason = wake
            .payload
            .get("reason")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("cancelled");
        let run_id = wake.run_id.clone();
        let managed = run_id
            .as_ref()
            .and_then(|id| self.state.managed_runs.get(id).cloned());
        if let Some(ref rid) = run_id {
            let _ = self.flush_run_events_for(rid, now).await;
        }
        let target = self.resolve_sticky_wake_target(wake);
        if let Some((ref runner, ref handle, ref agent_id)) = target {
            match tokio::time::timeout(WAKE_SEND_TIMEOUT, runner.cancel(handle, reason)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) if err.downcast_ref::<CancelUnsupported>().is_some() => {
                    tracing::warn!(
                        "sticky cancel wake #{} on {agent_id} unsupported by runner: {err:#}; keeping run live until session exits",
                        wake.id
                    );
                    self.record_drop(
                        "sticky_cancel_unsupported",
                        managed.as_ref().map(|r| &r.board),
                        managed.as_ref().and_then(|r| r.task_id.as_ref()),
                        Some(agent_id),
                        &format!(
                            "runner cannot cancel live session for wake #{}: {err:#}",
                            wake.id
                        ),
                        serde_json::to_value(wake).unwrap_or_default(),
                        now,
                    );
                    // Acknowledge the wake so it does not loop, while keeping the server Run,
                    // conflict_locks, managed_runs, and resource_ledger live until the session
                    // actually terminates (preventing duplicate concurrent workers).
                    if let Err(ack_err) = self.client().ack_wake(wake.id).await {
                        tracing::warn!("acking unsupported cancel wake #{}: {ack_err:#}", wake.id);
                        return false;
                    }
                    return true;
                }
                Ok(Err(err)) => {
                    tracing::warn!(
                        "sticky cancel wake #{} on {agent_id} failed: {err:#}; leaving pending for retry",
                        wake.id
                    );
                    return false;
                }
                Err(_) => {
                    tracing::warn!(
                        "sticky cancel wake #{} on {agent_id} timed out after {}s; leaving pending for retry",
                        wake.id,
                        WAKE_SEND_TIMEOUT.as_secs()
                    );
                    return false;
                }
            }
        }
        if let Some(ref rid) = run_id {
            // Flush final cancellation/teardown events emitted by `runner.cancel` before
            // removing the run from `managed_runs`.
            let _ = self.flush_run_events_for(rid, now).await;
            let outcome = match target.as_ref() {
                Some((runner, handle, _)) => runner
                    .collect_token_usage(handle)
                    .await
                    .filter(RunTokenUsage::is_non_zero)
                    .map(|usage| Some(usage.to_outcome_json())),
                None => None,
            };
            if let Err(err) = self
                .client()
                .update_run(
                    rid,
                    &UpdateRun {
                        state: Some(RunState::Cancelled),
                        reason: Some(clip_run_reason(reason)),
                        outcome,
                        ..UpdateRun::default()
                    },
                )
                .await
                && !matches!(
                    error_code(&err),
                    Some(ErrorCode::InvalidTransition | ErrorCode::NotFound)
                )
            {
                tracing::warn!(
                    "sticky cancel wake #{}: patching run {rid} to cancelled failed: {err:#}",
                    wake.id
                );
                return false;
            }
            let _ =
                self.db
                    .update_local_run_state(rid.as_str(), LocalRunState::Cancelled, None, now);
            let _ = self.db.remove_resource(&format!("run:{rid}"));
            self.state.managed_runs.remove(rid);
            self.sync_resource_ledger(now);
            let board = managed.as_ref().map(|r| r.board.clone()).or_else(|| {
                wake.payload
                    .get("board")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
            });
            let _ = self.enqueue_transcript_archive(rid, board, now);
            if let Some(wt_id) = managed.as_ref().and_then(|r| r.worktree_id.as_deref()) {
                self.cleanup_worktree(wt_id, now).await;
            }
            let _ = self.save_at(now);
        }
        if let Err(err) = self.client().ack_wake(wake.id).await {
            tracing::warn!("acking executed cancel wake #{}: {err:#}", wake.id);
            return false;
        }
        let _ = self.flush_telemetry_outbox(now).await;
        true
    }

    /// Polls [`Runner::poll_events`] for every active managed run and pushes non-empty
    /// batches to `POST /v1/runs/{id}/events`, advancing `last_event_seq` in `daemon.db`.
    pub async fn flush_run_events(&mut self, now: Timestamp) -> Result<usize> {
        if self.run_events_unsupported || self.state.managed_runs.is_empty() {
            return Ok(0);
        }
        let run_ids: Vec<RunId> = self.state.managed_runs.keys().cloned().collect();
        let mut pushed = 0usize;
        for run_id in run_ids {
            if self.run_events_unsupported {
                break;
            }
            match self.flush_run_events_for(&run_id, now).await {
                Ok(n) => pushed += n,
                Err(err) => {
                    tracing::warn!("flushing live events for run {run_id}: {err:#}");
                }
            }
        }
        Ok(pushed)
    }

    async fn flush_run_events_for(&mut self, run_id: &RunId, now: Timestamp) -> Result<usize> {
        if self.run_events_unsupported {
            return Ok(0);
        }
        let Some(run) = self.state.managed_runs.get(run_id).cloned() else {
            return Ok(0);
        };
        let Some(native_id) = run.native_id.as_deref() else {
            return Ok(0);
        };
        let runner = self.runners.resolve(Some(&run.runner))?;
        let mut handle = RunHandle::new(&run.runner, native_id);
        if let Some(wd) = &run.worktree_path {
            handle = handle.with_workdir(wd);
        }
        if let Some(aid) = &run.agent_id {
            handle = handle.with_agent_id(aid.clone());
        }
        let polled = match tokio::time::timeout(
            self.timing.thread_check,
            runner.poll_events(&handle, run.last_event_seq),
        )
        .await
        {
            Ok(Ok(events)) => events,
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                anyhow::bail!(
                    "poll_events for run {run_id} timed out after {}s",
                    self.timing.thread_check.as_secs()
                );
            }
        };
        if polled.is_empty() {
            return Ok(0);
        }
        let mut new_events = Vec::with_capacity(polled.len());
        let mut max_seq = run.last_event_seq;
        for ev in polled {
            if ev.seq == 0 {
                continue;
            }
            max_seq = max_seq.max(ev.seq);
            let summary = clip_event_summary(&ev.summary);
            let summary = if summary.is_empty() {
                "(event)".to_owned()
            } else {
                summary
            };
            let meta = (!ev.metadata.is_empty())
                .then(|| serde_json::to_value(&ev.metadata).ok())
                .flatten()
                .map(|val| {
                    let len = serde_json::to_string(&val)
                        .map(|s| s.len())
                        .unwrap_or(usize::MAX);
                    if len <= board_proto::RUN_EVENT_META_BYTES_MAX {
                        val
                    } else {
                        serde_json::json!({
                            "truncated": true,
                            "original_bytes": len,
                        })
                    }
                });
            new_events.push(NewRunEvent {
                seq: i64::try_from(ev.seq).unwrap_or(i64::MAX),
                at: Some(ev.at),
                kind: map_runner_event_kind(ev.kind),
                summary,
                meta,
            });
        }
        if new_events.is_empty() {
            return Ok(0);
        }
        let mut pushed = 0usize;
        for chunk in new_events.chunks(RUN_EVENTS_BATCH_MAX) {
            let batch_max = chunk
                .iter()
                .map(|e| u64::try_from(e.seq).unwrap_or(0))
                .max()
                .unwrap_or(0);
            match self
                .client()
                .push_run_events(
                    run_id,
                    &PushRunEvents {
                        events: chunk.to_vec(),
                    },
                )
                .await
            {
                Ok(_) => {
                    pushed += chunk.len();
                    if let Some(entry) = self.state.managed_runs.get_mut(run_id) {
                        entry.last_event_seq = entry.last_event_seq.max(batch_max);
                    }
                    let _ = self.save_at(now);
                }
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                    // Structured 404 `not_found` means this specific run does not exist on
                    // the server (not that the endpoint is unsupported).
                    tracing::debug!(
                        "run {run_id} not found on server when pushing live events: {err:#}"
                    );
                    return Ok(pushed);
                }
                Err(err) if error_code(&err) == Some(ErrorCode::Invalid) => {
                    // Permanent 400 validation failure on this batch: record a Trail Rule
                    // drop and advance `last_event_seq` so newer live events are never wedged.
                    self.record_drop(
                        "run_events_rejected",
                        Some(&run.board),
                        run.task_id.as_ref(),
                        run.agent_id.as_ref(),
                        &format!("pushing live events for {run_id} rejected: {err:#}"),
                        serde_json::json!({
                            "run_id": run_id,
                            "batch_len": chunk.len(),
                            "batch_max_seq": batch_max,
                        }),
                        now,
                    );
                    if let Some(entry) = self.state.managed_runs.get_mut(run_id) {
                        entry.last_event_seq = entry.last_event_seq.max(batch_max);
                    }
                    let _ = self.save_at(now);
                }
                Err(err) if error_code(&err).is_none() && is_heartbeat_unsupported(&err) => {
                    self.run_events_unsupported = true;
                    tracing::info!(
                        "server does not support POST /v1/runs/{{id}}/events ({err:#}); disabling live run event pushing until restart"
                    );
                    return Ok(pushed);
                }
                Err(err) => return Err(err),
            }
        }
        if let Some(entry) = self.state.managed_runs.get_mut(run_id) {
            entry.last_event_seq = entry.last_event_seq.max(max_seq);
        }
        let _ = self.save_at(now);
        Ok(pushed)
    }

    /// Computes SHA-256 of `$BOARD_HOME/state/transcripts/<run_id>.jsonl` (if present) and
    /// enqueues a [`TranscriptArchiveJob`] into `daemon.db`'s `telemetry_outbox` when
    /// `[archive] enabled = true`. Deduplicates against already-queued or archived runs.
    pub fn enqueue_transcript_archive(
        &mut self,
        run_id: &RunId,
        board: Option<BoardSlug>,
        now: Timestamp,
    ) -> bool {
        if !self
            .ctx
            .config
            .archive
            .as_ref()
            .is_some_and(|cfg| cfg.enabled)
        {
            return false;
        }
        if self.archived_transcripts.contains(run_id.as_str()) {
            return false;
        }
        let path = state::transcript_path(&self.ctx.home, run_id.as_str());
        if !path.is_file() {
            return false;
        }
        if let Ok(existing) = self
            .db
            .list_telemetry_by_kind(TELEMETRY_KIND_TRANSCRIPT_ARCHIVE)
            && existing.iter().any(|item| {
                serde_json::from_str::<TranscriptArchiveJob>(&item.payload_json)
                    .is_ok_and(|job| job.run_id == *run_id)
            })
        {
            return false;
        }
        let sha256 = match compute_file_sha256(&path) {
            Ok(digest) => digest,
            Err(err) => {
                tracing::warn!(
                    "hashing transcript {} for {run_id}: {err:#}",
                    path.display()
                );
                return false;
            }
        };
        let job = TranscriptArchiveJob {
            run_id: run_id.clone(),
            board,
            file: path,
            sha256,
            uploaded_uri: None,
        };
        let Ok(payload_json) = serde_json::to_string(&job) else {
            return false;
        };
        match self
            .db
            .enqueue_telemetry(TELEMETRY_KIND_TRANSCRIPT_ARCHIVE, &payload_json, now)
        {
            Ok(_) => true,
            Err(err) => {
                tracing::warn!("enqueueing transcript archive for {run_id}: {err:#}");
                false
            }
        }
    }

    /// Scans `$BOARD_HOME/state/transcripts/*.jsonl` on startup/reconnect for local
    /// transcripts belonging to terminal (including `orphaned`) runs that do not yet have a
    /// server-recorded `transcript_uri`, enqueueing them into `telemetry_outbox` best-effort.
    pub async fn reconcile_orphaned_transcripts(&mut self, now: Timestamp) -> usize {
        self.reconcile_orphaned_transcripts_checked(now).await.0
    }

    async fn reconcile_orphaned_transcripts_checked(&mut self, now: Timestamp) -> (usize, bool) {
        if !self
            .ctx
            .config
            .archive
            .as_ref()
            .is_some_and(|cfg| cfg.enabled)
        {
            return (0, true);
        }
        let dir = self.ctx.home.state_dir().join("transcripts");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return (0, true);
        };
        let mut enqueued = 0usize;
        let mut complete = true;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(run_id) = stem.parse::<RunId>() else {
                continue;
            };
            if self.state.managed_runs.contains_key(&run_id)
                || self.archived_transcripts.contains(run_id.as_str())
            {
                continue;
            }
            match self.client().get_run(&run_id).await {
                Ok(run) if run.device != self.ctx.device => {
                    self.archived_transcripts.insert(run_id.to_string());
                }
                Ok(run) if run.state.is_terminal() => {
                    if run.transcript_uri.is_some() {
                        self.archived_transcripts.insert(run_id.to_string());
                    } else if self.enqueue_transcript_archive(&run_id, run.board, now) {
                        enqueued += 1;
                    }
                }
                Ok(_) => {
                    // Untracked local transcript for a still-active run on this device (e.g.
                    // after a hard crash before lease expiry): re-check on later ticks once
                    // the server marks it terminal/orphaned.
                    complete = false;
                }
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                    self.archived_transcripts.insert(run_id.to_string());
                }
                Err(err) => {
                    complete = false;
                    tracing::debug!(
                        "checking orphaned transcript candidate {run_id} on server: {err:#}"
                    );
                }
            }
        }
        (enqueued, complete)
    }

    /// Best-effort bounded extraction of local token usage for a run from either its local
    /// ACP JSONL transcript (`$BOARD_HOME/state/transcripts/<run_id>.jsonl`) or T3 Code's
    /// read-only `statev2.sqlite` projection table.
    async fn collect_local_run_tokens(
        &self,
        run_id: &RunId,
        runner_kind: &str,
        session_id: Option<&str>,
        agent_id: Option<&AgentId>,
        since: Option<Timestamp>,
        until: Option<Timestamp>,
    ) -> Option<RunTokenUsage> {
        let transcript_path = state::transcript_path(&self.ctx.home, run_id.as_str());
        let t3_db_path = self.t3_state_db_path.clone();
        let t3_thread_id = if runner_kind == "t3" {
            session_id.map(str::to_owned).or_else(|| {
                agent_id.and_then(|a| a.as_str().strip_prefix("t3:").map(str::to_owned))
            })
        } else {
            agent_id.and_then(|a| a.as_str().strip_prefix("t3:").map(str::to_owned))
        };
        let task = tokio::task::spawn_blocking(move || {
            if transcript_path.is_file()
                && let Some(usage) =
                    crate::acp_runner::read_acp_transcript_token_usage(&transcript_path)
            {
                return Some(usage);
            }
            if let (Some(db_path), Some(tid)) = (t3_db_path.as_deref(), t3_thread_id.as_deref()) {
                return crate::runner::read_t3_thread_token_usage_window(
                    db_path, tid, since, until,
                );
            }
            None
        });
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .ok()
            .and_then(std::result::Result::ok)
            .flatten()
            .filter(RunTokenUsage::is_non_zero)
    }

    /// Best-effort `PATCH /v1/runs/{id}` attaching backfilled token usage onto a terminal
    /// run whose `outcome` does not yet carry token metrics, preserving any existing keys.
    async fn patch_missing_run_tokens(
        &self,
        run_id: &RunId,
        existing_outcome: Option<&serde_json::Value>,
        usage: RunTokenUsage,
    ) -> bool {
        if !usage.is_non_zero() {
            return false;
        }
        let fetched;
        let current = match existing_outcome {
            Some(v) => Some(v),
            None => match self.client().get_run(run_id).await {
                Ok(run) => {
                    fetched = run.outcome;
                    fetched.as_ref()
                }
                Err(_) => None,
            },
        };
        if outcome_has_tokens(current) {
            return false;
        }
        let merged = usage.merge_into_outcome_json(current);
        match self
            .client()
            .update_run(
                run_id,
                &UpdateRun {
                    outcome: Some(Some(merged)),
                    ..UpdateRun::default()
                },
            )
            .await
        {
            Ok(_) => true,
            Err(err) => {
                tracing::debug!("backfilling token usage onto run {run_id}: {err:#}");
                false
            }
        }
    }

    /// Scans recent terminal runs on this device (both orphaned/terminal managed runs and
    /// completed interactive runs) that recorded no token metrics and backfills available
    /// token usage from local ACP transcripts or T3 Code's read-only SQLite database.
    pub async fn backfill_missing_run_tokens(&mut self, now: Timestamp) -> usize {
        let runs = match self
            .client()
            .list_runs(&board_proto::RunFilter {
                device: Some(self.ctx.device.clone()),
                active: Some(false),
                limit: Some(50),
                ..board_proto::RunFilter::default()
            })
            .await
        {
            Ok(runs) => runs,
            Err(err) => {
                tracing::debug!("listing terminal runs for token backfill: {err:#}");
                return 0;
            }
        };
        if self.token_backfill_checked.len() > 500 {
            self.token_backfill_checked.clear();
        }
        let mut backfilled = 0usize;
        for run in runs {
            if self.token_backfill_checked.contains(run.id.as_str()) {
                continue;
            }
            if outcome_has_tokens(run.outcome.as_ref())
                || matches!(
                    run.kind,
                    RunKind::Merge | RunKind::Housekeeping | RunKind::Command
                )
                || run.runner == DETERMINISTIC_RUNNER_KIND
                || run
                    .ended_at
                    .is_some_and(|ended| now - ended > TimeDelta::days(7))
            {
                self.token_backfill_checked.insert(run.id.to_string());
                continue;
            }
            let usage = self
                .collect_local_run_tokens(
                    &run.id,
                    &run.runner,
                    None,
                    run.agent_id.as_ref(),
                    run.started_at.or(Some(run.created_at)),
                    run.ended_at,
                )
                .await;
            if let Some(usage) = usage {
                if self
                    .patch_missing_run_tokens(&run.id, run.outcome.as_ref(), usage)
                    .await
                {
                    backfilled += 1;
                    self.token_backfill_checked.insert(run.id.to_string());
                }
            } else if run
                .ended_at
                .is_some_and(|ended| now - ended >= TimeDelta::minutes(2))
            {
                self.token_backfill_checked.insert(run.id.to_string());
            }
        }
        backfilled
    }

    /// Processes due entries in `daemon.db`'s `telemetry_outbox` (`next_retry_at <= now`):
    /// runs the configured `[archive]` command, records `transcript_uri` + `transcript_sha256`
    /// via `PATCH /v1/runs/{id}`, and backs off exponentially on transient upload/network
    /// failures.
    pub async fn flush_telemetry_outbox(&mut self, now: Timestamp) -> Result<usize> {
        let Some(archive_cfg) = self.ctx.config.archive.clone().filter(|cfg| cfg.enabled) else {
            return Ok(0);
        };
        let due = self.db.due_telemetry(now, ARCHIVE_MAX_PER_TICK)?;
        let deadline = tokio::time::Instant::now() + ARCHIVE_CMD_TIMEOUT;
        let mut flushed = 0usize;
        for item in due {
            if flushed > 0 && tokio::time::Instant::now() >= deadline {
                break;
            }
            if item.kind != TELEMETRY_KIND_TRANSCRIPT_ARCHIVE {
                continue;
            }
            let mut job: TranscriptArchiveJob = match serde_json::from_str(&item.payload_json) {
                Ok(job) => job,
                Err(err) => {
                    tracing::warn!("dropping unreadable telemetry_outbox #{} ({err})", item.id);
                    self.record_drop(
                        "telemetry_outbox_unreadable",
                        None,
                        None,
                        None,
                        &format!("unreadable transcript_archive payload: {err}"),
                        serde_json::json!({ "id": item.id, "payload": item.payload_json }),
                        now,
                    );
                    let _ = self.db.mark_telemetry_sent(item.id);
                    continue;
                }
            };
            let uri = match job.uploaded_uri.clone() {
                Some(uri) if valid_transcript_uri(&uri) => uri,
                _ => {
                    if job.uploaded_uri.is_some() {
                        job.uploaded_uri = None;
                        if let Ok(cleared) = serde_json::to_string(&job) {
                            let _ = self.db.update_telemetry_payload(item.id, &cleared);
                        }
                    }
                    if !job.file.is_file() {
                        tracing::warn!(
                            "transcript file {} for {} is missing; dropping outbox #{}",
                            job.file.display(),
                            job.run_id,
                            item.id
                        );
                        self.record_drop(
                            "transcript_archive_missing_file",
                            job.board.as_ref(),
                            None,
                            None,
                            &format!("local transcript file {} missing", job.file.display()),
                            serde_json::to_value(&job).unwrap_or_default(),
                            now,
                        );
                        let _ = self.db.mark_telemetry_sent(item.id);
                        continue;
                    }
                    match run_archive_command(&archive_cfg, &job).await {
                        Ok(uri) if valid_transcript_uri(&uri) => {
                            job.uploaded_uri = Some(uri.clone());
                            if let Ok(updated_payload) = serde_json::to_string(&job) {
                                let _ = self.db.update_telemetry_payload(item.id, &updated_payload);
                            }
                            uri
                        }
                        Ok(bad_uri) => {
                            let delay = archive_retry_delay(item.attempts);
                            tracing::warn!(
                                "archive command for {} returned invalid transcript_uri {:?}; retrying in {}s",
                                job.run_id,
                                bad_uri,
                                delay.num_seconds()
                            );
                            let _ = self.db.mark_telemetry_retry(item.id, now + delay);
                            continue;
                        }
                        Err(err) => {
                            let delay = archive_retry_delay(item.attempts);
                            tracing::warn!(
                                "archiving transcript for {} failed (attempt {}): {err:#}; retrying in {}s",
                                job.run_id,
                                item.attempts + 1,
                                delay.num_seconds()
                            );
                            let _ = self.db.mark_telemetry_retry(item.id, now + delay);
                            continue;
                        }
                    }
                }
            };

            match self
                .client()
                .update_run(
                    &job.run_id,
                    &UpdateRun {
                        transcript_uri: Some(Some(uri.clone())),
                        transcript_sha256: Some(Some(job.sha256.clone())),
                        ..UpdateRun::default()
                    },
                )
                .await
            {
                Ok(_) => {
                    let _ = self.db.mark_telemetry_sent(item.id);
                    self.archived_transcripts.insert(job.run_id.to_string());
                    flushed += 1;
                    tracing::info!(
                        "archived transcript for {} to {uri} (sha256={})",
                        job.run_id,
                        &job.sha256[..12.min(job.sha256.len())]
                    );
                }
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                    self.record_drop(
                        "transcript_archive_run_not_found",
                        job.board.as_ref(),
                        None,
                        None,
                        &format!(
                            "run {} not found on server when recording transcript_uri",
                            job.run_id
                        ),
                        serde_json::to_value(&job).unwrap_or_default(),
                        now,
                    );
                    let _ = self.db.mark_telemetry_sent(item.id);
                }
                Err(err) if error_code(&err) == Some(ErrorCode::Forbidden) => {
                    self.record_drop(
                        "transcript_archive_forbidden",
                        job.board.as_ref(),
                        None,
                        None,
                        &format!(
                            "forbidden recording transcript_uri for {}: {err:#}",
                            job.run_id
                        ),
                        serde_json::to_value(&job).unwrap_or_default(),
                        now,
                    );
                    let _ = self.db.mark_telemetry_sent(item.id);
                }
                Err(err) if error_code(&err) == Some(ErrorCode::Invalid) => {
                    // Clear the cached URI so a fixed `[archive]` command can produce a valid
                    // URI on the next attempt instead of retrying the same bad PATCH forever.
                    job.uploaded_uri = None;
                    if let Ok(cleared) = serde_json::to_string(&job) {
                        let _ = self.db.update_telemetry_payload(item.id, &cleared);
                    }
                    let delay = archive_retry_delay(item.attempts);
                    tracing::warn!(
                        "server rejected transcript_uri for {} as invalid ({err:#}); cleared cached URI and retrying in {}s",
                        job.run_id,
                        delay.num_seconds()
                    );
                    let _ = self.db.mark_telemetry_retry(item.id, now + delay);
                }
                Err(err) => {
                    let delay = archive_retry_delay(item.attempts);
                    tracing::warn!(
                        "recording transcript_uri for {} failed (attempt {}): {err:#}; retrying in {}s",
                        job.run_id,
                        item.attempts + 1,
                        delay.num_seconds()
                    );
                    let _ = self.db.mark_telemetry_retry(item.id, now + delay);
                }
            }
        }
        Ok(flushed)
    }

    fn project_for_run(
        &self,
        board: &Board,
        task: Option<&Task>,
    ) -> Option<(Option<String>, PathBuf)> {
        project_for_task_config(&self.ctx.config, board, task)
    }

    /// Resolves the [`Runner`] and [`RunHandle`] responsible for `agent`, checking active
    /// `managed_runs` first and then falling back to `AgentSource` in `self.runners`.
    fn resolve_runner_for_agent(&self, agent: &AgentId) -> (Arc<dyn Runner>, RunHandle) {
        if let Some(run) = self
            .state
            .managed_runs
            .values()
            .find(|r| r.agent_id.as_ref() == Some(agent))
            && let Some(native_id) = &run.native_id
            && let Ok(runner) = self.runners.resolve(Some(&run.runner))
        {
            let mut handle = RunHandle::new(&run.runner, native_id).with_agent_id(agent.clone());
            if let Some(wd) = &run.worktree_path {
                handle = handle.with_workdir(wd);
            }
            return (runner, handle);
        }
        let kind = match agent.source() {
            AgentSource::T3 => "t3",
            AgentSource::Antigravity => "acp",
            _ => self.runner.kind(),
        };
        let runner = self
            .runners
            .resolve(Some(kind))
            .unwrap_or_else(|_| Arc::clone(&self.runner));
        let handle = RunHandle::new(runner.kind(), agent.native()).with_agent_id(agent.clone());
        (runner, handle)
    }

    fn host_project_for_run(
        &self,
        board: &Board,
        task: Option<&Task>,
    ) -> Result<(Option<String>, PathBuf)> {
        let (repo, project) = self
            .project_for_run(board, task)
            .with_context(|| no_project(board))?;
        if folder_gone(&project) {
            anyhow::bail!(
                "the project folder {} of board {} is missing on this device: restore it or update [projects] in config.toml",
                project.display(),
                board.slug
            );
        }
        Ok((repo, project))
    }

    /// The active [`Runner`]'s live status for `thread` (whether it is runnable and whether a
    /// turn is currently active or queued), read within [`Timing::thread_check`].
    async fn thread_status(&self, thread: &str, project: Option<&Path>) -> Result<RunnerStatus> {
        let limit = self.timing.thread_check;
        let mut handle = RunHandle::new(self.runner.kind(), thread);
        if let Some(project) = project {
            handle = handle.with_workdir(project);
        }
        match tokio::time::timeout(limit, self.runner.status(&handle)).await {
            Ok(status) => status,
            Err(_) => Err(TimedOut(format!("{} status {thread}", self.runner.kind())).into()),
        }
    }

    /// Safely cleans up a daemon-managed worktree once its run has ended, logging errors
    /// without failing the reconcile.
    pub(super) async fn cleanup_worktree(&self, worktree_id: &str, now: Timestamp) {
        if let Err(err) = self.worktrees.cleanup(worktree_id, now).await {
            tracing::warn!("cleaning up worktree {worktree_id}: {err:#}");
        }
    }

    /// Reconciles all tracked daemon-managed worktrees not owned by any live worker, judge,
    /// or pull-claimed managed run (including worktrees orphaned by a hard crash or transient
    /// cleanup error).
    async fn cleanup_ended_worktrees(&self, now: Timestamp) {
        let active: BTreeSet<String> = self
            .state
            .workers
            .values()
            .filter_map(|w| w.worktree_id.clone())
            .chain(
                self.state
                    .judges
                    .iter()
                    .filter_map(|r| r.worktree_id.clone()),
            )
            .chain(
                self.state
                    .managed_runs
                    .values()
                    .filter_map(|r| r.worktree_id.clone()),
            )
            .collect();
        if let Err(err) = self.worktrees.cleanup_ended(&active, now).await {
            tracing::warn!("cleaning up ended worktrees: {err:#}");
        }
    }

    /// Delays the next worker start on `board` after one failed at `failed_at`: doubled after each failure, reset when its hosting settings change.
    fn note_failure(&mut self, board: &Board, failed_at: Timestamp) {
        let config = HostingConfig::of(board);
        let last = self
            .failures
            .get(&board.slug)
            .filter(|last| last.config == config)
            .map(|last| last.delay);
        let delay = next_delay(last, START_RETRY_FIRST, START_RETRY_MAX);
        let failure = StartFailure {
            config,
            delay,
            retry_at: failed_at + delay,
        };
        self.failures.insert(board.slug.clone(), failure);
    }

    /// `agent`'s recent messages for checking timed-out wake-up deliveries, read within
    /// [`Timing::thread_check`]; `None` when the runner no longer knows the run/thread.
    async fn read_messages(&self, agent: &AgentId) -> Result<Option<crate::t3::RecentMessages>> {
        let (runner, handle) = self.resolve_runner_for_agent(agent);
        if runner.kind() == "t3" {
            return tokio::time::timeout(
                self.timing.thread_check,
                self.t3.recent_messages(&handle.native_id),
            )
            .await
            .unwrap_or_else(|_| {
                Err(TimedOut(format!("t3 thread show {}", handle.native_id)).into())
            });
        }
        let check = async {
            let status = runner.status(&handle).await?;
            if status.state == RunnerState::Gone && !status.runnable {
                return Ok(None);
            }
            let events = runner.poll_events(&handle, 0).await?;
            let user = events
                .into_iter()
                .filter(|ev| ev.kind == RunEventKind::Message)
                .map(|ev| {
                    let text = match ev.metadata.get("wake_digest") {
                        Some(digest) => format!("{}\n{}", ev.summary, wake_marker(digest)),
                        None => ev.summary,
                    };
                    crate::t3::UserMessage {
                        text,
                        run: ev.metadata.get("run_id").cloned(),
                    }
                })
                .collect();
            Ok(Some(crate::t3::RecentMessages {
                busy: status.busy,
                user,
            }))
        };
        tokio::time::timeout(self.timing.thread_check, check)
            .await
            .unwrap_or_else(|_| {
                Err(TimedOut(format!(
                    "{} poll_events {}",
                    runner.kind(),
                    handle.native_id
                ))
                .into())
            })
    }

    /// What a flush does about `agent`'s timed-out wake-up messages, if any, read without
    /// changing anything: whether its lines wait, and, when a message is due, what a read
    /// of its thread finds of each. `reads` limits how many threads a flush reads at once.
    async fn look_for_unsure(
        &self,
        agent: &AgentId,
        unsure: Option<&UnsureWakes>,
        now: Timestamp,
        reads: &tokio::sync::Semaphore,
    ) -> Look {
        let Some(unsure) = unsure else {
            return Look::default();
        };
        let mut look = Look {
            hold: unsure.holds(now),
            ..Look::default()
        };
        if unsure.read_due(now) {
            let _permit = reads.acquire().await;
            look.read = Some(match self.read_messages(agent).await {
                Ok(recent) => {
                    let sorted = sort_unsure(unsure, recent.as_ref(), now);
                    look.recent = recent;
                    Ok(sorted)
                }
                Err(err) => Err(err),
            });
        }
        look
    }

    /// Records what [`Self::look_for_unsure`] found of `agent`'s timed-out wake-up messages:
    /// those found are settled; the wake-ups of those missing are already in the queue the
    /// flush checked ([`requeue_unsure`]). A failed read backs off.
    fn settle_look(&mut self, agent: &AgentId, read: Option<Result<Sorted>>, now: Timestamp) {
        let Some(read) = read else {
            return;
        };
        let Some(unsure) = self.state.unsure_wakes.get_mut(agent) else {
            return;
        };
        let sorted = match read {
            Ok(sorted) => sorted,
            Err(err) => {
                let last = unsure.failed_delay_secs.map(TimeDelta::seconds);
                let delay = next_delay(last, WAKE_RETRY_FIRST, WAKE_RETRY_MAX);
                tracing::warn!(
                    "waking {agent}: looking for timed-out messages: {err:#}; trying again in \
                     {}s",
                    delay.num_seconds()
                );
                unsure.failed_delay_secs = Some(delay.num_seconds());
                unsure.read_after = Some(now + delay);
                return;
            }
        };
        unsure.failed_delay_secs = None;
        // A run that may still take a message is not asked about on every flush.
        unsure.read_after = sorted.waiting.then(|| now + SEND_CHECK_AFTER);
        unsure.messages.retain(|m| {
            !sorted.arrived.contains(&m.digest)
                && !sorted.missing.iter().any(|x| x.digest == m.digest)
        });
        if unsure.messages.is_empty() {
            self.state.unsure_wakes.remove(agent);
        }
        if !sorted.arrived.is_empty() {
            tracing::info!(
                "woke {agent}: {} timed-out messages reached the thread",
                sorted.arrived.len()
            );
        }
        for _ in sorted.missing {
            tracing::warn!(
                "waking {agent}: a send timed out and the thread never got the message; what \
                 it carried is sent again"
            );
        }
    }

    async fn claim(&self, slug: &BoardSlug, agent: Option<&AgentId>) -> Result<Board> {
        let client = self.client().with_agent(agent.cloned());
        client
            .claim_coordinator(
                slug,
                &ClaimCoordinator {
                    device: self.ctx.device.clone(),
                    agent: agent.cloned(),
                },
            )
            .await
    }

    /// Releases the coordinator lease, logging a failure: the lease expires on its own.
    async fn release(&mut self, slug: &BoardSlug) {
        match self.client().release_coordinator(slug).await {
            Ok(board) => {
                self.boards.insert(slug.clone(), board);
            }
            Err(err) => tracing::warn!("releasing the coordinator lease of {slug}: {err:#}"),
        }
    }

    /// This device's dispatch agent, which holds the lease of the daemon-dispatch boards it
    /// runs.
    fn dispatch_agent(&self) -> AgentId {
        AgentId::dispatch(&self.ctx.device)
    }

    /// Whether `slug` is a board this device runs when it holds the lease.
    fn dispatches_here(&self, slug: &BoardSlug) -> bool {
        self.boards.get(slug).is_some_and(|b| self.is_home(b))
    }

    /// Whether this device dispatches `slug` now: it runs the board, and no other device
    /// holds the board's lease. Steps owed elsewhere would never be done.
    fn dispatching(&self, slug: &BoardSlug) -> bool {
        self.dispatches_here(slug)
            && self.boards.get(slug).is_some_and(|b| {
                b.coordinator
                    .as_ref()
                    .is_none_or(|c| *c == self.dispatch_agent())
            })
    }

    /// The verdict judges that run now on the boards off autopilot, all of them together.
    fn home_verdict_runs(&self) -> usize {
        let judges = self.state.judges.iter();
        let judges = judges.filter(|r| r.kind == board_proto::JudgeKind::Verdict);
        judges
            .filter(|r| {
                let board = self.boards.get(&r.board);
                board.is_some_and(|b| self.judges_proposals(b))
            })
            .count()
    }

    /// Follows and starts the verdict judges of a home board that is not on autopilot.
    /// The server gives no lease there, so the dispatch agent registers them without one.
    async fn judge_proposals(&mut self, board: &Board, now: Timestamp) -> Result<()> {
        let agent = self.dispatch_agent();
        // Before the agent is registered: a device that cannot host the judge's thread
        // asks the server for nothing.
        self.host_folder(board)?;
        if !self.dispatch_registered {
            self.register_dispatch(&agent).await?;
            self.dispatch_registered = true;
        }
        let judged = self
            .judge_and_close(board, &agent, judges::Judging::Verdicts, 0, now)
            .await;
        if judged.is_err() {
            // The server may have lost the agent: registered again on the next try.
            self.dispatch_registered = false;
        }
        judged
    }

    /// Runs a home board: holds its coordinator lease as this
    /// device's dispatch agent, does the steps it owes for agents no device can wake, follows
    /// the workers it started there, and starts workers for the next ready tasks while slots
    /// are free, the board's usage allows new work and no failed start waits for its retry
    /// (`docs/DESIGN.md`, "Daemon dispatch", Slots).
    async fn dispatch(&mut self, board: &Board, now: Timestamp) -> Result<()> {
        let slug = &board.slug;
        let agent = self.dispatch_agent();
        // A device that cannot start workers leaves the lease to a home device that can.
        let (repo_key, project) = match self.host_project(board) {
            Ok(pair) => pair,
            Err(err) => {
                if board.coordinator.as_ref() == Some(&agent) {
                    self.release(slug).await;
                }
                self.dispatch_renewed.remove(slug);
                return Err(err);
            }
        };
        let repo_keys = repo_keys_for_run(repo_key.as_deref(), board, None);
        if let Some(until) = self
            .tcc_repo_backoffs
            .active_for(&repo_keys, &project, now)
            .map(|e| e.until)
        {
            if board.coordinator.as_ref() == Some(&agent) {
                self.release(slug).await;
            }
            self.dispatch_renewed.remove(slug);
            anyhow::bail!(
                "project folder {} of board {} is backed off on {} until {} after macOS TCC privacy refusal",
                project.display(),
                slug,
                self.ctx.device,
                until.to_rfc3339()
            );
        }
        if !self.hold_dispatch_lease(board, &agent, now).await? {
            return Ok(());
        }
        let _ = self.pay_owed(slug, &agent, now).await;
        let followed = self.follow_workers(board, &agent, now).await;
        // A task in `doing` that no worker of this daemon follows also takes a slot: one
        // the user's own thread started, a worker another device started
        // before the lease moved here, or one that claimed after its claim wait.
        let doing = self
            .client()
            .tasks(&TaskQuery {
                board: Some(slug.clone()),
                status: Some(TaskStatus::Doing),
                limit: Some(DOING_READ),
                ..TaskQuery::default()
            })
            .await
            .context("listing the tasks in doing");
        let doing = match doing {
            Ok(doing) => doing,
            Err(err) => {
                self.save()?;
                return Err(err);
            }
        };
        let opened = if self.scan_away_doing(slug, &agent, &doing, now).await {
            self.pay_owed(slug, &agent, now).await
        } else {
            BTreeSet::new()
        };
        let mut in_progress: BTreeMap<TaskId, InProgress> = doing
            .into_iter()
            .filter(|t| t.status == TaskStatus::Doing && !opened.contains(&t.id))
            .map(|t| {
                let since = t.updated_at;
                (
                    t.id.clone(),
                    InProgress {
                        task: Some(t),
                        since,
                    },
                )
            })
            .collect();
        // A held task that someone else started is held no longer.
        for task in in_progress.keys() {
            self.hold_ended("worker", task.as_str());
            self.state.hold_notes.remove(task);
        }
        let mut left = Vec::new();
        for (task, followed) in followed {
            if opened.contains(&task) {
                continue;
            }
            match followed {
                Followed::Holds {
                    task: current,
                    since,
                } => {
                    let entry = in_progress
                        .entry(task)
                        .or_insert(InProgress { task: None, since });
                    entry.since = since;
                    if let Some(current) = current {
                        entry.task = Some(*current);
                    }
                }
                Followed::Left => left.push(task),
                Followed::Freed => {}
            }
        }
        // A worker that gave its task back while other tasks were in `doing` or `review`
        // stopped on a lease overlap, by its brief (tasks in `review` keep their file
        // leases): that task waits until one of them leaves `doing` and `review`. With
        // nothing else in `doing` or `review` it stopped for another reason, and waits out
        // the retry.
        let needs_review =
            !left.is_empty() || self.state.worker_skips.keys().any(|t| t.board() == *slug);
        let in_review: BTreeSet<TaskId> = if needs_review {
            match self
                .client()
                .tasks(&TaskQuery {
                    board: Some(slug.clone()),
                    status: Some(TaskStatus::Review),
                    limit: Some(board_proto::TASKS_LIMIT_MAX),
                    ..TaskQuery::default()
                })
                .await
                .context("listing the tasks in review")
            {
                Ok(tasks) => tasks
                    .into_iter()
                    .filter(|t| t.status == TaskStatus::Review && !opened.contains(&t.id))
                    .map(|t| t.id)
                    .collect(),
                Err(err) => {
                    self.save_at(now)?;
                    return Err(err);
                }
            }
        } else {
            BTreeSet::new()
        };
        for task in left {
            let others: BTreeSet<TaskId> = in_progress
                .keys()
                .chain(in_review.iter())
                .filter(|t| **t != task)
                .cloned()
                .collect();
            if others.is_empty() {
                let retry_at = now + WORKER_RETRY;
                tracing::warn!(
                    "the worker of {task} left it open; no new worker for it before {retry_at}"
                );
                self.record_drop(
                    "worker_give_back",
                    Some(slug),
                    Some(&task),
                    None,
                    &format!(
                        "worker left task open with no other tasks in doing or review; waiting {}s until {retry_at}",
                        WORKER_RETRY.num_seconds()
                    ),
                    serde_json::json!({ "task": task, "retry_at": retry_at }),
                    now,
                );
                self.state.worker_retry.insert(task, retry_at);
            } else {
                let names: Vec<&str> = others.iter().map(|t| t.as_str()).collect();
                tracing::info!(
                    "the worker of {task} left it open beside {}; it starts again once one of them leaves doing/review",
                    names.join(", ")
                );
                self.record_drop(
                    "worker_give_back",
                    Some(slug),
                    Some(&task),
                    None,
                    &format!(
                        "worker left task open beside {} other task(s) in doing or review; waiting until one leaves doing/review",
                        others.len()
                    ),
                    serde_json::json!({ "task": task, "waiting_on": others }),
                    now,
                );
                self.state.worker_skips.insert(task, others);
            }
        }
        self.state.worker_skips.retain(|task, others| {
            task.board() != *slug
                || others
                    .iter()
                    .all(|t| in_progress.contains_key(t) || in_review.contains(t))
        });
        self.state
            .idle_notices
            .retain(|task, _| task.board() != *slug || in_progress.contains_key(task));
        self.save_at(now)?;
        // A failure here leaves the workers their turn.
        if let Err(err) = self
            .judge_and_close(board, &agent, judges::Judging::All, in_progress.len(), now)
            .await
        {
            tracing::warn!("the judges and reviewed tasks of {slug}: {err:#}");
        }
        self.save_at(now)?;
        // The board asks again below if it still wants one of the device's slots.
        let mut waited = self.worker_turns.take(slug);
        let slots = self.pace_limit(board.parallel_tasks.max(1), now) as usize;
        if self.usage_holds(slug, now) {
            let hold_key = format!("usage_hold worker {slug}");
            if !self.state.holds.contains_key(&hold_key) {
                self.record_drop(
                    "usage_hold",
                    Some(slug),
                    None,
                    Some(&agent),
                    &format!(
                        "usage holds worker dispatch on {slug} (usage_stop_at={}, pace_limit={slots})",
                        board.usage_stop_at
                    ),
                    serde_json::json!({
                        "kind": "worker",
                        "board": slug,
                        "usage_stop_at": board.usage_stop_at,
                        "pace_limit": slots,
                    }),
                    now,
                );
                self.state.holds.insert(hold_key, now);
            }
            return Ok(());
        }
        self.hold_ended("usage_hold", &format!("worker {slug}"));
        if in_progress.len() >= slots {
            return Ok(());
        }
        if let Some(failure) = self.failures.get(slug)
            && failure.config == HostingConfig::of(board)
            && now < failure.retry_at
        {
            return Ok(());
        }
        if !in_progress.is_empty() && !self.may_start_beside(slug, &in_progress, now).await? {
            return Ok(());
        }
        for task in self.next_tasks(slug, now).await? {
            if in_progress.len() >= slots {
                break;
            }
            if !independent(&task, &in_progress) {
                continue;
            }
            // The device's limit holds across boards, and the board that waited longest
            // for a slot gets the next one.
            self.read_running().await;
            self.sync_resource_ledger(now);
            let daemon_limit = self.daemon_run_limit();
            let flat_limit = device::worker_limit(daemon_limit);
            let needed_units = device::cost_units_for_task(&task);
            let weighted_mode = self.ctx.config.weight_units.is_some();
            let weighted_held = self.weighted_full(needed_units);
            let flat_held =
                !weighted_mode && (self.worker_runs() >= flat_limit || self.runs() >= daemon_limit);
            if self.load_unknown().is_some()
                || flat_held
                || weighted_held
                || self.worker_turns.another_first(slug, waited)
            {
                let (limit, load) = if weighted_mode || (!flat_held && weighted_held) {
                    self.weighted_load(needed_units)
                } else {
                    (flat_limit, self.load())
                };
                if self.worker_turns.wait(slug, waited, now) {
                    tracing::info!(
                        "{slug}: {} waits for one of the {limit} worker slots of this device; {load}",
                        task.id
                    );
                }
                // Each task the limit holds gets its record, not only the one that
                // started the board's wait.
                self.record_hold("worker", slug, task.id.as_str(), limit, &load, now);
                self.note_held(&task.id, &agent, limit, &load, now).await;
                break;
            }
            let task = match self
                .recheck_worker_task(&task.id, slug, &agent, now)
                .await?
            {
                WorkerRecheck::Ready(fresh) => *fresh,
                WorkerRecheck::InProgress(current) => {
                    self.state.hold_notes.remove(&current.id);
                    self.hold_ended("worker", current.id.as_str());
                    let since = current.updated_at;
                    in_progress.insert(
                        current.id.clone(),
                        InProgress {
                            task: Some(*current),
                            since,
                        },
                    );
                    continue;
                }
                WorkerRecheck::Skipped => {
                    self.state.hold_notes.remove(&task.id);
                    self.hold_ended("worker", task.id.as_str());
                    continue;
                }
            };
            let fresh_units = device::cost_units_for_task(&task);
            if fresh_units > needed_units && self.weighted_full(fresh_units) {
                let (limit, load) = self.weighted_load(fresh_units);
                if self.worker_turns.wait(slug, waited, now) {
                    tracing::info!(
                        "{slug}: {} waits for one of the {limit} worker slots of this device; {load}",
                        task.id
                    );
                }
                self.record_hold("worker", slug, task.id.as_str(), limit, &load, now);
                self.note_held(&task.id, &agent, limit, &load, now).await;
                break;
            }
            self.state.hold_notes.remove(&task.id);
            self.hold_ended("worker", task.id.as_str());
            if let Err(err) = self.start_worker(board, &task, &agent, now).await {
                self.note_failure(board, now);
                // One task that cannot start does not hold up the others. A start that
                // recorded its worker gets this when that worker is found to have no thread.
                if !self.state.workers.contains_key(&task.id) {
                    self.state
                        .worker_retry
                        .insert(task.id.clone(), now + WORKER_RETRY);
                }
                return Err(err.context(format!("starting a worker for {}", task.id)));
            }
            self.failures.remove(slug);
            // Served: a further task of this board waits behind the boards that asked.
            waited = None;
            in_progress.insert(
                task.id.clone(),
                InProgress {
                    task: Some(task),
                    since: now,
                },
            );
        }
        Ok(())
    }

    /// Whether a further worker may start on `slug` beside the tasks in progress: each of
    /// them could be read, and no agent of the board stopped on a usage limit since the
    /// first of them started that still holds at `now`. A stop no longer holds once the
    /// agent runs again or its reset has passed, or when a fresh usage reading taken after
    /// it shows no full window of the account: then the limit was not the account's, such
    /// as one model's limit, and does not stop the workers.
    async fn may_start_beside(
        &self,
        slug: &BoardSlug,
        in_progress: &BTreeMap<TaskId, InProgress>,
        now: Timestamp,
    ) -> Result<bool> {
        if in_progress.values().any(|p| p.task.is_none()) {
            return Ok(false);
        }
        let Some(first) = in_progress.values().map(|p| p.since).min() else {
            return Ok(true);
        };
        let account_not_full_since = self
            .usage
            .as_ref()
            // A window at 100 percent without a reset time counts as full, and a reading
            // without windows says nothing.
            .filter(|u| u.fresh(now) && u.windows().next().is_some() && u.over(100, now).is_none())
            .map(|u| u.read_at);
        for (agent, at) in &self.state.usage_stops {
            if *at < first || account_not_full_since.is_some_and(|read| read > *at) {
                continue;
            }
            let holds = match self.client().agent_info(agent).await {
                // The server keeps an ended agent's `paused_until` only while its reset is
                // ahead; a paused agent without one waits for a reset nobody knows.
                Ok(info) => {
                    info.boards.contains(slug)
                        && match info.state {
                            AgentState::Paused => info.paused_until.is_none_or(|until| until > now),
                            AgentState::Ended => info.paused_until.is_some_and(|until| until > now),
                            AgentState::Running | AgentState::Idle => false,
                        }
                }
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => false,
                Err(err) => return Err(err.context("reading an agent that stopped on usage")),
            };
            if holds {
                tracing::info!(
                    "{agent} stopped on a usage limit at {at}, after the tasks in progress on {slug} started; no further worker starts beside them"
                );
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Claims or renews the lease of `board` as `agent`, every [`RENEW_EVERY`]; false when
    /// another device holds it.
    async fn hold_dispatch_lease(
        &mut self,
        board: &Board,
        agent: &AgentId,
        now: Timestamp,
    ) -> Result<bool> {
        let slug = &board.slug;
        let named = board.coordinator.as_ref() == Some(agent);
        // A clock set back renews at once.
        if named
            && self
                .dispatch_renewed
                .get(slug)
                .is_some_and(|at| (TimeDelta::zero()..RENEW_EVERY).contains(&(now - *at)))
        {
            return Ok(true);
        }
        if !self.dispatch_registered {
            self.register_dispatch(agent).await?;
            self.dispatch_registered = true;
        }
        match self.claim(slug, Some(agent)).await {
            Ok(updated) => {
                if !named {
                    tracing::info!("{agent} dispatches {slug}");
                }
                self.hold_ended("claim_refused", slug.as_str());
                self.boards.insert(slug.clone(), updated);
                self.dispatch_renewed.insert(slug.clone(), now);
                Ok(true)
            }
            Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => {
                self.dispatch_renewed.remove(slug);
                let hold_key = format!("claim_refused {slug}");
                if !self.state.holds.contains_key(&hold_key) {
                    self.record_drop(
                        "claim_refused",
                        Some(slug),
                        None,
                        Some(agent),
                        &format!("coordinator claim refused for {slug}: {err:#}"),
                        serde_json::json!({
                            "board": slug,
                            "coordinator": board.coordinator,
                        }),
                        now,
                    );
                    self.state.holds.insert(hold_key, now);
                }
                Ok(false)
            }
            Err(err) => {
                // The server may have lost the agent: registered again on the next try.
                self.dispatch_registered = false;
                Err(err)
            }
        }
    }

    fn dispatch_runtime(&self) -> Runtime {
        Self::runtime_for_config(&self.ctx.config)
    }

    /// Maps configured provider/runner settings (`[thread_model]`, `[judge_model]`,
    /// `[judge_models]`, and `[acp_runner]`, aligned with [`Config::usage_provider`] and
    /// [`Config::quota_bucket`]) to the [`Runtime`] reported by `dispatch:<device>`.
    fn runtime_for_config(cfg: &crate::config::Config) -> Runtime {
        if let Some(p) = cfg.usage_provider() {
            let lower = p.to_ascii_lowercase();
            if lower.contains("antigravity") || lower.contains("gemini") {
                return Runtime::Antigravity;
            }
            if lower.contains("codex") || lower.contains("openai") {
                return Runtime::Codex;
            }
            if lower.contains("opencode") {
                return Runtime::Opencode;
            }
            if lower.contains("claude") || lower.contains("anthropic") {
                return Runtime::Claude;
            }
        }
        if let Some(acp) = &cfg.acp_runner
            && acp.resolved_bin().is_some()
        {
            let hint = acp
                .model
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if hint.contains("claude") || hint.contains("anthropic") {
                return Runtime::Claude;
            }
            if hint.contains("codex") || hint.contains("openai") || hint.contains("gpt") {
                return Runtime::Codex;
            }
            if hint.contains("opencode") {
                return Runtime::Opencode;
            }
            return Runtime::Antigravity;
        }
        Runtime::Claude
    }

    async fn register_dispatch(&self, agent: &AgentId) -> Result<()> {
        self.client()
            .with_agent(Some(agent.clone()))
            .register(&RegisterAgent {
                id: agent.clone(),
                // The runtime of the threads this dispatcher starts.
                runtime: self.dispatch_runtime(),
                device: self.ctx.device.clone(),
                role: AgentRole::Coordinator,
                title: Some(format!("dispatcher on {}", self.ctx.device)),
                repo: None,
                branch: None,
                cwd: None,
                native_ids: BTreeMap::new(),
                forked_from: None,
            })
            .await?;
        Ok(())
    }

    /// Updates the workers started on `board` from their tasks, and returns what became of
    /// each. One that cannot be read holds its slot until it can.
    async fn follow_workers(
        &mut self,
        board: &Board,
        agent: &AgentId,
        now: Timestamp,
    ) -> Vec<(TaskId, Followed)> {
        let tasks: Vec<TaskId> = self
            .state
            .workers
            .iter()
            .filter(|(task, _)| task.board() == board.slug)
            .map(|(task, _)| task.clone())
            .collect();
        let mut followed = Vec::new();
        for task in tasks {
            let Some(mut worker) = self.state.workers.get(&task).cloned() else {
                continue;
            };
            let outcome = match self
                .follow_worker(board, &task, &mut worker, agent, now)
                .await
            {
                Ok(outcome) => outcome,
                Err(err) => {
                    tracing::warn!("worker of {task}: {err:#}");
                    Followed::Holds {
                        task: None,
                        since: worker.started_at,
                    }
                }
            };
            if matches!(outcome, Followed::Holds { .. }) {
                self.state.workers.insert(task.clone(), worker);
            } else {
                self.state.workers.remove(&task);
                if let Some(run_id) = worker.run_id.as_deref() {
                    let _ =
                        self.db
                            .update_local_run_state(run_id, LocalRunState::Finished, None, now);
                } else {
                    let _ = self.db.finish_local_runs_for_task(
                        "worker",
                        &task,
                        LocalRunState::Finished,
                        now,
                    );
                }
                self.sync_resource_ledger(now);
                if let Some(wt_id) = worker.worktree_id.as_deref() {
                    self.cleanup_worktree(wt_id, now).await;
                }
            }
            followed.push((task, outcome));
        }
        followed
    }

    /// Whether `worker` still holds a slot: it has the task in `doing`, or may still claim
    /// it. A start that did not record its thread looks for it first, and a thread not
    /// noted on the task yet is noted. A worker that ends without claiming the task holds
    /// it from another worker for [`WORKER_RETRY`]; one that gives it back to `open` after
    /// claiming it is [`Followed::Left`].
    async fn follow_worker(
        &mut self,
        board: &Board,
        task: &TaskId,
        worker: &mut Worker,
        agent: &AgentId,
        now: Timestamp,
    ) -> Result<Followed> {
        let thread = match worker.thread.clone() {
            Some(thread) => thread,
            None => match self.find_worker_thread(board, task, worker).await? {
                Some(thread) => {
                    worker.thread = Some(thread.clone());
                    if let Some(run_id) = worker.run_id.as_deref() {
                        let _ = self.db.update_local_run_state(
                            run_id,
                            LocalRunState::Running,
                            Some(&thread),
                            now,
                        );
                    }
                    thread
                }
                None => {
                    self.state
                        .worker_retry
                        .insert(task.clone(), now + WORKER_RETRY);
                    return Ok(Followed::Freed);
                }
            },
        };
        if !worker.noted {
            match self.note_worker(task, &thread, worker, agent).await {
                Ok(()) => worker.noted = true,
                Err(err) => tracing::warn!("noting worker thread {thread} on {task}: {err:#}"),
            }
        }
        let current = match self.client().task(task).await {
            Ok(detail) => detail.task,
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                return Ok(Followed::Freed);
            }
            Err(err) => return Err(err),
        };
        let holds = |current: Task| Followed::Holds {
            task: Some(Box::new(current)),
            since: worker.started_at,
        };
        let worker_agent = match &worker.agent_id {
            Some(agent_id) => agent_id.clone(),
            None => RunHandle::new(worker.runner.as_deref().unwrap_or("t3"), &thread)
                .resolve_agent_id()?,
        };
        if current.status == TaskStatus::Doing && current.owner.as_ref() == Some(&worker_agent) {
            worker.claimed = true;
            return Ok(holds(current));
        }
        let open = current.status == TaskStatus::Open && current.owner.is_none();
        if worker.claimed {
            // It parked the task, finished it, or gave it back.
            return Ok(if open {
                Followed::Left
            } else {
                Followed::Freed
            });
        }
        if !open {
            // Closed, or taken by another agent, before the worker claimed it.
            return Ok(Followed::Freed);
        }
        let ended = match self.client().agent_info(&worker_agent).await {
            Ok(info) => info.state == AgentState::Ended,
            // Its session has not started yet.
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => false,
            Err(err) => return Err(err),
        };
        if !ended && now - worker.started_at < WORKER_CLAIM_WAIT {
            return Ok(holds(current));
        }
        tracing::warn!(
            "worker thread {thread} did not claim {task}; no new worker for it before {}",
            now + WORKER_RETRY
        );
        self.state
            .worker_retry
            .insert(task.clone(), now + WORKER_RETRY);
        Ok(Followed::Freed)
    }

    /// The thread of a worker start that did not record it (the daemon was killed, or `t3`
    /// failed or timed out): the newest thread with its title created since, briefed when its
    /// message never reached it; `None` when there is none, or it cannot be briefed.
    async fn find_worker_thread(
        &self,
        board: &Board,
        task: &TaskId,
        worker: &Worker,
    ) -> Result<Option<String>> {
        if worker.runner.as_deref().unwrap_or("t3") != "t3" {
            return Ok(None);
        }
        let project = self.host_folder(board)?;
        let found = self
            .t3
            .threads_titled(&project, &worker.title)
            .await
            .context("looking for the thread of an unfinished worker start")?
            .into_iter()
            .filter(|t| t.created_at >= worker.started_at && thread_agent(&t.id).is_ok())
            .max_by_key(|t| t.created_at);
        let Some(found) = found else {
            tracing::warn!(
                "the worker start for {task} at {} made no thread",
                worker.started_at
            );
            return Ok(None);
        };
        if !found.ran {
            let brief = crate::brief::worker_brief(&board.slug, task, crate::brief::WORKER_CLOSER);
            let sent = if self.runner.kind() == "t3" {
                self.runner
                    .send(&RunHandle::t3(&found.id), &SteerMessage::new(&brief))
                    .await
            } else {
                self.t3.send(&found.id, &brief).await
            };
            if let Err(err) = sent {
                tracing::warn!(
                    "briefing worker thread {} left by an earlier start for {task}: {err:#}; archive it in T3",
                    found.id
                );
                return Ok(None);
            }
        }
        tracing::warn!(
            "worker of {task}: adopting thread {}, which a start at {} launched but did not record",
            found.id,
            worker.started_at
        );
        Ok(Some(found.id))
    }

    /// Notes on `task` the worker thread started for it, by its T3 link when T3 gives one.
    async fn note_worker(
        &self,
        task: &TaskId,
        thread: &str,
        worker: &Worker,
        agent: &AgentId,
    ) -> Result<()> {
        let runner_kind = worker.runner.as_deref().unwrap_or("t3");
        let link = if runner_kind == "t3" {
            match self.t3.thread_link(thread).await {
                Ok(Some(link)) => link,
                Ok(None) => format!("thread {thread}"),
                Err(err) => {
                    tracing::debug!("reading the link of thread {thread}: {err:#}");
                    format!("thread {thread}")
                }
            }
        } else {
            format!("{runner_kind} run {thread}")
        };
        let branch = worker
            .branch
            .as_ref()
            .map(|b| format!(" on branch {b}"))
            .unwrap_or_default();
        let text = format!("{WORKER_NOTE_PREFIX}{branch}: {link}");
        self.client()
            .with_agent(Some(agent.clone()))
            .add_note(
                task,
                &AddNote {
                    agent: agent.clone(),
                    kind: NoteKind::Note,
                    text,
                },
            )
            .await?;
        Ok(())
    }

    /// The tasks the next workers on `slug` take, in order: the ready `auto` tasks that
    /// have no worker, no retry still to wait for and no skip, the highest priority first,
    /// inside it the ranked tasks by rank, then the rest, oldest first ([`start_order`]).
    async fn next_tasks(&self, slug: &BoardSlug, now: Timestamp) -> Result<Vec<Task>> {
        let tasks = self
            .client()
            .tasks(&TaskQuery {
                board: Some(slug.clone()),
                ready: Some(true),
                proposal: Some(false),
                limit: Some(1000),
                ..TaskQuery::default()
            })
            .await?;
        let mut tasks: Vec<Task> = tasks
            .into_iter()
            // A feedback item goes to a triage judge, not to a worker.
            .filter(worker_eligible)
            .filter(|t| !self.state.workers.contains_key(&t.id))
            .filter(|t| !self.state.worker_skips.contains_key(&t.id))
            .filter(|t| {
                self.state
                    .worker_retry
                    .get(&t.id)
                    .is_none_or(|at| *at <= now)
            })
            .collect();
        tasks.sort_by_key(start_order);
        Ok(tasks)
    }

    /// Re-reads `task_id` right before starting a worker so a claim, close, or update after
    /// `next_tasks` does not waste a worker thread, worktree, or branch. Writes a Trail Rule
    /// record before skipping any task that is gone or no longer eligible.
    async fn recheck_worker_task(
        &self,
        task_id: &TaskId,
        slug: &BoardSlug,
        agent: &AgentId,
        now: Timestamp,
    ) -> Result<WorkerRecheck> {
        let detail = match self.client().task(task_id).await {
            Ok(detail) => detail,
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                let cause = format!("task {task_id} not found before worker start");
                self.record_drop(
                    "worker_skipped_not_ready",
                    Some(slug),
                    Some(task_id),
                    Some(agent),
                    &cause,
                    serde_json::json!({
                        "task": task_id,
                        "error": "not_found",
                    }),
                    now,
                );
                tracing::info!("{slug}: {cause}; skipping worker start");
                return Ok(WorkerRecheck::Skipped);
            }
            Err(err) => {
                return Err(err.context(format!("re-reading {task_id} before starting a worker")));
            }
        };
        if worker_eligible(&detail.task) {
            return Ok(WorkerRecheck::Ready(Box::new(detail.task)));
        }
        let cause = worker_ineligible_cause(&detail.task);
        self.record_drop(
            "worker_skipped_not_ready",
            Some(slug),
            Some(task_id),
            Some(agent),
            &cause,
            worker_ineligible_subject(&detail.task, None),
            now,
        );
        tracing::info!("{slug}: {cause}; skipping worker start");
        if detail.task.status == TaskStatus::Doing {
            Ok(WorkerRecheck::InProgress(Box::new(detail.task)))
        } else {
            Ok(WorkerRecheck::Skipped)
        }
    }

    /// Starts a worker run for `task`, in a new worktree on `task/<id>` when the project
    /// folder is a repository's top, with the worker brief; records it before the start, so
    /// a start the daemon does not live to finish is found again, then notes it on the task.
    async fn start_worker(
        &mut self,
        board: &Board,
        task: &Task,
        agent: &AgentId,
        now: Timestamp,
    ) -> Result<()> {
        let (repo_key, project) = self.host_project(board)?;
        let repo_keys = repo_keys_for_run(repo_key.as_deref(), board, Some(task));
        let base_branch = self
            .ctx
            .config
            .base_branch(&board.slug, repo_key.as_deref())
            .map(ToOwned::to_owned);
        let worktree = match crate::worktree::worker_worktree_with_base(
            &project,
            &task.id,
            base_branch.as_deref(),
            now,
        )
        .await
        .context("preparing the worker's worktree")
        {
            Ok(worktree) => {
                if worktree.is_some() {
                    self.clear_tcc_repo_backoff(&repo_keys, &project);
                }
                worktree
            }
            Err(err) => {
                if let Some(refusal) = crate::worktree::privacy_refusal(&err) {
                    self.record_tcc_repo_backoff(
                        &board.slug,
                        Some(&task.id),
                        None,
                        &repo_keys,
                        &project,
                        &refusal.detail,
                        now,
                    );
                    // Unconditionally release: `hold_dispatch_lease` may have acquired
                    // the lease earlier in this tick without mutating `board.coordinator`.
                    self.release(&board.slug).await;
                    self.dispatch_renewed.remove(&board.slug);
                }
                return Err(err);
            }
        };
        let title = format!("{} {}", task.id, task.title);
        let brief = crate::brief::worker_brief_for_task_with_base(
            &board.slug,
            task,
            crate::brief::WORKER_CLOSER,
            self.runner.kind() == "t3",
            base_branch.as_deref(),
        );
        let run_id = format!("worker-{}-{}", task.id, now.format("%Y%m%d-%H%M%S"));
        let cost_units = device::cost_units_for_task(task);
        let intended_branch = worktree.as_ref().map(|w| w.branch.clone());

        let preassigned_agent_id = (self.runner.kind() == "acp")
            .then(|| format!("antigravity:{run_id}").parse::<AgentId>().ok())
            .flatten();
        // Write-before-act: persist local_runs intent and worker record naming the planned
        // branch into daemon.db BEFORE creating any git worktree or spawning the runner
        // (docs/SCHEDULER_DESIGN.md Section 4.1, agent-board-539).
        let mut worker = Worker {
            thread: None,
            title: title.clone(),
            branch: intended_branch.clone(),
            runner: Some(self.runner.kind().to_owned()),
            agent_id: preassigned_agent_id.clone(),
            worktree_id: None,
            worktree_path: None,
            run_id: Some(run_id.clone()),
            cost_units: Some(cost_units),
            started_at: now,
            claimed: false,
            noted: false,
        };
        let mut local_run = LocalRunRecord {
            run_id: run_id.clone(),
            task_id: Some(task.id.clone()),
            board: board.slug.clone(),
            kind: "worker".to_owned(),
            runner: self.runner.kind().to_owned(),
            session_id: None,
            worktree_path: None,
            branch: intended_branch,
            cost_units,
            pid: None,
            state: LocalRunState::Intent,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
            meta_json: serde_json::to_string(&worker).ok(),
        };
        self.db.upsert_local_run(&local_run)?;
        self.state.workers.insert(task.id.clone(), worker.clone());
        if let Err(err) = self.save_at(now) {
            self.state.workers.remove(&task.id);
            let _ = self
                .db
                .update_local_run_state(&run_id, LocalRunState::Failed, None, now);
            return Err(err);
        }
        self.sync_resource_ledger(now);

        let (workdir, spec_worktree) = if self.daemon_worktrees
            && let Some(spec) = &worktree
        {
            match self
                .worktrees
                .create(&project, repo_key.as_deref(), &run_id, spec, now)
                .await
            {
                Ok(managed) => {
                    worker.branch = Some(managed.branch.clone());
                    worker.worktree_id = Some(managed.id);
                    worker.worktree_path = Some(managed.path.clone());
                    local_run.branch = Some(managed.branch);
                    local_run.worktree_path = Some(managed.path.clone());
                    local_run.state = LocalRunState::Starting;
                    local_run.updated_at = now;
                    local_run.meta_json = serde_json::to_string(&worker).ok();
                    self.state.workers.insert(task.id.clone(), worker.clone());
                    let persisted = self
                        .db
                        .upsert_local_run(&local_run)
                        .and_then(|()| self.save_at(now));
                    if let Err(err) = persisted {
                        self.state.workers.remove(&task.id);
                        let _ = self.db.update_local_run_state(
                            &run_id,
                            LocalRunState::Failed,
                            None,
                            now,
                        );
                        self.sync_resource_ledger(now);
                        if let Some(wt_id) = worker.worktree_id.as_deref() {
                            self.cleanup_worktree(wt_id, now).await;
                        }
                        return Err(err);
                    }
                    (managed.path, None)
                }
                Err(err) => {
                    self.state.workers.remove(&task.id);
                    let _ =
                        self.db
                            .update_local_run_state(&run_id, LocalRunState::Failed, None, now);
                    self.sync_resource_ledger(now);
                    let _ = self.save_at(now);
                    if let Some(refusal) = crate::worktree::privacy_refusal(&err) {
                        self.record_tcc_repo_backoff(
                            &board.slug,
                            Some(&task.id),
                            None,
                            &repo_keys,
                            &project,
                            &refusal.detail,
                            now,
                        );
                        // Unconditionally release: `hold_dispatch_lease` may have acquired
                        // the lease earlier in this tick without mutating `board.coordinator`.
                        self.release(&board.slug).await;
                        self.dispatch_renewed.remove(&board.slug);
                    }
                    return Err(err.context("creating daemon-managed worker worktree"));
                }
            }
        } else {
            local_run.state = LocalRunState::Starting;
            local_run.updated_at = now;
            self.db.upsert_local_run(&local_run)?;
            (project.clone(), worktree.clone())
        };
        let mut metadata = BTreeMap::from([
            ("board".to_owned(), board.slug.to_string()),
            ("task".to_owned(), task.id.to_string()),
            ("kind".to_owned(), "worker".to_owned()),
        ]);
        if let Some(agent_id) = &preassigned_agent_id {
            metadata.insert("agent_id".to_owned(), agent_id.to_string());
        }
        let spec = RunSpec {
            run_id: run_id.clone(),
            title: worker.title.clone(),
            prompt: brief,
            model: self.ctx.config.thread_model.clone(),
            worktree: spec_worktree,
            metadata,
        };
        let handle = match self.runner.spawn(&spec, &workdir).await {
            Ok(handle) => handle,
            Err(err) => {
                if let Some(wt_id) = worker.worktree_id.as_deref() {
                    self.cleanup_worktree(wt_id, now).await;
                }
                return Err(err);
            }
        };
        let thread = handle.native_id.clone();
        // A handle id that names no agent could never be followed, and would hold the slot.
        let resolved_agent = if self.runner.kind() == "t3" {
            thread_agent(&thread)
                .with_context(|| format!("t3 started thread {thread:?}; archive it in T3"))?
        } else {
            handle.resolve_agent_id()?
        };
        if handle.runner == "t3" {
            tracing::info!(
                "started worker thread {thread} for {} on {}",
                task.id,
                board.slug
            );
        } else {
            tracing::info!(
                "started worker {thread} ({}) for {} on {}",
                handle.runner,
                task.id,
                board.slug
            );
        }
        worker.thread = Some(thread.clone());
        worker.runner = Some(handle.runner.clone());
        worker.agent_id = Some(resolved_agent);
        local_run.session_id = Some(thread.clone());
        local_run.runner = handle.runner;
        local_run.state = LocalRunState::Running;
        local_run.updated_at = now;
        local_run.meta_json = serde_json::to_string(&worker).ok();
        self.db.upsert_local_run(&local_run)?;
        self.state.workers.insert(task.id.clone(), worker.clone());
        self.sync_resource_ledger(now);
        self.save_at(now)?;
        match self.note_worker(&task.id, &thread, &worker, agent).await {
            Ok(()) => {
                worker.noted = true;
                self.state.workers.insert(task.id.clone(), worker);
                let _ = self.save_at(now);
            }
            // Noted on a later reconcile.
            Err(err) => tracing::warn!("noting worker thread {thread} on {}: {err:#}", task.id),
        }
        Ok(())
    }

    /// [`host_folder`] on this device, paired with the matched repo key when any.
    pub(super) fn host_project(&self, board: &Board) -> Result<(Option<String>, PathBuf)> {
        let (repo, project) =
            project_for(&self.ctx.config, board).with_context(|| no_project(board))?;
        if folder_gone(&project) {
            anyhow::bail!(
                "the project folder {} of board {} is missing on this device: restore it, point \
                 config.toml at where it is now, or switch the board's autopilot off (board board \
                 update {} --autopilot false)",
                project.display(),
                board.slug,
                board.slug
            );
        }
        Ok((repo, project))
    }

    /// [`host_folder`] on this device.
    fn host_folder(&self, board: &Board) -> Result<PathBuf> {
        host_folder(&self.ctx.config, board)
    }

    /// Whether `slug` is this device's feedback board, which gets triage judges.
    fn triages(&self, slug: &BoardSlug) -> bool {
        *slug == self.ctx.config.feedback_board()
    }

    /// Handles one event: queues its wake-ups and advances `last_seq`. Returns true when the
    /// boards should be reconciled. On an error nothing is queued and `last_seq` stays.
    pub async fn on_event(&mut self, event: &Event, now: Timestamp) -> Result<bool> {
        if event.event_type == EventType::AgentStateChanged {
            self.note_state(event, now);
            if event.payload["to"] == "paused"
                && let Ok(agent) = event.actor.parse::<AgentId>()
            {
                let at = self.state.usage_stops.entry(agent).or_insert(event.at);
                *at = (*at).max(event.at);
            }
            // Back at work: a notice that it was away no longer holds.
            if event.payload["to"] == "running"
                && let Ok(agent) = event.actor.parse::<AgentId>()
            {
                for queue in self.state.wakes.values_mut() {
                    queue.wakes.retain(
                        |w| !matches!(&w.about, WakeAbout::Idle { away, .. } if *away == agent),
                    );
                }
                self.state.wakes.retain(|_, queue| !queue.wakes.is_empty());
                self.state.idle_notices.retain(|_, n| n.away != agent);
                self.state
                    .owed
                    .retain(|o| !matches!(o, Owed::Release { away, .. } if *away == agent));
            }
        }
        let cleared_merge_backoff = self.merge_backoffs.on_event(event);
        let cleared_review_wait = self.review_waits.on_event(event);
        let cleared_llm_spawn_backoff = self.llm_spawn_backoffs.on_event(event);
        let (mut reconcile, wakes) = self.examine(event).await?;
        reconcile |= cleared_merge_backoff;
        reconcile |= cleared_review_wait;
        reconcile |= cleared_llm_spawn_backoff;
        reconcile |= self.note_for_dispatch(event, now);
        for (board, agent, about) in wakes {
            let wake = Wake {
                board,
                about,
                queued_at: now,
                forwarded: false,
                resent: false,
            };
            self.queue(agent, wake);
        }
        self.state.last_seq = Some(self.state.last_seq.unwrap_or(0).max(event.seq));
        Ok(reconcile)
    }

    /// What the dispatcher of the event's board, when this device runs it now, takes from it:
    /// an answer to an asker that is no T3 thread, which no daemon can wake, and the
    /// server's idle notices, whose repeat for an owner still away releases the task.
    /// Returns true when a step is owed.
    fn note_for_dispatch(&mut self, event: &Event, now: Timestamp) -> bool {
        // In pull mode, server-scheduled Housekeeping & Goal runs replace local owed steps
        // and idle_notices tracking so state.owed/idle_notices never leak on home devices.
        if self.dispatch_claim_supported == Some(true)
            || self.ctx.config.dispatch_mode == DispatchMode::Pull
        {
            return false;
        }
        let Some(slug) = event.board.as_ref().filter(|s| self.dispatching(s)) else {
            return false;
        };
        let dispatcher = self.dispatch_agent();
        match event.event_type {
            EventType::AskAnswered => {
                let asker = event.payload["agent"]
                    .as_str()
                    .and_then(|a| a.parse::<AgentId>().ok());
                let (Some(ask), Some(asker)) = (event.payload["id"].as_i64(), asker) else {
                    return false;
                };
                if asker.source() == self.runner_source() || asker.source() == AgentSource::T3 {
                    return false;
                }
                let check_gone = asker.source() != AgentSource::Dispatch;
                self.owe(Owed::Answer {
                    board: slug.clone(),
                    ask,
                    asker,
                    check_gone,
                    noted: false,
                    at: now,
                })
            }
            EventType::TaskUpdated if event.payload.get("idle_doing").is_some() => {
                let away = event.payload["away"]
                    .as_str()
                    .and_then(|a| a.parse::<AgentId>().ok());
                let since =
                    serde_json::from_value::<Timestamp>(event.payload["idle_doing"].clone());
                let (Some(task), Some(away), Ok(since)) = (&event.task, away, since) else {
                    return false;
                };
                if away == dispatcher {
                    return false;
                }
                let repeat = self
                    .state
                    .idle_notices
                    .get(task)
                    .is_some_and(|n| n.away == away && n.since == since);
                let late_first = event.at - since >= DOING_IDLE_TWICE;
                if !repeat && !late_first {
                    let notice = IdleNotice {
                        away,
                        since,
                        noticed: event.at,
                    };
                    self.state.idle_notices.insert(task.clone(), notice);
                    return false;
                }
                self.state.idle_notices.remove(task);
                self.owe(Owed::Release {
                    board: slug.clone(),
                    task: task.clone(),
                    away,
                    since,
                    noticed: event.at,
                    noted: false,
                    at: now,
                })
            }
            // A finished task may complete the goal: its review is due once the board rests.
            EventType::TaskDone => {
                self.state.goal_due.insert(slug.clone(), event.task.clone());
                false
            }
            _ => false,
        }
    }

    /// Adds `step` unless the same one is owed already; true when added.
    fn owe(&mut self, step: Owed) -> bool {
        if self.state.owed.iter().any(|o| o.same(&step)) {
            return false;
        }
        self.state.owed.push(step);
        true
    }

    /// Scans `doing` tasks on a dispatched board for away owners whose repeat (or late first)
    /// `idle_doing` notice was never observed over SSE — for example, notices sent before the
    /// dispatcher ran, a late first notice at twice `doing_idle_secs` with no separate repeat,
    /// or an owner on another device — and owes [`Owed::Release`] for each qualifying task.
    /// Returns true when at least one new release was owed.
    async fn scan_away_doing(
        &mut self,
        slug: &BoardSlug,
        dispatcher: &AgentId,
        doing: &[Task],
        now: Timestamp,
    ) -> bool {
        let mut owed_any = false;
        for task in doing {
            if task.status != TaskStatus::Doing {
                continue;
            }
            let Some(owner) = task.owner.as_ref() else {
                continue;
            };
            if owner == dispatcher {
                continue;
            }
            if self
                .state
                .owed
                .iter()
                .any(|o| matches!(o, Owed::Release { task: t, .. } if t == &task.id))
            {
                continue;
            }
            let detail = match self.client().task(&task.id).await {
                Ok(detail) => detail,
                Err(_) => continue,
            };
            let Some(spell) = idle_spell_from_history(&detail.history, owner) else {
                continue;
            };
            let info = match self.client().agent_info(owner).await {
                Ok(info) => Some(info),
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => None,
                Err(err) => {
                    tracing::warn!("scanning away owner {owner} of {}: {err:#}", task.id);
                    continue;
                }
            };
            let late_first = spell.noticed - spell.since >= DOING_IDLE_TWICE;
            let qualifies = match &info {
                Some(info) => {
                    let waits_for_reset =
                        matches!(info.state, AgentState::Paused | AgentState::Ended)
                            && info.paused_until.is_some_and(|p| p > now);
                    let back = info.state != AgentState::Ended && info.last_seen > spell.noticed;
                    if waits_for_reset || back {
                        false
                    } else {
                        let local_thread_gone = if info.device == self.ctx.device
                            && owner.source() == self.runner_source()
                        {
                            let handle = RunHandle::new(self.runner.kind(), owner.native())
                                .with_agent_id(owner.clone());
                            matches!(
                                tokio::time::timeout(
                                    self.timing.thread_check,
                                    self.runner.status(&handle),
                                )
                                .await,
                                Ok(Ok(status)) if !status.runnable
                            )
                        } else {
                            false
                        };
                        if local_thread_gone {
                            true
                        } else if info.last_seen > spell.noticed {
                            // `detail.history` omits `agent_state_changed`, so even an ended owner
                            // seen after `spell.noticed` (e.g. resumed and later ended) must not
                            // be released from a potentially stale history spell unless its local
                            // thread is gone.
                            false
                        } else {
                            spell.count >= 2 || late_first
                        }
                    }
                }
                None => spell.count >= 2 || late_first,
            };
            if !qualifies {
                continue;
            }
            self.state.idle_notices.remove(&task.id);
            if self.owe(Owed::Release {
                board: slug.clone(),
                task: task.id.clone(),
                away: owner.clone(),
                since: spell.since,
                noticed: spell.noticed,
                noted: false,
                at: now,
            }) {
                owed_any = true;
            }
        }
        owed_any
    }

    /// Does the steps owed on `slug` as its dispatch agent `agent`. A step that waits, or
    /// fails on a transient error, is tried again on the next reconcile, until [`WAKE_TTL`]
    /// after it was owed. Each step stays in the state until it is done, so a reconcile cut
    /// short loses none. Returns the tasks opened during this pass.
    async fn pay_owed(
        &mut self,
        slug: &BoardSlug,
        agent: &AgentId,
        now: Timestamp,
    ) -> BTreeSet<TaskId> {
        let mut opened = BTreeSet::new();
        let steps: Vec<Owed> = (self.state.owed.iter())
            .filter(|s| s.board() == slug)
            .cloned()
            .collect();
        for mut step in steps {
            let keep = if now - step.at() >= WAKE_TTL {
                tracing::warn!("dispatcher of {slug}: {step:?} was not done in time; dropped");
                self.record_drop(
                    "owed_expired",
                    Some(slug),
                    match &step {
                        Owed::Release { task, .. } => Some(task),
                        Owed::Answer { .. } => None,
                    },
                    Some(agent),
                    &format!(
                        "owed step exceeded WAKE_TTL ({}s) before completion",
                        WAKE_TTL.num_seconds()
                    ),
                    serde_json::to_value(&step).unwrap_or_default(),
                    now,
                );
                false
            } else {
                match self.pay(&mut step, agent, now).await {
                    Ok(Paid::Opened(task)) => {
                        opened.insert(task.clone());
                        // It starts again at once, with a new worker.
                        if let Some(worker) = self.state.workers.remove(&task) {
                            if let Some(run_id) = worker.run_id.as_deref() {
                                let _ = self.db.update_local_run_state(
                                    run_id,
                                    LocalRunState::Finished,
                                    None,
                                    now,
                                );
                            } else {
                                let _ = self.db.finish_local_runs_for_task(
                                    "worker",
                                    &task,
                                    LocalRunState::Finished,
                                    now,
                                );
                            }
                            self.sync_resource_ledger(now);
                            if let Some(wt_id) = worker.worktree_id.as_deref() {
                                self.cleanup_worktree(wt_id, now).await;
                            }
                        }
                        self.state.worker_skips.remove(&task);
                        self.state.worker_retry.remove(&task);
                        self.state.idle_notices.remove(&task);
                        false
                    }
                    Ok(Paid::Nothing) => false,
                    Ok(Paid::Later) => true,
                    Err(err) if is_transient(&err) => {
                        tracing::warn!("dispatcher of {slug}: {err:#}; trying again");
                        true
                    }
                    Err(err) => {
                        tracing::warn!("dispatcher of {slug}: {err:#}; dropped");
                        self.record_drop(
                            "owed_failed",
                            Some(slug),
                            match &step {
                                Owed::Release { task, .. } => Some(task),
                                Owed::Answer { .. } => None,
                            },
                            Some(agent),
                            &format!("non-transient error paying owed step: {err:#}"),
                            serde_json::to_value(&step).unwrap_or_default(),
                            now,
                        );
                        false
                    }
                }
            };
            let Some(at) = self.state.owed.iter().position(|o| o.same(&step)) else {
                continue;
            };
            if keep {
                self.state.owed[at] = step;
            } else {
                self.state.owed.remove(at);
            }
            let _ = self.save_at(now);
        }
        opened
    }

    /// Persists an updated `step` in `self.state.owed` and `daemon.db` before taking the next
    /// action so a crash between noting and releasing never duplicates the board note.
    fn persist_owed_step(&mut self, step: &Owed, now: Timestamp) {
        if let Some(at) = self.state.owed.iter().position(|o| o.same(step)) {
            self.state.owed[at] = step.clone();
            if let Err(err) = self.save_at(now) {
                tracing::warn!("persisting owed step before release: {err:#}");
            }
        }
    }

    /// Does one owed step, as of `now`.
    async fn pay(&mut self, step: &mut Owed, agent: &AgentId, now: Timestamp) -> Result<Paid> {
        let client = self.client().with_agent(Some(agent.clone()));
        match step {
            Owed::Answer {
                ask,
                asker,
                check_gone,
                noted,
                ..
            } => {
                let ask_id = *ask;
                let asker_id = asker.clone();
                let check_gone_flag = *check_gone;
                let already_noted = *noted;
                let found = client
                    .ask(ask_id)
                    .await
                    .with_context(|| format!("reading ask #{ask_id}"))?;
                let (Some(task), Some(answer)) = (found.task, found.answer) else {
                    return Ok(Paid::Nothing);
                };
                // An asker still present may read the answer, or end without it.
                if check_gone_flag && !gone(&client, &asker_id).await? {
                    return Ok(Paid::Later);
                }
                let current = match client.task(&task).await {
                    Ok(detail) => detail.task,
                    Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                        return Ok(Paid::Nothing);
                    }
                    Err(err) => return Err(err),
                };
                if asker_id.source() == AgentSource::Dispatch {
                    let mut was_noted = already_noted;
                    let result = judges::handle_dispatch_ask_answer(
                        &client,
                        agent,
                        &current,
                        ask_id,
                        &asker_id,
                        &answer,
                        &mut was_noted,
                    )
                    .await;
                    if was_noted && !already_noted {
                        *noted = true;
                        self.persist_owed_step(step, now);
                    }
                    return match result? {
                        judges::DispatchAskOutcome::Reopened => Ok(Paid::Opened(task)),
                        judges::DispatchAskOutcome::ResumedReview
                        | judges::DispatchAskOutcome::Noop(_) => Ok(Paid::Nothing),
                    };
                }
                if !already_noted {
                    let text = clip_note(format!(
                        "decision: the user answered ask #{ask_id} from {asker_id}, which cannot be woken: {answer}"
                    ));
                    add_note(&client, &task, agent, NoteKind::Decision, text).await?;
                    *noted = true;
                    self.persist_owed_step(step, now);
                }
                let held = matches!(
                    current.status,
                    TaskStatus::Doing | TaskStatus::Blocked | TaskStatus::Review
                ) && current.owner.as_ref().is_none_or(|o| o == &asker_id);
                if !held {
                    return Ok(Paid::Nothing);
                }
                reopen_held_task(&client, agent, &current)
                    .await
                    .with_context(|| format!("setting {task} open after ask #{ask_id}"))?;
                tracing::info!("{task}: ask #{ask_id} answered for gone {asker_id}; set open");
                Ok(Paid::Opened(task))
            }
            Owed::Release {
                task,
                away,
                since,
                noticed,
                noted,
                ..
            } => {
                let task_id = task.clone();
                let away_id = away.clone();
                let since_at = *since;
                let noticed_at = *noticed;
                let already_noted = *noted;
                // The repeat notice also wakes the owner: it may still come back.
                if now - noticed_at < RELEASE_GRACE {
                    return Ok(Paid::Later);
                }
                let current = match client.task(&task_id).await {
                    Ok(detail) => detail.task,
                    Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                        return Ok(Paid::Nothing);
                    }
                    Err(err) => return Err(err),
                };
                if current.status != TaskStatus::Doing || current.owner.as_ref() != Some(&away_id) {
                    return Ok(Paid::Nothing);
                }
                let info = match client.agent_info(&away_id).await {
                    Ok(info) => Some(info),
                    Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => None,
                    Err(err) => return Err(err),
                };
                if let Some(info) = &info {
                    let back = info.state != AgentState::Ended && info.last_seen > noticed_at;
                    let waits_for_reset =
                        matches!(info.state, AgentState::Paused | AgentState::Ended)
                            && info.paused_until.is_some_and(|p| p > now);
                    if back || waits_for_reset {
                        return Ok(Paid::Nothing);
                    }
                }
                if !already_noted {
                    let thread = info
                        .as_ref()
                        .and_then(|i| i.native_ids.get("t3_link"))
                        .map_or_else(|| away_id.to_string(), |link| format!("{away_id} ({link})"));
                    let branch = self
                        .state
                        .workers
                        .get(&task_id)
                        .and_then(|w| w.branch.clone())
                        .or_else(|| info.and_then(|i| i.branch))
                        .map_or_else(|| "none recorded".to_owned(), |b| format!("`{b}`"));
                    let text = clip_note(format!(
                        "note: the owner was away from this task since {} and still was after the \
                         server's repeat notice, or its thread is gone, so the dispatcher set it \
                         open for a new worker. Old thread: {thread}. Branch: {branch}.",
                        since_at.format("%Y-%m-%d %H:%MZ")
                    ));
                    add_note(&client, &task_id, agent, NoteKind::Note, text).await?;
                    *noted = true;
                    self.persist_owed_step(step, now);
                }
                client
                    .task_action(&task_id, "release")
                    .await
                    .with_context(|| format!("setting {task_id} open from away {away_id}"))?;
                tracing::info!("{task_id}: {away_id} away since {since_at}; set open");
                Ok(Paid::Opened(task_id))
            }
        }
    }

    /// Adds `wake` to `agent`'s queue unless the same one is already there.
    fn queue(&mut self, agent: AgentId, wake: Wake) {
        add_wake(&mut self.state.wakes.entry(agent).or_default().wakes, wake);
    }

    /// Keeps what is known of an agent with queued wake-ups current: its held wake-ups are
    /// sent on the next flush once it stops running.
    fn note_state(&mut self, event: &Event, now: Timestamp) {
        let Ok(agent) = event.actor.parse::<AgentId>() else {
            return;
        };
        let Some(seen) = self.agents.get_mut(&agent) else {
            return;
        };
        let Ok(state) = serde_json::from_value::<AgentState>(event.payload["to"].clone()) else {
            tracing::warn!("event #{}: agent state change without a state", event.seq);
            return;
        };
        seen.agent.state = state;
        seen.agent.paused_until = serde_json::from_value(event.payload["paused_until"].clone())
            .ok()
            .flatten();
        seen.since = now;
        seen.idled |= state != AgentState::Running;
        // Asked again when it next matters: a thread may be archived or resumed meanwhile.
        seen.thread = ThreadCheck::NotAsked;
        seen.idle_busy_until = None;
    }

    /// What one event calls for: a reconcile, and wake-ups by agent, each with the board
    /// it is about. Only reads.
    async fn examine(&self, event: &Event) -> Result<(bool, Vec<(BoardSlug, AgentId, WakeAbout)>)> {
        let mut wakes = Vec::new();
        // A steer or cancel targeting this device enqueues a sticky wake; reconcile promptly
        // even for runs without a board.
        let targets_this_device = matches!(
            event.event_type,
            EventType::RunSteered | EventType::RunCancelled
        ) && event.payload.get("device").and_then(|v| v.as_str())
            == Some(self.ctx.device.as_str());
        let Some(slug) = &event.board else {
            return Ok((targets_this_device, wakes));
        };
        // On a daemon-dispatch board run here, a task that changes may become ready or free
        // a slot: the reconcile it asks for starts the next worker. In pull mode, any active
        // board's task/run/ask lifecycle change triggers a pull check.
        let pull_active = self.dispatch_claim_supported == Some(true)
            || self.ctx.config.dispatch_mode == DispatchMode::Pull;
        let mut reconcile = targets_this_device
            || ((self.dispatches_here(slug) || pull_active)
                && matches!(
                    event.event_type,
                    EventType::TaskCreated
                        | EventType::TaskReleased
                        | EventType::TaskUpdated
                        | EventType::TaskDone
                ));
        if pull_active
            && (matches!(
                event.event_type,
                EventType::RunOrphaned | EventType::AskAnswered
            ) || (event.event_type == EventType::RunCreated
                && event.payload.get("state").and_then(|v| v.as_str()) == Some("queued")))
        {
            reconcile = true;
        }
        match event.event_type {
            EventType::BoardCreated => return Ok((true, wakes)),
            // A board paused and active again: the reconcile this asks for dispatches it.
            EventType::BoardUpdated if event.payload["status"] == "active" => {
                return Ok((true, wakes));
            }
            // This device's own lease claims and releases change nothing a reconcile would act
            // on, nor does a judge's registration, which each lease holder makes per judge.
            EventType::BoardUpdated => {
                let judges = event.payload["fields"] == serde_json::json!(["judges"]);
                return Ok((!judges && !self.is_own_claim(event), wakes));
            }
            // The server's notice that a task's owner has been away from it: the owner.
            // Whether it is on this device and still away is checked when the wake-up is sent.
            EventType::TaskUpdated if event.payload.get("idle_doing").is_some() => {
                let away = event.payload["away"]
                    .as_str()
                    .and_then(|a| a.parse::<AgentId>().ok());
                let since =
                    serde_json::from_value::<Timestamp>(event.payload["idle_doing"].clone());
                if let (Some(task), Some(away), Ok(since)) = (&event.task, away, since)
                    && let Some(to) = self.wakeable(Some(&away), event).cloned()
                {
                    let about = WakeAbout::Idle {
                        task: task.clone(),
                        away,
                        since,
                        noticed: event.at,
                    };
                    wakes.push((slug.clone(), to, about));
                }
            }
            // The tasks waiting on it may be on other boards: a reconcile dispatches those that
            // became ready, and the owner of a blocked task hears of its last blocker.
            EventType::TaskDone => {
                if let Some(done) = &event.task {
                    let detail = self.client().task(done).await?;
                    // Each waiting task is read apart from the others, so a few are read at
                    // once; the results are taken in the order the finished task lists them.
                    let checks = futures_util::stream::iter(detail.blocks.iter().map(
                        |blocked| async move {
                            // One gone since (its board deleted) does not hold up the rest.
                            let t = match self.client().task(blocked).await {
                                Ok(d) => d.task,
                                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                                    return Ok(None);
                                }
                                Err(err) => return Err(err),
                            };
                            let unblocked = !(t.ready && t.auto)
                                && self.wakeable(t.owner.as_ref(), event).is_some()
                                && t.status == TaskStatus::Blocked
                                && self.blockers_done(&t).await?;
                            Ok(Some((t, unblocked)))
                        },
                    ))
                    .buffered(BLOCKED_CHECKS_AT_ONCE);
                    tokio::pin!(checks);
                    while let Some(check) = checks.next().await {
                        let Some((t, unblocked)) = check? else {
                            continue;
                        };
                        if t.ready && t.auto {
                            reconcile |= self.dispatches_here(&t.board);
                        } else if unblocked && let Some(owner) = t.owner {
                            let about = WakeAbout::Unblocked {
                                done: done.clone(),
                                task: t.id,
                            };
                            wakes.push((t.board, owner, about));
                        }
                    }
                }
            }
            // The asker. One that turns out not to be wakeable when the flush comes is handled
            // there.
            EventType::AskAnswered => {
                let asker = event.payload["agent"]
                    .as_str()
                    .and_then(|a| a.parse::<AgentId>().ok());
                let (Some(ask), Some(asker)) = (event.payload["id"].as_i64(), asker) else {
                    tracing::warn!(
                        "event #{}: ask_answered without its ask or asker",
                        event.seq
                    );
                    return Ok((false, wakes));
                };
                // In pull mode, live managed runs receive their answered-ask wake via the
                // server's `sticky_wakes` queue (`poll_sticky_wakes`), avoiding double delivery.
                let handled_by_sticky = pull_active
                    && self
                        .state
                        .managed_runs
                        .values()
                        .any(|r| r.agent_id.as_ref() == Some(&asker));
                if !handled_by_sticky && let Some(to) = self.wakeable(Some(&asker), event).cloned()
                {
                    wakes.push((slug.clone(), to, WakeAbout::Answered { ask, asker }));
                }
            }
            _ => {}
        }
        Ok((reconcile, wakes))
    }

    /// Whether every blocker of `task` is done, wherever it is.
    async fn blockers_done(&self, task: &Task) -> Result<bool> {
        for blocker in &task.blockers {
            if self.client().task(blocker).await?.task.status != TaskStatus::Done {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether `away` showed activity after the server's notice at `noticed`: marked running
    /// and seen since. A return to running from another state drops the notice when its
    /// event arrives; this finds an owner left marked running that came back, which no
    /// state change announces. Being seen in another state does not count: a session that
    /// T3 closes marks its agent ended, which records it as seen.
    async fn back_since(&self, away: &AgentId, noticed: Timestamp) -> Result<bool> {
        match self.client().agent_info(away).await {
            Ok(info) => Ok(info.state == AgentState::Running && info.last_seen > noticed),
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// A lease claim this device made (the server records the claiming device).
    fn is_own_claim(&self, event: &Event) -> bool {
        event.payload["device"].as_str() == Some(self.ctx.device.as_str())
            && event.payload["fields"] == serde_json::json!(["coordinator"])
    }

    /// `agent` when it is owned by this daemon's runner source and did not cause `event`.
    /// Whether it is on this device and awake is checked when the wake-up is sent.
    fn wakeable<'a>(&self, agent: Option<&'a AgentId>, event: &Event) -> Option<&'a AgentId> {
        let source = self.runner_source();
        agent.filter(|a| a.as_str() != event.actor && a.source() == source)
    }

    /// Sends the queued wake-ups, one message per agent, after checking each again. The
    /// checks read only, a few agents at a time, within [`Timing::flush_budget`]; then the
    /// queues change at once, without waiting on anything, so a flush cut short at any
    /// point loses no wake-up. The sends run side by side, each given
    /// [`WAKE_SEND_TIMEOUT`], so a hanging T3 holds the daemon up once per flush, not once
    /// per agent; a flush cut short while sending counts those lines as maybe delivered.
    /// Each message ends with a [`wake_marker`]; one whose
    /// send timed out is looked for in the thread [`SEND_CHECK_AFTER`] later, while the
    /// agent's next lines wait, and its wake-ups are queued again, once, when the thread
    /// never got it (see [`UnsureWakes`]).
    pub async fn flush(&mut self, now: Timestamp) {
        if (self.dispatch_claim_supported == Some(true)
            || self.ctx.config.dispatch_mode == DispatchMode::Pull
            || !self.state.managed_runs.is_empty())
            && !self.wakes_unsupported
            && let Err(err) = self.poll_sticky_wakes(now).await
        {
            tracing::warn!("flush: polling device sticky wakes: {err:#}");
        }
        if !self.state.managed_runs.is_empty()
            && let Err(err) = self.flush_run_events(now).await
        {
            tracing::warn!("flush: pushing live run events: {err:#}");
        }
        if let Err(err) = self.flush_telemetry_outbox(now).await {
            tracing::warn!("flush: processing telemetry outbox: {err:#}");
        }
        let deadline = tokio::time::Instant::now() + self.timing.flush_budget;
        match self.usage.as_ref().map(|usage| usage.full_until(now)) {
            Some(Some(until)) => self.state.usage_full_until = Some(until),
            // The hold ended before its reset (the reading fell below 100 percent or went
            // stale): what it kept lives from now on, not from a reset days ahead.
            Some(None) if self.state.usage_full_until.is_some_and(|until| until > now) => {
                self.state.usage_full_until = Some(now);
            }
            Some(None) | None => {}
        }
        let mut expired_unsure = Vec::new();
        for (agent, unsure) in &mut self.state.unsure_wakes {
            unsure.messages.retain(|m| {
                let live = now - m.since < WAKE_TTL;
                if !live {
                    tracing::warn!(
                        "waking {agent}: a timed-out message was not found in time; what it \
                         carried is dropped"
                    );
                    expired_unsure.push((agent.clone(), m.clone()));
                }
                live
            });
        }
        for (agent, msg) in expired_unsure {
            self.record_drop(
                "unsure_wake_expired",
                msg.wakes.first().map(|w| &w.board),
                None,
                Some(&agent),
                &format!(
                    "timed-out wake-up message {} was not confirmed within WAKE_TTL ({}s)",
                    msg.digest,
                    WAKE_TTL.num_seconds()
                ),
                serde_json::to_value(&msg).unwrap_or_default(),
                now,
            );
        }
        self.state
            .unsure_wakes
            .retain(|_, unsure| !unsure.messages.is_empty());
        let agents: BTreeSet<&AgentId> = self
            .state
            .wakes
            .keys()
            .chain(self.state.unsure_wakes.iter().filter_map(|(agent, u)| {
                u.messages
                    .iter()
                    .any(|m| {
                        m.wakes
                            .iter()
                            .any(|w| !matches!(w.about, WakeAbout::Sticky { .. }))
                    })
                    .then_some(agent)
            }))
            .collect();
        let queues: Vec<(AgentId, WakeQueue, Option<UnsureWakes>)> = agents
            .into_iter()
            .map(|agent| {
                let non_sticky_unsure = self.state.unsure_wakes.get(agent).and_then(|u| {
                    let messages: Vec<UnsureMessage> = u
                        .messages
                        .iter()
                        .filter(|m| {
                            m.wakes
                                .iter()
                                .any(|w| !matches!(w.about, WakeAbout::Sticky { .. }))
                        })
                        .cloned()
                        .collect();
                    (!messages.is_empty()).then_some(UnsureWakes {
                        messages,
                        read_after: u.read_after,
                        failed_delay_secs: u.failed_delay_secs,
                    })
                });
                (
                    agent.clone(),
                    self.state.wakes.get(agent).cloned().unwrap_or_default(),
                    non_sticky_unsure,
                )
            })
            .collect();
        // Thread reads start `t3` processes: as few at once as the reconcile's checks.
        let reads = tokio::sync::Semaphore::new(THREAD_CHECKS_AT_ONCE);
        let this = &*self;
        type Read = (Checked, Option<Result<Sorted>>, Option<WakeQueue>);
        let reads = &reads;
        let checked: Vec<Read> =
            futures_util::stream::iter(queues.iter().map(|(agent, queue, unsure)| async move {
                tokio::time::timeout_at(deadline, async {
                    let look = this
                        .look_for_unsure(agent, unsure.as_ref(), now, reads)
                        .await;
                    // The wake-ups of messages the thread never got go with the queue.
                    let requeued = match &look.read {
                        Some(Ok(sorted)) if !sorted.missing.is_empty() => {
                            Some(requeue_unsure(queue.clone(), &sorted.missing))
                        }
                        _ => None,
                    };
                    let queue = requeued.as_ref().unwrap_or(queue);
                    if look.hold || queue.wakes.is_empty() {
                        return (Checked::Waiting, look.read, requeued);
                    }
                    let checked = this.check(agent, queue, now, reads).await;
                    (checked, look.read, requeued)
                })
                .await
                .unwrap_or((Checked::NotReached, None, None))
            }))
            .buffered(FLUSH_CHECKS_AT_ONCE)
            .collect()
            .await;
        let late = checked
            .iter()
            .filter(|(c, ..)| matches!(c, Checked::NotReached))
            .count();
        if late > 0 {
            tracing::warn!(
                "flush: {late} agents' wake-ups not checked within {}s; checked on the next flush",
                self.timing.flush_budget.as_secs()
            );
        }
        let mut outgoing = Vec::new();
        for ((agent, queue, _), (checked, read, requeued)) in queues.into_iter().zip(checked) {
            self.settle_look(&agent, read, now);
            let queue = requeued.unwrap_or(queue);
            if let Some(mut out) = self.apply(agent, queue, checked, now) {
                self.mark_unsure(&mut out, now);
                outgoing.push(out);
            }
        }
        // Write-before-act: durably commit `unsure_sends` and updated `sticky_wakes` to
        // `daemon.db` (`PRAGMA synchronous=FULL`) BEFORE sending any wake-up message, so a
        // hard kill mid-send never loses the unsure digest or resends twice (agent-board-539).
        if !outgoing.is_empty()
            && let Err(err) = self.save_at(now)
        {
            tracing::warn!(
                "flush: persisting unsure wake intent before send failed ({err:#}); deferring {} sends",
                outgoing.len()
            );
            for out in outgoing {
                let _ = self.settle_unsure(
                    &out.agent,
                    &out,
                    &Sent::Failed(anyhow::anyhow!("save failed")),
                );
                self.requeue(
                    out.agent,
                    WakeQueue {
                        wakes: out.wakes,
                        backoff: out.backoff,
                    },
                );
            }
            return;
        }
        self.sending = outgoing.iter().map(|out| out.agent.clone()).collect();
        let sends = outgoing
            .iter()
            .map(|out| self.send_wake(&out.agent, &out.message, out.mode));
        let results = futures_util::future::join_all(sends).await;
        self.sending.clear();
        for (out, sent) in outgoing.into_iter().zip(results) {
            self.settle(out, sent, now);
        }
        let wakes = &self.state.wakes;
        self.agents.retain(|agent, _| wakes.contains_key(agent));
        if let Err(err) = self.save_at(now) {
            tracing::warn!("flush: persisting state after wake sends failed: {err:#}");
        }
    }

    /// Reads what a flush needs to decide on `agent`'s `queue`: the agent, its runner status
    /// when needed (ended threads or `WakeAbout::Idle` busy check), and each line when it can
    /// be sent now. Changes nothing.
    async fn check(
        &self,
        agent: &AgentId,
        queue: &WakeQueue,
        now: Timestamp,
        reads: &tokio::sync::Semaphore,
    ) -> Checked {
        let backing_off = queue.backoff.as_ref().filter(|b| now < b.retry_at);
        let (mut seen, mut read) = match self.agents.get(agent) {
            Some(seen) => (seen.clone(), false),
            // During an outage a lookup backing off costs no request.
            None if backing_off.is_some_and(|b| b.lookup) => return Checked::Waiting,
            None => match self.client().agent_info(agent).await {
                Ok(info) => {
                    let seen = Seen {
                        agent: info,
                        since: now,
                        idled: false,
                        thread: ThreadCheck::NotAsked,
                        idle_busy_until: None,
                    };
                    (seen, true)
                }
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                    return Checked::Unknown;
                }
                Err(err) => return Checked::LookupFailed(err),
            },
        };
        let mut live_status: Option<RunnerStatus> = None;
        let registered = seen.agent.cwd.clone();
        // Asked once per state change, also while a failed send backs off. A thread that goes
        // later is found by the next send that fails, which asks the runner again.
        if seen.needs_thread_check(&self.ctx.device, self.runner_source()) {
            // Its folder: the worktree T3 names, else the one its session registered from.
            let _permit = reads.acquire().await;
            seen.thread = match self
                .thread_status(agent.native(), registered.as_deref().map(Path::new))
                .await
            {
                Ok(status) => {
                    let runnable = status.runnable;
                    live_status = Some(status);
                    if runnable {
                        ThreadCheck::Live
                    } else {
                        ThreadCheck::Gone
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        "waking {agent}: checking its ended thread: {err:#}; trying the send"
                    );
                    ThreadCheck::NoAnswer
                }
            };
            read = true;
        }
        let mut record_busy_idle_hold = false;
        let mut idle_deferred = false;
        let waited_until = self.waited_until(&seen);
        let reach = self.reach(&seen, now);
        // Before sending a `WakeAbout::Idle` notice, ask the runner (`T3::thread_info`) whether
        // the thread currently has an active or queued run (`busy`). Deferring when busy avoids
        // queueing stale away notices behind live turns.
        if backing_off.is_none()
            && matches!(reach, Reach::Now)
            && queue
                .wakes
                .iter()
                .any(|w| is_fresh(w, waited_until, now) && w.about.is_idle_for(agent))
        {
            if seen.idle_busy_until.is_some_and(|until| now < until) {
                idle_deferred = true;
            } else {
                let status = match live_status {
                    Some(status) => Ok(status),
                    None => {
                        let _permit = reads.acquire().await;
                        self.thread_status(agent.native(), registered.as_deref().map(Path::new))
                            .await
                    }
                };
                match status {
                    Ok(status) if !status.runnable => {
                        seen.thread = ThreadCheck::Gone;
                        seen.idle_busy_until = None;
                        read = true;
                    }
                    Ok(status) if status.busy => {
                        record_busy_idle_hold = seen.idle_busy_until.is_none();
                        seen.idle_busy_until = Some(now + WAKE_HOLD_MAX);
                        idle_deferred = true;
                        read = true;
                    }
                    Ok(_) => {
                        if seen.idle_busy_until.take().is_some() {
                            read = true;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            "waking {agent}: checking thread status before idle wake: {err:#}; \
                             trying the send"
                        );
                    }
                }
            }
        }
        let reach = self.reach(&seen, now);
        let due = backing_off.is_none()
            && (matches!(reach, Reach::Now)
                || (matches!(reach, Reach::Held)
                    && queue
                        .wakes
                        .iter()
                        .any(|w| is_fresh(w, waited_until, now) && w.about.is_urgent())));
        let rechecks = if due {
            let mut lines = Vec::new();
            for wake in queue
                .wakes
                .iter()
                .filter(|w| is_fresh(w, waited_until, now))
            {
                if matches!(reach, Reach::Held) && !wake.about.is_urgent() {
                    lines.push(WakeCheck::Deferred);
                } else if idle_deferred && wake.about.is_idle_for(agent) {
                    lines.push(WakeCheck::DeferredBusy);
                } else {
                    lines.push(WakeCheck::Checked(self.recheck(agent, wake, now).await));
                }
            }
            Some(lines)
        } else {
            None
        };
        Checked::Known {
            seen: Box::new(seen),
            read,
            rechecks,
            record_busy_idle_hold,
        }
    }

    /// Settles `queue`, as [`Self::check`] read it, from what that found: drops wake-ups
    /// past their time to live, keeps those for later (held, paused, backing off, not
    /// checked, or a check that failed on a transient error), passes answers the agent
    /// cannot be woken for to the dispatcher, and returns the message to send, if any.
    /// Wake-ups queued for the agent since the check wait for the next flush.
    fn apply(
        &mut self,
        agent: AgentId,
        queue: WakeQueue,
        checked: Checked,
        now: Timestamp,
    ) -> Option<Outgoing> {
        let meanwhile: Vec<Wake> = self
            .state
            .wakes
            .remove(&agent)
            .map(|kept| {
                kept.wakes
                    .into_iter()
                    .filter(|w| !queue.wakes.contains(w))
                    .collect()
            })
            .unwrap_or_default();
        let out = self.decide(&agent, queue, checked, now);
        for wake in meanwhile {
            self.queue(agent.clone(), wake);
        }
        out
    }

    fn decide(
        &mut self,
        agent: &AgentId,
        mut queue: WakeQueue,
        checked: Checked,
        now: Timestamp,
    ) -> Option<Outgoing> {
        let (seen, rechecks, record_busy_idle_hold) = match checked {
            Checked::NotReached | Checked::Waiting => {
                self.requeue(agent.clone(), queue);
                return None;
            }
            // Unknown to the server: its answers and idle notices go to the dispatcher.
            Checked::Unknown => {
                let unheard = self.forward(agent, &mut queue.wakes, true, now);
                if unheard > 0 {
                    tracing::warn!(
                        "waking {agent}: unknown to the server; {unheard} answers to its asks \
                         or idle notices dropped: this device does not dispatch their boards"
                    );
                }
                if !queue.wakes.is_empty() {
                    self.record_drop(
                        "wake_unknown_agent",
                        queue.wakes.first().map(|w| &w.board),
                        None,
                        Some(agent),
                        &format!(
                            "agent unknown to server ({} wake-ups, {unheard} unheard)",
                            queue.wakes.len()
                        ),
                        serde_json::to_value(&queue.wakes).unwrap_or_default(),
                        now,
                    );
                }
                return None;
            }
            Checked::LookupFailed(err) => {
                if is_transient(&err) {
                    let delay = back_off(&mut queue, now, true);
                    tracing::warn!("waking {agent}: {err:#}; trying again in {delay}");
                    self.requeue(agent.clone(), queue);
                } else {
                    tracing::warn!("waking {agent}: {err:#}; dropped");
                    self.record_drop(
                        "wake_lookup_failed",
                        queue.wakes.first().map(|w| &w.board),
                        None,
                        Some(agent),
                        &format!("non-transient agent lookup failure: {err:#}"),
                        serde_json::to_value(&queue.wakes).unwrap_or_default(),
                        now,
                    );
                }
                return None;
            }
            Checked::Known {
                seen,
                read,
                rechecks,
                record_busy_idle_hold,
            } => {
                if read {
                    self.agents.insert(agent.clone(), (*seen).clone());
                }
                (*seen, rechecks, record_busy_idle_hold)
            }
        };
        let waited_until = self.waited_until(&seen);
        let mut expired_wakes = Vec::new();
        queue.wakes.retain(|w| {
            let fresh = is_fresh(w, waited_until, now);
            if !fresh {
                tracing::warn!("waking {agent}: not delivered in time; dropped");
                expired_wakes.push(w.clone());
            }
            fresh
        });
        if !expired_wakes.is_empty() {
            self.record_drop(
                "wake_expired",
                expired_wakes.first().map(|w| &w.board),
                None,
                Some(agent),
                &format!(
                    "{} wake-up(s) exceeded WAKE_TTL ({}s) before delivery",
                    expired_wakes.len(),
                    WAKE_TTL.num_seconds()
                ),
                serde_json::to_value(&expired_wakes).unwrap_or_default(),
                now,
            );
        }
        match self.reach(&seen, now) {
            Reach::Now => {}
            // Non-urgent lines are held while the agent runs; urgent wakes (ask answers)
            // bypass the hold and steer via `SendMode::Auto`.
            Reach::Held => {
                if !queue.wakes.iter().any(|w| w.about.is_urgent()) {
                    self.requeue(agent.clone(), queue);
                    return None;
                }
            }
            // Kept whole for the reset of the full usage window.
            Reach::Full => {
                self.requeue(agent.clone(), queue);
                return None;
            }
            // Kept for the reset.
            Reach::Paused => {
                // Waiting for a usage-limit reset is not being away: its idle notices no
                // longer hold, so none is sent at the reset.
                queue
                    .wakes
                    .retain(|w| !matches!(&w.about, WakeAbout::Idle { away, .. } if away == agent));
                self.requeue(agent.clone(), queue);
                return None;
            }
            // Another device's, or not to be woken by the daemon: its answers go to the
            // dispatcher, and so do its idle notices when no device can wake it; the rest
            // is not ours to send.
            Reach::Never(why) => {
                let idle = matches!(why, Unreachable::NotT3 | Unreachable::ThreadGone);
                let dropped = queue
                    .wakes
                    .iter()
                    .filter(|w| !passed_on(agent, &w.about, idle))
                    .count();
                let unheard = self.forward(agent, &mut queue.wakes, idle, now);
                if !why.expected() {
                    if dropped > 0 {
                        tracing::warn!("waking {agent}: {why}; {dropped} wake-ups dropped");
                    }
                    if unheard > 0 {
                        tracing::warn!(
                            "waking {agent}: {why}; {unheard} answers to its asks or idle \
                             notices dropped: this device does not dispatch their boards"
                        );
                    }
                    if dropped > 0 || unheard > 0 {
                        self.record_drop(
                            "wake_unreachable",
                            queue.wakes.first().map(|w| &w.board),
                            None,
                            Some(agent),
                            &format!("{why} ({dropped} dropped, {unheard} unheard)"),
                            serde_json::to_value(&queue.wakes).unwrap_or_default(),
                            now,
                        );
                    }
                }
                return None;
            }
        }
        // A failed send is backing off.
        let Some(rechecks) = rechecks else {
            self.requeue(agent.clone(), queue);
            return None;
        };
        let backoff = queue.backoff.clone();
        let mut sent = Vec::new();
        let mut lines = Vec::new();
        let mut deferred_busy = Vec::new();
        for (wake, outcome) in std::mem::take(&mut queue.wakes).into_iter().zip(rechecks) {
            match outcome {
                WakeCheck::Checked(Ok(Some(line))) => {
                    if !lines.contains(&line) {
                        lines.push(line);
                    }
                    sent.push(wake);
                }
                WakeCheck::Checked(Ok(None)) => {}
                WakeCheck::Checked(Err(err)) => {
                    tracing::warn!("waking {agent}: {err:#}");
                    if is_transient(&err) {
                        queue.wakes.push(wake);
                    } else {
                        self.record_drop(
                            "wake_recheck_failed",
                            Some(&wake.board),
                            None,
                            Some(agent),
                            &format!("non-transient wake recheck error: {err:#}"),
                            serde_json::to_value(&wake).unwrap_or_default(),
                            now,
                        );
                    }
                }
                WakeCheck::Deferred => {
                    queue.wakes.push(wake);
                }
                WakeCheck::DeferredBusy => {
                    deferred_busy.push(wake.clone());
                    queue.wakes.push(wake);
                }
            }
        }
        if record_busy_idle_hold && !deferred_busy.is_empty() {
            let task_id = deferred_busy.iter().find_map(|w| match &w.about {
                WakeAbout::Idle { task, .. } => Some(task),
                _ => None,
            });
            tracing::info!(
                "waking {agent}: {} reports active run (busy); deferring {} idle wake-up(s) for {}s",
                self.runner.kind(),
                deferred_busy.len(),
                WAKE_HOLD_MAX.num_seconds()
            );
            self.record_drop(
                "wake_idle_deferred_busy",
                deferred_busy.first().map(|w| &w.board),
                task_id,
                Some(agent),
                &format!(
                    "runner {} reports active run (busy); deferring {} idle wake-up(s) for {}s",
                    self.runner.kind(),
                    deferred_busy.len(),
                    WAKE_HOLD_MAX.num_seconds()
                ),
                serde_json::to_value(&deferred_busy).unwrap_or_default(),
                now,
            );
        }
        self.requeue(agent.clone(), queue);
        if lines.is_empty() {
            return None;
        }
        let mode = if sent.iter().any(|w| w.about.is_urgent()) {
            SendMode::Auto
        } else {
            SendMode::Queue
        };
        Some(Outgoing {
            agent: agent.clone(),
            message: format!("{}\n{WAKE_END}", lines.join("\n")),
            mode,
            wakes: sent,
            backoff,
            unsure: None,
        })
    }

    /// Puts `queue` back for a later flush, after wake-ups queued for `agent` meanwhile.
    fn requeue(&mut self, agent: AgentId, queue: WakeQueue) {
        if queue.wakes.is_empty() {
            return;
        }
        let backoff = queue.backoff;
        for wake in queue.wakes {
            self.queue(agent.clone(), wake);
        }
        if let Some(backoff) = backoff
            && let Some(kept) = self.state.wakes.get_mut(&agent)
        {
            kept.backoff = Some(backoff);
        }
    }

    /// Owes the dispatcher the step of each wake-up [`passed_on`] names once no device can
    /// wake `from` (`idle`), once each: an answer or an idle notice no one hears of would
    /// stall its task (see [`Owed`]). Until then the wake-up waits for `from`. Returns how
    /// many are on a board this device does not dispatch; a queue kept for later tries those
    /// again.
    fn forward(&mut self, from: &AgentId, wakes: &mut [Wake], idle: bool, now: Timestamp) -> usize {
        let mut unheard = 0;
        for wake in wakes {
            if !passed_on(from, &wake.about, idle) || wake.forwarded {
                continue;
            }
            if !self.dispatching(&wake.board) {
                unheard += 1;
                continue;
            }
            if idle {
                wake.forwarded = true;
                if let Some(step) = owed_for(from, wake, now) {
                    self.owe(step);
                }
            }
        }
        unheard
    }

    /// Sends `message` to `agent`'s run through the active [`Runner`] with `mode`; on a
    /// failure, asks [`Runner::status`] whether the run is gone.
    async fn send_wake(&self, agent: &AgentId, message: &str, mode: SendMode) -> Sent {
        let thread = agent.native();
        let handle = RunHandle::new(self.runner.kind(), thread).with_agent_id(agent.clone());
        let msg = SteerMessage::new(message).with_mode(mode);
        let err =
            match tokio::time::timeout(WAKE_SEND_TIMEOUT, self.runner.send(&handle, &msg)).await {
                Ok(Ok(())) => return Sent::Delivered,
                Ok(Err(err)) if err.downcast_ref::<TimedOut>().is_some() => {
                    return Sent::MaybeDelivered(err);
                }
                Ok(Err(err)) => err,
                Err(_) => {
                    return Sent::MaybeDelivered(
                        TimedOut(format!("{} send {thread}", self.runner.kind())).into(),
                    );
                }
            };
        match tokio::time::timeout(WAKE_SEND_TIMEOUT, self.runner.status(&handle)).await {
            Ok(Ok(status)) if !status.runnable => Sent::Gone(err),
            _ => Sent::Failed(err),
        }
    }

    /// Ends `out`'s message with its [`wake_marker`]. When it carries something not sent
    /// twice yet, records it in
    /// [`WatchState::unsure_wakes`] before the send, so a flush cut short while sending
    /// leaves it to be looked for; [`Self::settle`] drops it unless the send timed out.
    fn mark_unsure(&mut self, out: &mut Outgoing, now: Timestamp) {
        let digest = wake_digest(&out.message, now);
        out.message = format!("{}\n{}", out.message, wake_marker(&digest));
        let message = UnsureMessage {
            digest: digest.clone(),
            wakes: out.wakes.iter().filter(|w| !w.resent).cloned().collect(),
            since: now,
        };
        if message.is_empty() {
            return;
        }
        let unsure = self
            .state
            .unsure_wakes
            .entry(out.agent.clone())
            .or_default();
        unsure.messages.push(message);
        out.unsure = Some(digest);
    }

    /// Updates `agent`'s timed-out messages once `out` was sent: one that was delivered
    /// settles what earlier ones carried of the same, and one that failed is retried as a
    /// queued wake-up, not looked for. A gone thread drops them all, returning their
    /// wake-ups, which are passed on with the rest.
    fn settle_unsure(&mut self, agent: &AgentId, out: &Outgoing, sent: &Sent) -> Vec<Wake> {
        if matches!(sent, Sent::Gone(_)) {
            return self
                .state
                .unsure_wakes
                .remove(agent)
                .into_iter()
                .flat_map(|u| u.messages)
                .flat_map(|m| m.wakes)
                .collect();
        }
        let Some(unsure) = self.state.unsure_wakes.get_mut(agent) else {
            return Vec::new();
        };
        match sent {
            Sent::MaybeDelivered(_) | Sent::Gone(_) => {}
            Sent::Failed(_) => unsure
                .messages
                .retain(|m| Some(&m.digest) != out.unsure.as_ref()),
            Sent::Delivered => {
                unsure
                    .messages
                    .retain(|m| Some(&m.digest) != out.unsure.as_ref());
                for message in &mut unsure.messages {
                    message.wakes.retain(|w| {
                        !out.wakes
                            .iter()
                            .any(|d| d.board == w.board && d.about == w.about)
                    });
                }
                unsure.messages.retain(|m| !m.is_empty());
            }
        }
        if unsure.messages.is_empty() {
            self.state.unsure_wakes.remove(agent);
        }
        Vec::new()
    }

    /// Records how sending `out` went.
    fn settle(&mut self, out: Outgoing, sent: Sent, now: Timestamp) {
        let agent = out.agent.clone();
        let unsure_wakes = self.settle_unsure(&agent, &out, &sent);
        let recorded = out.unsure.is_some();
        let kinds = || {
            let mut kinds: Vec<&str> = out.wakes.iter().map(|w| w.about.kind()).collect();
            kinds.sort_unstable();
            kinds.dedup();
            kinds.join(", ")
        };
        match sent {
            Sent::Delivered => {
                tracing::info!("woke {agent}: {}", kinds());
                if let Some(queue) = self.state.wakes.get_mut(&agent) {
                    queue.backoff = None;
                }
            }
            Sent::MaybeDelivered(err) => {
                if recorded {
                    tracing::warn!(
                        "waking {agent} ({}): {err:#}; T3 may have delivered it, so a flush \
                         looks for it in the thread",
                        kinds()
                    );
                } else {
                    tracing::warn!(
                        "waking {agent} ({}): {err:#}; its lines were sent twice already, so \
                         not sent again",
                        kinds()
                    );
                }
            }
            Sent::Gone(err) => {
                tracing::warn!(
                    "waking {agent}: {err:#}; its thread is gone, so its wake-ups are dropped"
                );
                let mut wakes = out.wakes;
                wakes.extend(unsure_wakes);
                if let Some(queue) = self.state.wakes.remove(&agent) {
                    wakes.extend(queue.wakes);
                }
                let unheard = self.forward(&agent, &mut wakes, true, now);
                if unheard > 0 {
                    tracing::warn!(
                        "waking {agent}: {unheard} answers to its asks or idle notices dropped: \
                         this device does not dispatch their boards"
                    );
                }
                self.record_drop(
                    "wake_thread_gone",
                    wakes.first().map(|w| &w.board),
                    None,
                    Some(&agent),
                    &format!(
                        "thread gone while sending ({err:#}); {} wake-up(s) dropped ({unheard} unheard)",
                        wakes.len()
                    ),
                    serde_json::to_value(&wakes).unwrap_or_default(),
                    now,
                );
                self.agents.remove(&agent);
            }
            Sent::Failed(err) => {
                let mut queue = WakeQueue {
                    wakes: out.wakes,
                    backoff: out.backoff,
                };
                let delay = back_off(&mut queue, now, false);
                tracing::warn!("waking {agent}: {err:#}; trying again in {delay}");
                self.requeue(agent, queue);
            }
        }
    }

    /// The reset every Claude agent's wake-ups wait for at `now`: that of a full window of
    /// a fresh reading ([`Usage::full_until`]). A daemon with no reading at all keeps the
    /// reset it last waited for, as [`Self::usage_holds`] keeps its boards.
    fn usage_full_until(&self, now: Timestamp) -> Option<Timestamp> {
        match &self.usage {
            Some(usage) => usage.full_until(now),
            None => self.state.usage_full_until.filter(|until| *until > now),
        }
    }

    /// Whether `seen`'s wake-ups can be sent at `now`.
    fn reach(&self, seen: &Seen, now: Timestamp) -> Reach {
        Reach::of(
            seen,
            &self.ctx.device,
            self.runner_source(),
            self.usage_full_until(now),
            now,
        )
    }

    /// The reset `seen`'s wake-ups last waited for, from which they live: its own
    /// ([`paused_until`]), or for a Claude agent that of a full usage window, the later one.
    fn waited_until(&self, seen: &Seen) -> Option<Timestamp> {
        let full = self
            .state
            .usage_full_until
            .filter(|_| matches!(seen.agent.runtime, Runtime::Claude | Runtime::Antigravity));
        paused_until(seen).max(full)
    }

    /// Whether the usage holds `slug` from new work at `now`: a window of a fresh reading
    /// is at or above the board's `usage_stop_at`, or the weekly pace is 0.
    pub fn usage_holds(&self, slug: &BoardSlug, now: Timestamp) -> bool {
        let (Some(board), Some(usage)) = (self.boards.get(slug), &self.usage) else {
            return false;
        };
        usage.over(board.usage_stop_at, now).is_some()
            || usage.pace_limit(board.parallel_tasks.max(1), now) == 0
    }

    /// Whether a hard usage limit holds `slug` at `now`: `usage_stop_at` or the weekly
    /// reserve's hard line (`100 - reserve`). Review judges skip the soft daily pace hold
    /// (ask #203 / agent-board-618), but these hard limits still hold them.
    pub fn usage_holds_hard(&self, slug: &BoardSlug, now: Timestamp) -> bool {
        let (Some(board), Some(usage)) = (self.boards.get(slug), &self.usage) else {
            return false;
        };
        usage.over(board.usage_stop_at, now).is_some() || usage.hard_line_reached(now)
    }

    /// How many of a board's `slots` the weekly pace leaves at `now`.
    fn pace_limit(&self, slots: u32, now: Timestamp) -> u32 {
        self.usage
            .as_ref()
            .map_or(slots, |u| u.pace_limit(slots, now))
    }

    /// How long after its last reading the daemon reads the usage endpoint again: every
    /// minute near a pace line or while waiting to anchor a newly crossed pace day, unless
    /// the endpoint refused a reading in the last 30 minutes; else every 5 minutes.
    fn usage_poll_every(&self, now: Timestamp) -> TimeDelta {
        let refused = self
            .usage_refused
            .is_some_and(|at| now - at < USAGE_REFUSED_BACKOFF);
        let near = self.usage.as_ref().is_some_and(|u| u.near_line(now));
        let crossed_day = self.usage.as_ref().is_some_and(|u| {
            u.fresh(now)
                && u.pace.is_some_and(|p| now >= p.next_day())
                && u.seven_day
                    .and_then(|w| w.resets_at)
                    .is_none_or(|r| r > now)
        });
        if (near || crossed_day) && !refused {
            USAGE_POLL_NEAR
        } else {
            USAGE_POLL_EVERY
        }
    }

    /// Takes a new usage reading and reports it to the server: for ClaudeCode, T3 Code's
    /// snapshot while one is new enough and can follow the reading known here
    /// ([`crate::usage::follows`]), else the endpoint when [`Self::usage_poll_every`] has
    /// passed (with [`USAGE_CHECK_EVERY`] while snapshots stand in unless near a line); for
    /// Antigravity, the official `agy`/`antigravity -p /usage --output-format json` CLI on
    /// the same poll cadence (deduped across devices when a known server reading is already
    /// fresh within the interval), falling back to any fresh legacy snapshot file when no CLI
    /// binary is available. With no new reading, takes the newest any device reported. A
    /// failure keeps the last reading; once stale, only a measured weekly pace breach holds
    /// monotonically until the next pace day or weekly reset.
    async fn refresh_usage(&mut self, now: Timestamp) {
        let mut reading = None;
        let mut report_provider: Option<&'static str> = None;
        let own_known = (!self.usage_from_fallback)
            .then_some(self.usage.as_ref())
            .flatten();
        if let UsageSource::ClaudeCode {
            url,
            credentials,
            snapshots,
        } = &self.usage_source
        {
            report_provider = Some(board_proto::LEGACY_USAGE_PROVIDER);
            let every = self.usage_poll_every(now);
            let snapshot = crate::usage::read_snapshots(snapshots, now)
                .await
                .into_iter()
                .filter(|s| now - s.read_at < every + USAGE_SNAPSHOT_SLACK)
                .filter(|s| own_known.is_none_or(|known| crate::usage::follows(s, known)))
                .max_by_key(|s| s.read_at);
            let wait = if snapshot.is_some() && every > USAGE_POLL_NEAR {
                USAGE_CHECK_EVERY
            } else {
                every
            };
            let waited = self.usage_polled.is_none_or(|at| now - at >= wait);
            let due = match self.usage_retry_at {
                // The endpoint's own wait replaces the usual one, but a reading that
                // stands in still saves the call.
                Some(at) => now >= at && (snapshot.is_none() || waited),
                None => waited,
            };
            if due {
                self.usage_polled = Some(now);
                self.usage_retry_at = None;
                match crate::usage::read(url, credentials, now).await {
                    Ok(Some(report)) => reading = Some(report),
                    Ok(None) => tracing::debug!(
                        "usage: Claude Code is not signed in here or its token has expired"
                    ),
                    Err(err) => {
                        if let Some(refused) = err.downcast_ref::<crate::usage::Refused>() {
                            self.usage_refused = Some(now);
                            self.usage_retry_at = refused.retry_after.map(|wait| {
                                let wait = TimeDelta::from_std(wait).unwrap_or(USAGE_RETRY_MAX);
                                now + wait.clamp(USAGE_POLL_NEAR, USAGE_RETRY_MAX)
                            });
                        }
                        tracing::warn!("reading the usage: {err:#}");
                    }
                }
            }
            // The endpoint's reading is the newer one; T3 Code's is news only once, also
            // when its report fails and the server's older reading comes back.
            if reading.is_none()
                && let Some(snapshot) = snapshot.filter(|s| {
                    self.usage_snapshot_at.is_none_or(|at| s.read_at > at)
                        && own_known.is_none_or(|k| s.read_at > k.read_at)
                })
            {
                self.usage_snapshot_at = Some(snapshot.read_at);
                reading = Some(snapshot);
            }
        } else if let UsageSource::Antigravity { cli_bin, snapshots } = &self.usage_source {
            report_provider = Some("antigravity");
            if let Some(bin) = cli_bin {
                let every = self.usage_poll_every(now);
                let waited = self.usage_polled.is_none_or(|at| now - at >= every);
                // Avoid redundant CLI invocations when another device (or a recent read)
                // already reported a fresh reading within this poll interval.
                let fresh_known = own_known.is_some_and(|k| now - k.read_at < every);
                let due = match self.usage_retry_at {
                    Some(at) => now >= at && !fresh_known,
                    None => waited && !fresh_known,
                };
                if due {
                    self.usage_polled = Some(now);
                    self.usage_retry_at = None;
                    match crate::usage::read_antigravity_cli(bin, now).await {
                        Ok(Some(report)) => reading = Some(report),
                        Ok(None) => tracing::debug!(
                            "usage: Antigravity CLI ({}) is not signed in here",
                            bin.display()
                        ),
                        Err(err) => {
                            self.usage_retry_at = Some(now + USAGE_POLL_EVERY);
                            tracing::warn!("reading Antigravity usage via CLI: {err:#}");
                        }
                    }
                }
            } else {
                let snapshot = crate::usage::read_snapshots(snapshots, now)
                    .await
                    .into_iter()
                    .filter(|s| {
                        now - s.read_at <= TimeDelta::minutes(board_proto::USAGE_FRESH_MINUTES)
                    })
                    .filter(|s| own_known.is_none_or(|known| crate::usage::follows(s, known)))
                    .max_by_key(|s| s.read_at);
                if let Some(snapshot) = snapshot.filter(|s| {
                    self.usage_snapshot_at.is_none_or(|at| s.read_at > at)
                        && own_known.is_none_or(|k| s.read_at > k.read_at)
                }) {
                    self.usage_snapshot_at = Some(snapshot.read_at);
                    reading = Some(snapshot);
                }
            }
        }
        let bucket_ref = self.ctx.config.quota_bucket();
        let is_legacy_bucket = bucket_ref.provider == board_proto::LEGACY_USAGE_PROVIDER
            && bucket_ref.account == board_proto::LEGACY_USAGE_ACCOUNT;
        let mut reported = None;
        if let (Some(report), Some(provider)) = (reading, report_provider) {
            let res = if self.usage_buckets_unsupported {
                if provider == board_proto::LEGACY_USAGE_PROVIDER {
                    self.client().report_usage(&report).await
                } else {
                    Err(anyhow::anyhow!(
                        "server does not support per-provider usage buckets for {provider}"
                    ))
                }
            } else {
                let bucket_req = board_proto::PutUsageBucket::from(report.clone());
                match self
                    .client()
                    .put_usage_bucket(provider, &bucket_ref.account, &bucket_req)
                    .await
                {
                    Ok(bucket) => Ok(bucket.as_usage().unwrap_or_else(|| Usage {
                        five_hour: report.five_hour,
                        seven_day: report.seven_day,
                        read_at: report.read_at,
                        device: self.ctx.device.clone(),
                        pace: own_known.and_then(|u| u.pace),
                        reserve_until: own_known.and_then(|u| u.reserve_until),
                        reserve_prev: own_known.and_then(|u| u.reserve_prev),
                    })),
                    Err(err) if is_heartbeat_unsupported(&err) => {
                        self.usage_buckets_unsupported = true;
                        self.client().report_usage(&report).await
                    }
                    Err(err) => Err(err),
                }
            };
            let usage = match res {
                Ok(usage) => usage,
                Err(err) => {
                    tracing::warn!("reporting the usage: {err:#}");
                    // The server is likely unreachable too: act on the local reading.
                    Usage {
                        five_hour: report.five_hour,
                        seven_day: report.seven_day,
                        read_at: report.read_at,
                        device: self.ctx.device.clone(),
                        // The server computes the pace: keep the last one it gave, so
                        // a failed report does not lift its hold.
                        pace: own_known.and_then(|u| u.pace),
                        reserve_until: own_known.and_then(|u| u.reserve_until),
                        reserve_prev: own_known.and_then(|u| u.reserve_prev),
                    }
                }
            };
            if provider == bucket_ref.provider {
                reported = Some(usage);
            }
        }
        match reported {
            Some(usage) => {
                self.usage = Some(usage);
                self.usage_from_fallback = false;
            }
            None if is_legacy_bucket || self.usage_buckets_unsupported => {
                match self.client().usage().await {
                    Ok(Some(usage)) => {
                        self.usage = Some(usage);
                        self.usage_from_fallback = !is_legacy_bucket;
                    }
                    Ok(None) => {}
                    Err(err) => tracing::debug!("reading the usage from the server: {err:#}"),
                }
            }
            None => match self.client().list_usage_buckets().await {
                Ok(buckets) => {
                    if let Some(usage) = buckets
                        .iter()
                        .find(|b| {
                            b.provider == bucket_ref.provider && b.account == bucket_ref.account
                        })
                        .and_then(|b| b.as_usage())
                    {
                        self.usage = Some(usage);
                        self.usage_from_fallback = false;
                    } else if let Some(usage) = buckets
                        .iter()
                        .find(|b| {
                            b.provider == board_proto::LEGACY_USAGE_PROVIDER
                                && b.account == board_proto::LEGACY_USAGE_ACCOUNT
                        })
                        .and_then(|b| b.as_usage())
                    {
                        self.usage = Some(usage);
                        self.usage_from_fallback = true;
                    }
                }
                Err(err) if is_heartbeat_unsupported(&err) => {
                    self.usage_buckets_unsupported = true;
                    match self.client().usage().await {
                        Ok(Some(usage)) => {
                            self.usage = Some(usage);
                            self.usage_from_fallback = !is_legacy_bucket;
                        }
                        Ok(None) => {}
                        Err(err) => tracing::debug!("reading the usage from the server: {err:#}"),
                    }
                }
                Err(err) => tracing::debug!("reading usage buckets from the server: {err:#}"),
            },
        }
    }

    /// The line for `wake` when what it is about still holds, else `None`.
    async fn recheck(
        &self,
        agent: &AgentId,
        wake: &Wake,
        now: Timestamp,
    ) -> Result<Option<String>> {
        let slug = &wake.board;
        // A paused board starts no turn.
        if self
            .boards
            .get(slug)
            .is_some_and(|b| b.status == BoardStatus::Paused)
        {
            return Ok(None);
        }
        // The weekly usage at the user's reserve holds a board as a pause does.
        if self
            .usage
            .as_ref()
            .is_some_and(|u| u.hard_since(now).is_some())
        {
            return Ok(None);
        }
        let line = match &wake.about {
            // Still the agent's, still blocked, and nothing left to wait on.
            WakeAbout::Unblocked { task, .. } => {
                let task = self.client().task(task).await?.task;
                let due = task.owner.as_ref() == Some(agent)
                    && task.status == TaskStatus::Blocked
                    && self.blockers_done(&task).await?;
                // A blocked reason may name another wait, such as an ask.
                let next = if task.blocked_reason.is_some() {
                    format!(
                        "check its blocked reason (task_show {}) before resuming",
                        task.id
                    )
                } else if let Some(at) = task.not_before.filter(|&at| at > now) {
                    // The server refuses the move to doing until then.
                    format!(
                        "it waits until {}; resume it then (task_update {} status=doing)",
                        at.format("%Y-%m-%d %H:%MZ"),
                        task.id
                    )
                } else {
                    format!("resume it (task_update {} status=doing)", task.id)
                };
                due.then(|| {
                    format!(
                        "board {slug}: every blocker of your blocked {} is done; {next}",
                        task.id
                    )
                })
            }
            // An answer stays answered; only its text is read.
            WakeAbout::Answered { ask, asker } if asker == agent => {
                let ask = self.client().ask(*ask).await?;
                let about = ask
                    .task
                    .as_ref()
                    .map(|t| format!(" about {t}"))
                    .unwrap_or_default();
                let answer = serde_json::to_string(ask.answer.as_deref().unwrap_or_default())?
                    .replace('<', "\\u003c")
                    .replace('>', "\\u003e");
                Some(format!(
                    "the user answered ask #{}{about}: {answer}",
                    ask.id
                ))
            }
            // Still the owner's, still in doing, and the owner not back since the notice.
            WakeAbout::Idle {
                task,
                away,
                since,
                noticed,
            } if away == agent => {
                let task = self.client().task(task).await?.task;
                let due = task.owner.as_ref() == Some(away)
                    && task.status == TaskStatus::Doing
                    && !self.back_since(away, *noticed).await?;
                let since = since.format("%Y-%m-%d %H:%MZ");
                due.then(|| {
                    format!(
                        "board {slug}: your {} is in doing, and you have been away from it \
                         since {since}. Continue it, or park it by what it waits on: review \
                         with a note naming the PR, blocked with the reason, or open with \
                         your handoff",
                        task.id
                    )
                })
            }
            // Queued for another agent than the one it is about (or handled via sticky wakes).
            WakeAbout::Idle { .. } | WakeAbout::Answered { .. } | WakeAbout::Sticky { .. } => None,
        };
        Ok(line)
    }

    /// Handles every event after `last_seq` through the paged events endpoint.
    pub async fn catch_up(&mut self, now: Timestamp) -> Result<bool> {
        let mut reconcile = false;
        loop {
            let since = self.state.last_seq;
            let page = self.client().events(since, None, Some(200)).await?;
            for event in &page.events {
                reconcile |= self.handle(event, now).await;
            }
            if page.events.is_empty() || Some(page.next_since) == since {
                break;
            }
            self.state.last_seq = Some(page.next_since);
        }
        Ok(reconcile)
    }

    /// Handles an event from the feed. One that fails on a transient error (the server
    /// restarting, the network down) is kept for [`Self::retry_failed`]; either way the feed
    /// moves past it, so one bad event cannot stall the daemon.
    async fn handle(&mut self, event: &Event, now: Timestamp) -> bool {
        match self.on_event(event, now).await {
            Ok(reconcile) => reconcile,
            Err(err) => {
                if is_transient(&err) {
                    tracing::warn!("event #{}: {err:#}; retrying later", event.seq);
                    if !self.state.retry.iter().any(|e| e.seq == event.seq) {
                        self.state.retry.push(event.clone());
                    }
                    if self.state.retry.len() > RETRY_EVENTS_MAX {
                        let dropped = self.state.retry.remove(0);
                        tracing::warn!("event #{}: too many to retry; dropped", dropped.seq);
                        self.record_drop(
                            "event_retry_dropped",
                            dropped.board.as_ref(),
                            dropped.task.as_ref(),
                            None,
                            &format!(
                                "event retry queue exceeded RETRY_EVENTS_MAX ({RETRY_EVENTS_MAX})"
                            ),
                            serde_json::to_value(&dropped).unwrap_or_default(),
                            now,
                        );
                    }
                } else {
                    tracing::warn!("event #{}: {err:#}", event.seq);
                    self.record_drop(
                        "event_non_transient",
                        event.board.as_ref(),
                        event.task.as_ref(),
                        None,
                        &format!("non-transient event handling error: {err:#}"),
                        serde_json::to_value(event).unwrap_or_default(),
                        now,
                    );
                }
                self.state.last_seq = Some(self.state.last_seq.unwrap_or(0).max(event.seq));
                false
            }
        }
    }

    /// Handles again the events that failed on a transient error, dropping those older than
    /// [`RETRY_EVENTS_FOR`] or failing for good. Returns true when the boards should be
    /// reconciled. An event leaves the list only once settled, so a retry cut short keeps
    /// it. A server that does not answer at all stops the round: the rest wait for the next
    /// tick untried, so an outage costs one request timeout, not one per event. An event
    /// the server answers with an error does not hold up the others.
    pub async fn retry_failed(&mut self, now: Timestamp) -> bool {
        let mut expired = Vec::new();
        self.state.retry.retain(|event| {
            let keep = now - event.at <= RETRY_EVENTS_FOR;
            if !keep {
                tracing::warn!("event #{}: still failing; dropped", event.seq);
                expired.push(event.clone());
            }
            keep
        });
        for ev in expired {
            self.record_drop(
                "event_retry_dropped",
                ev.board.as_ref(),
                ev.task.as_ref(),
                None,
                &format!(
                    "event exceeded RETRY_EVENTS_FOR ({}s)",
                    RETRY_EVENTS_FOR.num_seconds()
                ),
                serde_json::to_value(&ev).unwrap_or_default(),
                now,
            );
        }
        let mut reconcile = false;
        for event in self.state.retry.clone() {
            let settled = match self.on_event(&event, now).await {
                Ok(again) => {
                    reconcile |= again;
                    true
                }
                Err(err) if is_transient(&err) => {
                    if is_unreachable(&err) {
                        tracing::warn!("event #{}: {err:#}; retrying the rest later", event.seq);
                        break;
                    }
                    false
                }
                Err(err) => {
                    tracing::warn!("event #{}: {err:#}; dropped", event.seq);
                    self.record_drop(
                        "event_non_transient",
                        event.board.as_ref(),
                        event.task.as_ref(),
                        None,
                        &format!("non-transient event retry error: {err:#}"),
                        serde_json::to_value(&event).unwrap_or_default(),
                        now,
                    );
                    true
                }
            };
            if settled {
                self.state.retry.retain(|e| e.seq != event.seq);
            }
        }
        reconcile
    }
}

/// Backs `queue` off after a failure: nothing is sent before the returned delay passes,
/// and after a failed `lookup` the agent is not looked up again before then either.
fn back_off(queue: &mut WakeQueue, now: Timestamp, lookup: bool) -> TimeDelta {
    let last = queue
        .backoff
        .as_ref()
        .map(|b| TimeDelta::seconds(b.delay_secs));
    let delay = next_delay(last, WAKE_RETRY_FIRST, WAKE_RETRY_MAX);
    queue.backoff = Some(Backoff {
        delay_secs: delay.num_seconds(),
        retry_at: now + delay,
        lookup,
    });
    delay
}

/// One `idle_doing` notice spell observed in a task's history.
struct IdleSpell {
    since: Timestamp,
    noticed: Timestamp,
    count: usize,
}

/// Finds the latest `idle_doing` notice spell for `owner` in `history` after the task last
/// entered `doing` or changed owner (matching `ENTERED_DOING` on the server).
fn idle_spell_from_history(history: &[Event], owner: &AgentId) -> Option<IdleSpell> {
    let entered_pos = history.iter().rposition(|ev| {
        matches!(ev.event_type, EventType::TaskClaimed | EventType::TaskTaken)
            || (ev.event_type == EventType::TaskUpdated
                && (ev.payload.get("to").and_then(|v| v.as_str()) == Some("doing")
                    || ev.payload.get("owner").is_some()))
    });
    let slice = match entered_pos {
        Some(pos) => &history[pos + 1..],
        None => history,
    };
    let mut latest: Option<IdleSpell> = None;
    for ev in slice {
        if ev.event_type != EventType::TaskUpdated {
            continue;
        }
        if ev.payload.get("away").and_then(|v| v.as_str()) != Some(owner.as_str()) {
            continue;
        }
        let Some(raw_since) = ev.payload.get("idle_doing") else {
            continue;
        };
        let Ok(since) = serde_json::from_value::<Timestamp>(raw_since.clone()) else {
            continue;
        };
        match &mut latest {
            Some(spell) if spell.since == since => {
                spell.count += 1;
                spell.noticed = spell.noticed.max(ev.at);
            }
            _ => {
                latest = Some(IdleSpell {
                    since,
                    noticed: ev.at,
                    count: 1,
                });
            }
        }
    }
    latest
}

/// The step the dispatcher owes for `wake`, which no device can wake `from` for.
fn owed_for(from: &AgentId, wake: &Wake, now: Timestamp) -> Option<Owed> {
    match &wake.about {
        WakeAbout::Answered { ask, asker } => Some(Owed::Answer {
            board: wake.board.clone(),
            ask: *ask,
            asker: asker.clone(),
            check_gone: false,
            noted: false,
            at: now,
        }),
        WakeAbout::Idle {
            task,
            away,
            since,
            noticed,
        } if away == from => Some(Owed::Release {
            board: wake.board.clone(),
            task: task.clone(),
            away: away.clone(),
            since: *since,
            noticed: *noticed,
            noted: false,
            at: now,
        }),
        _ => None,
    }
}

/// Whether `agent` is gone: ended, or unknown to the server.
async fn gone(client: &Client, agent: &AgentId) -> Result<bool> {
    match client.agent_info(agent).await {
        Ok(info) => Ok(info.state == AgentState::Ended),
        Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => Ok(true),
        Err(err) => Err(err),
    }
}

/// `text` cut to the server's note limit, ending in an ellipsis when cut.
pub(crate) fn clip_note(text: String) -> String {
    let max = crate::ops::limits().note_chars;
    if text.chars().count() <= max {
        return text;
    }
    let mut cut: String = text.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

pub(crate) async fn add_note(
    client: &Client,
    task: &TaskId,
    agent: &AgentId,
    kind: NoteKind,
    text: String,
) -> Result<()> {
    client
        .add_note(
            task,
            &AddNote {
                agent: agent.clone(),
                kind,
                text,
            },
        )
        .await?;
    Ok(())
}

/// Sets a held task (`doing`, `blocked`, or `review`) back to `open`.
///
/// Uses `POST /v1/tasks/{id}/release` when the task has an owner, and `PATCH /v1/tasks/{id}`
/// with `status: open` when it has no owner (which clears `blocked_reason`, whereas `/release`
/// refuses ownerless tasks with `403 forbidden`).
pub(crate) async fn reopen_held_task(client: &Client, agent: &AgentId, task: &Task) -> Result<()> {
    if task.owner.is_some() {
        client.task_action(&task.id, "release").await?;
    } else {
        client
            .update_task(
                &task.id,
                &UpdateTask {
                    agent: Some(agent.clone().into()),
                    version: task.version,
                    status: Some(TaskStatus::Open),
                    title: None,
                    body: None,
                    acceptance: None,
                    priority: None,
                    blockers: None,
                    blocked_reason: None,
                    auto: None,
                    not_before: None,
                    rank: None,
                    owner: None,
                    reason: None,
                    repo: None,
                    weight: None,
                    resource_units: None,
                    requires: None,
                    tolerations: None,
                    conflict_keys: None,
                    affinity: None,
                    preferred_runner: None,
                    size: None,
                    cooled: None,
                    before: None,
                    after: None,
                },
            )
            .await?;
    }
    Ok(())
}

/// Is this a failure that may pass on its own: an unreachable/timed-out transport, a 5xx
/// (or 408/429) server/proxy error, or a transient OS I/O error? Local parse/decode/logic
/// errors and 4xx client errors are NOT transient and are never retried for 30 minutes
/// (`agent-board-539`).
pub fn is_transient(err: &anyhow::Error) -> bool {
    if let Some(server) = err.chain().find_map(|c| c.downcast_ref::<ServerError>()) {
        return !server.status.is_client_error()
            || matches!(
                server.status,
                reqwest::StatusCode::REQUEST_TIMEOUT | reqwest::StatusCode::TOO_MANY_REQUESTS
            );
    }
    for cause in err.chain() {
        if let Some(req) = cause.downcast_ref::<reqwest::Error>() {
            return req.is_connect() || req.is_timeout() || req.is_request() || req.is_body();
        }
        if let Some(relay) = cause.downcast_ref::<RelayFailure>() {
            return matches!(relay, RelayFailure::Unreachable | RelayFailure::Slow);
        }
        if cause.downcast_ref::<TimedOut>().is_some() {
            return true;
        }
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            return matches!(
                io_err.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::UnexpectedEof
            );
        }
        let msg = cause.to_string();
        if msg.starts_with("the board server may be restarting")
            || msg.starts_with("server answered 5")
            || msg.starts_with("server answered 429")
            || msg.starts_with("server answered 408")
        {
            return true;
        }
    }
    false
}

/// Whether a `PUT /v1/devices/{name}/heartbeat` error indicates an older server that does not
/// implement the endpoint (`404 Not Found` or `405 Method Not Allowed`, whether structured JSON
/// or plain Axum fallback response).
fn is_heartbeat_unsupported(err: &anyhow::Error) -> bool {
    crate::client::is_endpoint_unsupported(err)
}

/// The local T3 project folder for one of the board's repos, with that repo; else the folder
/// `[board_projects]` names for the board, without a repo.
fn project_for(config: &Config, board: &Board) -> Option<(Option<String>, PathBuf)> {
    board
        .repos
        .iter()
        .find_map(|repo| {
            config
                .projects
                .get(repo)
                .map(|path| (Some(repo.clone()), path.clone()))
        })
        .or_else(|| {
            config
                .board_projects
                .get(&board.slug)
                .map(|path| (None, path.clone()))
        })
}

/// The folder a device runs the board's threads in: [`project_for`], which must not be
/// gone, since T3 fails every run of a thread whose folder is gone. Checked before claiming:
/// a device that cannot host the thread leaves the lease alone.
pub fn host_folder(config: &Config, board: &Board) -> Result<PathBuf> {
    let (_, project) = project_for(config, board).with_context(|| no_project(board))?;
    if folder_gone(&project) {
        anyhow::bail!(
            "the project folder {} of board {} is missing on this device: restore it, point \
             config.toml at where it is now, or switch the board's autopilot off (board board \
             update {} --autopilot false)",
            project.display(),
            board.slug,
            board.slug
        );
    }
    Ok(project)
}

fn no_project(board: &Board) -> String {
    if board.repos.is_empty() {
        return format!(
            "no T3 project for board {} on this device: add it under [board_projects] in \
             config.toml; board watch reads it within a minute",
            board.slug
        );
    }
    format!(
        "no T3 project for {} on this device: add it under [projects] (or the board under \
         [board_projects]) in config.toml; board watch reads it within a minute",
        board.repos.join(", ")
    )
}

/// Outcome of re-reading a task right before starting a worker.
enum WorkerRecheck {
    Ready(Box<Task>),
    /// Another agent moved the task to `doing` after `next_tasks` listed it; it now occupies
    /// a `parallel_tasks` slot on the board.
    InProgress(Box<Task>),
    Skipped,
}

/// Whether `task` is still eligible for a newly started worker: open, ready, `auto`, no live
/// owner, not a pending proposal, and not a feedback item.
fn worker_eligible(task: &Task) -> bool {
    task.status == TaskStatus::Open
        && task.ready
        && task.auto
        && (task.owner.is_none() || task.owner_stale)
        && !is_proposal(task)
        && task.feedback.is_none()
}

fn task_status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Open => "open",
        TaskStatus::Doing => "doing",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Review => "review",
        TaskStatus::Done => "done",
        TaskStatus::Dropped => "dropped",
    }
}

fn worker_ineligible_cause(task: &Task) -> String {
    let mut reasons = Vec::new();
    if task.status != TaskStatus::Open {
        reasons.push(format!("status={}", task_status_str(task.status)));
    }
    if !task.ready {
        reasons.push("ready=false".to_owned());
    }
    if !task.auto {
        reasons.push("auto=false".to_owned());
    }
    if let Some(owner) = &task.owner
        && !task.owner_stale
    {
        reasons.push(format!("owner={owner}"));
    }
    if is_proposal(task) {
        reasons.push("proposal=true".to_owned());
    }
    if task.feedback.is_some() {
        reasons.push("feedback=true".to_owned());
    }
    format!(
        "task {} stopped being ready before worker start ({})",
        task.id,
        reasons.join(", ")
    )
}

fn worker_ineligible_subject(task: &Task, run_id: Option<&RunId>) -> serde_json::Value {
    serde_json::json!({
        "run_id": run_id,
        "task": task.id,
        "status": task.status,
        "ready": task.ready,
        "auto": task.auto,
        "owner": task.owner,
        "owner_stale": task.owner_stale,
        "proposal": is_proposal(task),
        "has_feedback": task.feedback.is_some(),
    })
}

/// A pending proposal, which a triage judge reviews before it becomes work.
fn is_proposal(task: &board_proto::Task) -> bool {
    task.is_pending_proposal()
}

/// Names a wake-up message sent at `now` without keeping its text: the first 16 hex digits
/// of a SHA-256 digest of the text and the time, which its [`wake_marker`] carries.
pub fn wake_digest(message: &str, now: Timestamp) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    hash.update(message);
    hash.update([0]);
    hash.update(now.to_rfc3339());
    crate::config::hex(&hash.finalize())[..16].to_owned()
}

/// The last line of each wake-up message, naming its [`wake_digest`], so a flush finds the
/// message among a thread's messages after its send timed out.
pub fn wake_marker(digest: &str) -> String {
    format!("(wake-up {digest})")
}

/// Proof that this process is the only `board watch` for its `$BOARD_HOME`: an exclusive
/// lock on `state/watch.lock`, which holds the owner's pid. The system releases it when
/// the process exits, however it exits, so a restart under launchd or systemd takes it.
#[derive(Debug)]
pub struct WatchLock {
    _file: std::fs::File,
}

/// Takes the [`WatchLock`] of `home`, or says which process holds it.
pub fn lock_watch(home: &Home) -> Result<WatchLock> {
    use std::io::{Read as _, Seek as _, Write as _};
    let dir = home.state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("watch.lock");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut holder = String::new();
            // Best effort: the pid only names the holder in the message.
            let _ = file.read_to_string(&mut holder);
            let holder = match holder.trim().parse::<u32>() {
                Ok(pid) => format!("pid {pid}"),
                Err(_) => "another process".to_owned(),
            };
            anyhow::bail!(
                "board watch is already running for {} ({holder} holds {}); stop it before starting another, or set BOARD_HOME to another device's home",
                home.root().display(),
                path.display()
            );
        }
        Err(std::fs::TryLockError::Error(err)) => {
            return Err(err).with_context(|| format!("locking {}", path.display()));
        }
    }
    file.set_len(0)?;
    file.rewind()?;
    writeln!(file, "{}", std::process::id())?;
    Ok(WatchLock { _file: file })
}

/// Runs the daemon until interrupted or stopped (or one pass with `once`). Refuses to
/// start while another daemon runs for the same `$BOARD_HOME`.
pub async fn run(home: Home, once: bool) -> Result<()> {
    let _lock = lock_watch(&home)?;
    // Listen first, so a stop during start-up is handled once the loop runs.
    let mut shutdown = Shutdown::listen()?;
    let ctx = Ctx::load(home, None, Some(crate::client::REQUEST_TIMEOUT))?;
    let t3 = T3Cli::locate(ctx.config.t3_bin.as_deref());
    tracing::info!(
        "watching as device {} with {}",
        ctx.device,
        t3.bin.display()
    );
    // Hooks of this home reach the server through the relay while the daemon runs; without
    // it they call the server directly, so a relay that cannot start costs only speed.
    let _relay = if once { None } else { start_relay(&ctx) };
    let (config_tx, rpc_task, report_task) = if once {
        (None, None, None)
    } else {
        let (tx, rx) = tokio::sync::watch::channel(ctx.config.clone());
        let task = device_rpc::spawn(
            ctx.client.clone(),
            ctx.device.clone(),
            rx.clone(),
            t3.clone(),
            shutdown.clone(),
        );
        let reporter = crate::host_report::spawn_periodic(
            ctx.client.clone(),
            ctx.device.clone(),
            rx,
            shutdown.clone(),
        );
        (Some(tx), Some(task), Some(reporter))
    };
    let quota_provider = ctx.config.quota_bucket().provider;
    let acp_bin = ctx
        .config
        .acp_runner
        .as_ref()
        .and_then(|c| c.resolved_bin());
    let usage_source = UsageSource::for_provider(
        ctx.config
            .usage_provider()
            .or_else(|| (quota_provider == "antigravity").then_some("antigravity")),
        acp_bin,
    );
    let host_memory = device::host_memory();
    if host_memory.is_none() {
        tracing::warn!(
            "the memory of this host could not be read; without `runs_at_once` in config.toml the device runs {} agents at once",
            device::FALLBACK_RUNS
        );
    }
    let mut daemon = Daemon::new(ctx, t3)
        .with_default_run_limit(device::default_run_limit(host_memory))
        .with_host_memory(host_memory);
    daemon.config_tx = config_tx;
    daemon.usage_source = usage_source;
    // Probe `/healthz` for initial cursor (when missing) and `dispatch-claim-v1` capability.
    match daemon.client().health().await {
        Ok(health) => {
            if daemon.state.last_seq.is_none() {
                daemon.state.last_seq = health["seq"].as_i64();
            }
            if daemon.ctx.config.dispatch_mode == DispatchMode::Auto {
                daemon.set_dispatch_claim_supported(health_supports_dispatch_claim(&health));
            }
        }
        Err(err) if daemon.state.last_seq.is_none() => return Err(err),
        Err(err) => {
            tracing::debug!("probing /healthz on startup: {err:#}");
        }
    }
    if once {
        daemon.reconcile(Utc::now()).await?;
        let mut reconcile = daemon.retry_failed(Utc::now()).await;
        reconcile |= daemon.catch_up(Utc::now()).await?;
        if reconcile {
            daemon.reconcile(Utc::now()).await?;
        }
        daemon.flush(Utc::now()).await;
        if let Err(err) = daemon.send_heartbeat(Utc::now()).await {
            tracing::warn!("device heartbeat: {err:#}");
        }
        return daemon.save();
    }
    let result = daemon.follow(&mut shutdown).await;
    if let Some(rpc_task) = rpc_task {
        let _ = tokio::time::timeout(Duration::from_secs(2), rpc_task).await;
    }
    if let Some(report_task) = report_task {
        let _ = tokio::time::timeout(Duration::from_secs(2), report_task).await;
    }
    result
}

/// Starts the hook relay for `ctx`'s home with the server and token `ctx` loaded, or says
/// why not and carries on without it.
fn start_relay(ctx: &Ctx) -> Option<crate::relay::Relay> {
    let started = ctx.config.require_login().and_then(|(server, _)| {
        let token = ctx.client.token().context("no device token")?;
        crate::relay::start(&ctx.home, server, token.to_owned())
    });
    match started {
        Ok(relay) => {
            tracing::info!("serving hooks on {}", relay.path().display());
            Some(relay)
        }
        Err(err) => {
            tracing::warn!("hooks will call the server directly: {err:#}");
            None
        }
    }
}

/// A request to stop: Ctrl-C, SIGTERM from launchd or systemd stopping the service, or a
/// [`Stopper`]. Once asked, it stays asked.
#[derive(Clone)]
pub struct Shutdown {
    asked: tokio::sync::watch::Receiver<bool>,
}

/// Asks the daemon following a [`Shutdown`] to stop.
pub struct Stopper(tokio::sync::watch::Sender<bool>);

impl Stopper {
    pub fn stop(&self) {
        self.0.send_replace(true);
    }
}

impl Shutdown {
    /// Stops on Ctrl-C or SIGTERM.
    pub fn listen() -> Result<Self> {
        let (stopper, shutdown) = Self::manual();
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut interrupt = signal(SignalKind::interrupt()).context("listening for SIGINT")?;
            let mut terminate = signal(SignalKind::terminate()).context("listening for SIGTERM")?;
            tokio::spawn(async move {
                tokio::select! {
                    _ = interrupt.recv() => {}
                    _ = terminate.recv() => {}
                }
                stopper.stop();
            });
        }
        #[cfg(not(unix))]
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                stopper.stop();
            }
        });
        Ok(shutdown)
    }

    /// Stops when the returned [`Stopper`] says so.
    pub fn manual() -> (Stopper, Self) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        (Stopper(tx), Self { asked: rx })
    }

    /// Resolves when the daemon is asked to stop.
    pub async fn wait(&mut self) {
        // Every stopper gone without asking: never.
        if self.asked.wait_for(|asked| *asked).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// What the follow loop does next.
enum Step {
    Event(String),
    Flush,
    Reconcile,
    Heartbeat,
}

impl<T: T3 + 'static> Daemon<T> {
    /// Reconciles at once, then follows the event stream, flushing, heartbeating and
    /// reconciling on their ticks, also while the stream is down, until `shutdown` asks it
    /// to stop. Each step races the request to stop, so a step that waits on a server or
    /// `t3` never delays the exit; every step is safe to cut short.
    pub async fn follow(&mut self, shutdown: &mut Shutdown) -> Result<()> {
        let first = self.timing.reconnect_first;
        let mut delay = first;
        let mut reconcile_tick = tokio::time::interval(self.timing.reconcile_every);
        let mut flush_tick = tokio::time::interval(self.timing.flush_every);
        let mut heartbeat_tick = tokio::time::interval(self.timing.heartbeat_every);
        let mut reconcile_needed = false;
        if self
            .run_step(Step::Reconcile, &mut reconcile_needed, shutdown)
            .await?
        {
            return self.stop().await;
        }
        if self
            .run_step(Step::Heartbeat, &mut reconcile_needed, shutdown)
            .await?
        {
            return self.stop().await;
        }
        reconcile_tick.reset();
        heartbeat_tick.reset();
        loop {
            let client = self.client().clone();
            let connected = tokio::select! {
                connected = client.event_stream(self.state.last_seq) => connected,
                () = shutdown.wait() => return self.stop().await,
            };
            match connected {
                Ok(resp) => {
                    tracing::info!(
                        "event stream open after #{}",
                        self.state.last_seq.unwrap_or(0)
                    );
                    // Re-scan local transcripts on stream (re)connect in case any run was
                    // orphaned server-side while this daemon was offline.
                    self.orphaned_transcripts_checked = false;
                    // The daemon runs for days: log each server build that differs from
                    // this one.
                    if let Some(warning) =
                        crate::client::take_skew_warning(crate::client::Reporter::Daemon)
                    {
                        tracing::warn!("{warning}");
                    }
                    let opened = tokio::time::Instant::now();
                    let mut stream =
                        Box::pin(idle_limited(resp, self.timing.stream_idle).eventsource());
                    loop {
                        let step = tokio::select! {
                            item = stream.next() => match item {
                                // The delay resets only after a stable connection (below),
                                // so a stream that drops after each event still backs off.
                                Some(Ok(message)) => Step::Event(message.data),
                                Some(Err(err)) => {
                                    tracing::warn!("event stream: {err}");
                                    break;
                                }
                                None => break,
                            },
                            _ = flush_tick.tick() => Step::Flush,
                            _ = reconcile_tick.tick() => Step::Reconcile,
                            _ = heartbeat_tick.tick() => Step::Heartbeat,
                            () = shutdown.wait() => return self.stop().await,
                        };
                        if self.run_step(step, &mut reconcile_needed, shutdown).await? {
                            return self.stop().await;
                        }
                    }
                    if opened.elapsed() >= STREAM_HEALTHY {
                        delay = first;
                    }
                    tracing::info!(
                        "event stream ended; reconnecting in {:.1}s",
                        delay.as_secs_f32()
                    );
                }
                Err(err) => tracing::warn!(
                    "event stream: {err:#}; retrying in {:.1}s",
                    delay.as_secs_f32()
                ),
            }
            // Until the stream is back, leases are still renewed and wake-ups still sent.
            let reopen = tokio::time::sleep(delay);
            tokio::pin!(reopen);
            loop {
                let step = tokio::select! {
                    () = &mut reopen => break,
                    _ = flush_tick.tick() => Step::Flush,
                    _ = reconcile_tick.tick() => Step::Reconcile,
                    _ = heartbeat_tick.tick() => Step::Heartbeat,
                    () = shutdown.wait() => return self.stop().await,
                };
                if self.run_step(step, &mut reconcile_needed, shutdown).await? {
                    return self.stop().await;
                }
            }
            delay = (delay * 2).min(RECONNECT_MAX);
        }
    }

    /// Runs `step` unless asked to stop first; true when asked.
    async fn run_step(
        &mut self,
        step: Step,
        reconcile_needed: &mut bool,
        shutdown: &mut Shutdown,
    ) -> Result<bool> {
        tokio::select! {
            done = self.step(step, reconcile_needed) => done.map(|()| false),
            () = shutdown.wait() => Ok(true),
        }
    }

    async fn step(&mut self, step: Step, reconcile_needed: &mut bool) -> Result<()> {
        let now = (self.clock)();
        match step {
            Step::Event(data) => match serde_json::from_str::<Event>(&data) {
                Ok(event) => *reconcile_needed |= self.handle(&event, now).await,
                Err(err) => tracing::warn!("unreadable event: {err}"),
            },
            Step::Flush => {
                if std::mem::take(reconcile_needed) {
                    self.reconcile_logged().await;
                }
                self.flush((self.clock)()).await;
                self.save()?;
            }
            Step::Reconcile => {
                *reconcile_needed |= self.retry_failed(now).await;
                self.reconcile_logged().await;
            }
            Step::Heartbeat => {
                if let Err(err) = self.send_heartbeat(now).await {
                    tracing::warn!("device heartbeat: {err:#}");
                }
            }
        }
        Ok(())
    }

    /// Sends what is queued, within [`Timing::stop_budget`], and saves the state before
    /// exiting. A flush cut short, here or by the stop request, loses nothing still queued;
    /// lines it was sending count as maybe delivered, to be looked for in the thread.
    async fn stop(&mut self) -> Result<()> {
        tracing::info!("stopping");
        self.note_cut_sends();
        let budget = self.timing.stop_budget;
        let now = (self.clock)();
        if tokio::time::timeout(budget, self.flush(now)).await.is_err() {
            tracing::warn!(
                "stopping: sending the queued wake-ups took over {}s; the rest wait for the next start",
                budget.as_secs()
            );
            self.note_cut_sends();
        }
        self.save()
    }

    /// Logs the sends a cut-short flush left unsettled: they may have been delivered, so
    /// each stays recorded as unsure ([`Daemon::mark_unsure`]) for a flush to look for.
    fn note_cut_sends(&mut self) {
        for agent in std::mem::take(&mut self.sending) {
            tracing::warn!(
                "waking {agent}: stopped while sending; T3 may have delivered it, so it is looked for in the thread unless its lines were sent twice already"
            );
        }
    }

    async fn reconcile_logged(&mut self) {
        let budget = self.timing.reconcile_budget;
        let now = (self.clock)();
        match tokio::time::timeout(budget, self.reconcile(now)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!("reconcile: {err:#}"),
            Err(_) => tracing::warn!(
                "reconcile: still running after {}s; stopped until the next tick",
                budget.as_secs()
            ),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct ClaimedExecution {
    deferred_merge: bool,
    deferred_review: bool,
    deferred_housekeeping: bool,
    tcc_backed_off: bool,
    stop_pull_tick: bool,
}

enum ReviewPrep {
    Ready(
        Option<crate::t3::NewWorktree>,
        Option<String>,
        Option<Box<crate::gh::PullRequest>>,
    ),
    WaitForPr(Timestamp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MergeBackoffEntry {
    until: Timestamp,
    observed_at: Timestamp,
}

/// Per-task fallback merge backoff tracker for talking to older servers that do not enforce
/// server-side per-task merge cooldowns.
///
/// Only engaged when a server re-offers the exact same deferred/unclear/failed merge task
/// within a single reconcile tick. Clears an entry ONLY on events naming that exact task
/// (`TaskDone`, or `NoteAdded`/`TaskUpdated` strictly newer than `observed_at`), never on
/// unrelated board events (`agent-board-699`).
#[derive(Debug, Default)]
struct MergeBackoff {
    waits: BTreeMap<TaskId, MergeBackoffEntry>,
}

impl MergeBackoff {
    fn record(&mut self, task: TaskId, until: Timestamp, observed_at: Timestamp) {
        self.waits
            .insert(task, MergeBackoffEntry { until, observed_at });
    }

    fn has_active_wait(&mut self, now: Timestamp) -> bool {
        self.waits.retain(|_, entry| now < entry.until);
        !self.waits.is_empty()
    }

    /// Returns `true` when a backed-off task's entry was cleared by `event` on that exact
    /// task, so the caller should reconcile promptly.
    fn on_event(&mut self, event: &Event) -> bool {
        let Some(task) = event.task.as_ref() else {
            return false;
        };
        let Some(entry) = self.waits.get(task) else {
            return false;
        };
        let clear = match event.event_type {
            EventType::TaskDone => true,
            EventType::NoteAdded | EventType::TaskUpdated => event.at > entry.observed_at,
            _ => false,
        };
        if clear {
            self.waits.remove(task);
        }
        clear
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReviewWaitEntry {
    until: Timestamp,
    observed_at: Timestamp,
}

/// Per-task review-wait backoff tracker. Holds `RunKind::Review` pull claims while a
/// parked task is still inside its `REVIEW_PR_WAIT` window and clears a task's wait ONLY
/// when that exact task receives `TaskDone` or a `NoteAdded`/`TaskUpdated` strictly newer
/// than the server timestamp already inspected (`observed_at`), or once `until` passes.
#[derive(Debug, Default)]
struct ReviewWaitBackoff {
    waits: BTreeMap<TaskId, ReviewWaitEntry>,
}

impl ReviewWaitBackoff {
    fn record(&mut self, task: TaskId, until: Timestamp, observed_at: Timestamp) {
        self.waits
            .insert(task, ReviewWaitEntry { until, observed_at });
    }

    fn has_active_wait(&mut self, now: Timestamp) -> bool {
        self.waits.retain(|_, entry| now < entry.until);
        !self.waits.is_empty()
    }

    /// Returns `true` when a waiting task's entry was cleared by `event` (so the caller
    /// should reconcile promptly, e.g. right after a PR note lands on the waiting task or
    /// the task leaves `review`).
    fn on_event(&mut self, event: &Event) -> bool {
        let Some(task) = event.task.as_ref() else {
            return false;
        };
        let Some(entry) = self.waits.get(task) else {
            return false;
        };
        let clear = match event.event_type {
            EventType::TaskDone => true,
            EventType::NoteAdded
            | EventType::TaskUpdated
            | EventType::TaskClaimed
            | EventType::TaskTaken
            | EventType::TaskReleased => event.at > entry.observed_at,
            _ => false,
        };
        if clear {
            self.waits.remove(task);
        }
        clear
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LlmSpawnKey {
    kind: RunKind,
    board: BoardSlug,
    task_id: Option<TaskId>,
    runner: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LlmSpawnFailEntry {
    until: Timestamp,
    attempts: u32,
    observed_at: Timestamp,
    task_stamp: Option<(Timestamp, i64)>,
    /// Engaged only when an older server without server-side `SpawnBackoff` re-offers the same
    /// unchanged target inside its backoff window. Never engaged against up-to-date servers.
    hold_kind: bool,
}

/// Daemon-side fallback tracker for pull-dispatched LLM fast spawn failures (`agent-board-818`).
///
/// Mirrors the server's `30s -> 60s -> 120s -> 300s` exponential backoff so talking to an older
/// server that lacks server-side `SpawnBackoff` cannot tight-loop failed spawns. Against a
/// server with server-side `SpawnBackoff`, `hold_kind` stays `false` so unrelated tasks of the
/// same kind experience zero head-of-line blocking.
#[derive(Debug, Default)]
struct LlmSpawnBackoff {
    entries: std::collections::HashMap<LlmSpawnKey, LlmSpawnFailEntry>,
}

impl LlmSpawnBackoff {
    fn key_for(claimed: &ClaimedRun) -> LlmSpawnKey {
        let task_id = match claimed.run.kind {
            RunKind::Worker | RunKind::Review | RunKind::Verdict | RunKind::Triage => {
                claimed.task_id.clone()
            }
            _ => None,
        };
        LlmSpawnKey {
            kind: claimed.run.kind,
            board: claimed.board.clone(),
            task_id,
            runner: claimed.run.runner.clone(),
        }
    }

    fn record_failure(
        &mut self,
        claimed: &ClaimedRun,
        task_stamp: Option<(Timestamp, i64)>,
        now: Timestamp,
    ) -> (TimeDelta, Timestamp, u32) {
        let key = Self::key_for(claimed);
        let attempts = self
            .entries
            .get(&key)
            .filter(|e| now < e.until + LLM_SPAWN_BACKOFF_STREAK_RESET)
            .map_or(1, |e| e.attempts.saturating_add(1));
        let idx = usize::try_from(attempts.saturating_sub(1))
            .unwrap_or(LLM_SPAWN_BACKOFF_SECS.len() - 1)
            .min(LLM_SPAWN_BACKOFF_SECS.len() - 1);
        let delay = TimeDelta::seconds(LLM_SPAWN_BACKOFF_SECS[idx]);
        let until = now + delay;
        self.entries.insert(
            key,
            LlmSpawnFailEntry {
                until,
                attempts,
                observed_at: now,
                task_stamp,
                hold_kind: false,
            },
        );
        (delay, until, attempts)
    }

    fn duplicate_in_backoff(
        &mut self,
        claimed: &ClaimedRun,
        current_stamp: Option<(Timestamp, i64)>,
        now: Timestamp,
    ) -> Option<(Timestamp, u32)> {
        let key = Self::key_for(claimed);
        let unchanged = {
            let entry = self.entries.get(&key)?;
            if now >= entry.until {
                return None;
            }
            match (entry.task_stamp, current_stamp) {
                (Some(prev), Some(curr)) => prev == curr,
                _ => true,
            }
        };
        if !unchanged {
            self.entries.remove(&key);
            return None;
        }
        let entry = self.entries.get_mut(&key)?;
        entry.hold_kind = true;
        Some((entry.until, entry.attempts))
    }

    fn is_kind_held(&mut self, kind: RunKind, now: Timestamp) -> bool {
        let mut held = false;
        for (key, entry) in &mut self.entries {
            if now >= entry.until {
                entry.hold_kind = false;
            } else if key.kind == kind && entry.hold_kind {
                held = true;
            }
        }
        held
    }

    fn clear_runner(&mut self, runner: &str) {
        self.entries.retain(|key, _| key.runner != runner);
    }

    fn on_event(&mut self, event: &Event) -> bool {
        let mut woke_held = false;
        self.entries.retain(|key, entry| {
            let clear = if let Some(ref tid) = key.task_id
                && event.task.as_ref() == Some(tid)
            {
                match event.event_type {
                    EventType::TaskDone => true,
                    EventType::NoteAdded
                    | EventType::TaskUpdated
                    | EventType::TaskClaimed
                    | EventType::TaskTaken
                    | EventType::TaskReleased => event.at > entry.observed_at,
                    _ => false,
                }
            } else if key.kind == RunKind::Goal
                && event.board.as_ref() == Some(&key.board)
                && event.event_type == EventType::TaskDone
            {
                event.at > entry.observed_at
            } else {
                false
            };
            if clear {
                woke_held |= entry.hold_kind;
                false
            } else {
                true
            }
        });
        woke_held
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TccBackoffEntry {
    until: Timestamp,
    delay: TimeDelta,
    attempts: u32,
    project: PathBuf,
    error: String,
}

/// Per-repo and per-project-path backoff tracker for macOS TCC privacy refusals
/// (`agent-board-778`).
///
/// When a local git/worktree operation hits a macOS TCC privacy refusal on a protected folder,
/// the affected repo keys and checkout path are excluded from `POST /v1/dispatch/claim` and
/// device heartbeats on this device for an exponentially growing window
/// ([`TCC_REPO_BACKOFF_FIRST`] to [`TCC_REPO_BACKOFF_MAX`]) so peer runners claim the work
/// cleanly and reconcile does not stall. Cleared immediately on `[projects]`/`[board_projects]`
/// config reload or after a successful git/worktree operation on that repo/path.
#[derive(Debug, Default)]
struct TccRepoBackoff {
    by_repo: BTreeMap<String, TccBackoffEntry>,
    by_path: BTreeMap<PathBuf, TccBackoffEntry>,
}

impl TccRepoBackoff {
    /// Computes the next `(delay, until, attempts)` without mutating state so the caller can
    /// persist the Trail Rule record before activating the backoff.
    fn next_schedule(
        &self,
        repos: &[String],
        project: &Path,
        now: Timestamp,
    ) -> (TimeDelta, Timestamp, u32) {
        let prev = self
            .by_path
            .get(project)
            .into_iter()
            .chain(repos.iter().filter_map(|r| self.by_repo.get(r)))
            // Reset backoff history if the previous window expired long enough ago.
            .filter(|e| now < e.until + TCC_REPO_BACKOFF_MAX)
            .max_by_key(|e| (e.attempts, e.delay));
        match prev {
            Some(prev) => {
                let delay = (prev.delay + prev.delay).min(TCC_REPO_BACKOFF_MAX);
                let attempts = prev.attempts.saturating_add(1);
                (delay, now + delay, attempts)
            }
            None => (TCC_REPO_BACKOFF_FIRST, now + TCC_REPO_BACKOFF_FIRST, 1),
        }
    }

    fn record(
        &mut self,
        repos: &[String],
        project: PathBuf,
        delay: TimeDelta,
        until: Timestamp,
        attempts: u32,
        error: String,
    ) {
        let entry = TccBackoffEntry {
            until,
            delay,
            attempts,
            project: project.clone(),
            error,
        };
        self.by_path.insert(project, entry.clone());
        for repo in repos {
            self.by_repo.insert(repo.clone(), entry.clone());
        }
    }

    fn is_repo_backed_off(&self, repo: &str, now: Timestamp) -> bool {
        self.by_repo.get(repo).is_some_and(|e| now < e.until)
    }

    fn is_path_backed_off(&self, project: &Path, now: Timestamp) -> bool {
        self.by_path.get(project).is_some_and(|e| now < e.until)
    }

    fn active_for(
        &self,
        repos: &[String],
        project: &Path,
        now: Timestamp,
    ) -> Option<&TccBackoffEntry> {
        self.by_path
            .get(project)
            .into_iter()
            .chain(repos.iter().filter_map(|r| self.by_repo.get(r)))
            .filter(|e| now < e.until)
            .max_by_key(|e| e.until)
    }

    fn filter_projects(&self, projects: &BTreeMap<String, PathBuf>, now: Timestamp) -> Vec<String> {
        let mut repos = BTreeSet::new();
        for (key, path) in projects {
            if let Some(norm) = normalize_repo(key)
                && !self.is_repo_backed_off(&norm, now)
                && !self.is_path_backed_off(path, now)
            {
                repos.insert(norm);
            }
        }
        repos.into_iter().collect()
    }

    fn clear_for(&mut self, repos: &[String], project: &Path) {
        self.by_path.remove(project);
        for repo in repos {
            self.by_repo.remove(repo);
        }
    }

    fn clear(&mut self) {
        self.by_repo.clear();
        self.by_path.clear();
    }
}

pub(super) fn repo_keys_for_run(
    repo_key: Option<&str>,
    board: &Board,
    task: Option<&Task>,
) -> Vec<String> {
    let mut out = BTreeSet::new();
    if let Some(key) = repo_key.and_then(normalize_repo) {
        out.insert(key);
        return out.into_iter().collect();
    }
    if let Some(key) = task
        .and_then(|t| t.repo.as_deref())
        .and_then(normalize_repo)
    {
        out.insert(key);
    }
    for repo in &board.repos {
        if let Some(key) = normalize_repo(repo) {
            out.insert(key);
        }
    }
    out.into_iter().collect()
}

fn project_for_task_config(
    config: &Config,
    board: &Board,
    task: Option<&Task>,
) -> Option<(Option<String>, PathBuf)> {
    if let Some(task_repo) = task.and_then(|t| t.repo.as_deref())
        && let Some(found) = lookup_project_repo(config, task_repo)
    {
        return Some(found);
    }
    for repo in &board.repos {
        if let Some(found) = lookup_project_repo(config, repo) {
            return Some(found);
        }
    }
    config
        .board_projects
        .get(&board.slug)
        .map(|path| (None, path.clone()))
}

fn run_kind_str(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Worker => "worker",
        RunKind::Review => "review",
        RunKind::Verdict => "verdict",
        RunKind::Triage => "triage",
        RunKind::Goal => "goal",
        RunKind::Curate => "curate",
        RunKind::Merge => "merge",
        RunKind::Housekeeping => "housekeeping",
        RunKind::Command => "command",
        RunKind::Interactive => "interactive",
        RunKind::Unknown => "unknown",
    }
}

/// Classifies the reasoning-effort tier for a `RunKind::Review` judge run.
///
/// Conservative policy:
/// - `after_merge` verification reviews -> [`crate::config::ReviewEffort::Medium`].
/// - Non-empty, non-truncated diffs where every changed file is a doc or prompt file ->
///   [`crate::config::ReviewEffort::Medium`].
/// - Security-sensitive diffs (sensitive code/config/CI/deploy/auth/crypto/guard paths, or
///   security-sensitive task metadata when the diff is not docs/prompt-only) ->
///   [`crate::config::ReviewEffort::High`] (overrides `xs`).
/// - Non-security `TaskSize::Xs` PRs -> [`crate::config::ReviewEffort::Medium`].
/// - Substantive code PRs (`s`, `m`, `l`, `unknown`, or unestimated `None`) ->
///   [`crate::config::ReviewEffort::High`].
pub(crate) fn classify_review_effort(
    task: &Task,
    after_merge: bool,
    changed_files: &[String],
    files_truncated: bool,
) -> crate::config::ReviewEffort {
    use crate::config::ReviewEffort;
    use board_proto::TaskSize;

    if after_merge {
        return ReviewEffort::Medium;
    }
    let docs_or_prompts_only = !files_truncated
        && !changed_files.is_empty()
        && changed_files.iter().all(|p| is_doc_or_prompt_path(p));
    if docs_or_prompts_only {
        return ReviewEffort::Medium;
    }
    if is_security_sensitive_review(task, changed_files) {
        return ReviewEffort::High;
    }
    match task.size {
        Some(TaskSize::Xs) => ReviewEffort::Medium,
        Some(TaskSize::S | TaskSize::M | TaskSize::L | TaskSize::Unknown) | None => {
            ReviewEffort::High
        }
    }
}

/// Returns `true` when `path` is a non-executable documentation or prompt file.
fn is_doc_or_prompt_path(raw: &str) -> bool {
    let norm = raw.replace('\\', "/");
    let norm = norm.trim_start_matches("./").trim();
    if norm.is_empty() {
        return false;
    }
    let lower = norm.to_ascii_lowercase();
    let basename = lower.rsplit('/').next().unwrap_or(&lower);
    let in_doc_or_prompt_dir = lower.starts_with("docs/")
        || lower.starts_with("doc/")
        || lower.starts_with("prompts/")
        || lower.starts_with("prompt/")
        || lower.contains("/docs/")
        || lower.contains("/prompts/");

    if let Some((_, ext)) = basename.rsplit_once('.') {
        match ext {
            "md" | "mdx" | "markdown" | "rst" | "adoc" | "asciidoc" | "prompt" => true,
            "txt" => {
                // Exclude build/dependency manifests such as `requirements.txt` or `CMakeLists.txt`.
                let is_manifest = basename == "cmakelists.txt"
                    || basename.starts_with("requirements")
                    || basename.starts_with("constraints");
                !is_manifest
                    && (in_doc_or_prompt_dir
                        || matches!(
                            basename,
                            "readme.txt"
                                | "license.txt"
                                | "licence.txt"
                                | "changelog.txt"
                                | "notice.txt"
                                | "authors.txt"
                                | "copying.txt"
                                | "notes.txt"
                        ))
            }
            _ => false,
        }
    } else {
        matches!(
            basename,
            "readme"
                | "license"
                | "licence"
                | "changelog"
                | "changes"
                | "history"
                | "notice"
                | "authors"
                | "contributors"
                | "copying"
        )
    }
}

fn is_security_sensitive_review(task: &Task, changed_files: &[String]) -> bool {
    if changed_files
        .iter()
        .any(|p| !is_doc_or_prompt_path(p) && is_security_sensitive_path(p))
    {
        return true;
    }
    is_security_sensitive_task(task)
}

fn is_security_sensitive_path(raw: &str) -> bool {
    let norm = raw.replace('\\', "/");
    let norm = norm.trim_start_matches("./").trim();
    if norm.is_empty() {
        return false;
    }
    let lower = norm.to_ascii_lowercase();
    let basename = lower.rsplit('/').next().unwrap_or(&lower);

    if lower.starts_with(".github/workflows/")
        || lower.contains("/.github/workflows/")
        || lower.starts_with("deploy/")
        || lower.contains("/deploy/")
        || basename == ".gitlab-ci.yml"
        || basename == ".env"
        || basename.starts_with(".env.")
        || basename == "dockerfile"
        || basename.starts_with("dockerfile.")
        || basename.starts_with("docker-compose")
        || matches!(
            basename,
            "compose.yml"
                | "compose.yaml"
                | "codeowners"
                | "sudoers"
                | "sshd_config"
                | "ssh_config"
                | "caddyfile"
                | "nginx.conf"
        )
    {
        return true;
    }

    lower.split(['/', '_', '-', '.']).any(|tok| {
        matches!(
            tok,
            "auth"
                | "authn"
                | "authz"
                | "oauth"
                | "oidc"
                | "jwt"
                | "token"
                | "tokens"
                | "secret"
                | "secrets"
                | "cred"
                | "creds"
                | "credential"
                | "credentials"
                | "crypto"
                | "cipher"
                | "hmac"
                | "tls"
                | "ssl"
                | "cert"
                | "certs"
                | "keychain"
                | "vault"
                | "ssh"
                | "sudo"
                | "pam"
                | "rbac"
                | "acl"
                | "perm"
                | "perms"
                | "permission"
                | "permissions"
                | "privilege"
                | "privileges"
                | "guard"
                | "guards"
                | "sandbox"
                | "security"
                | "firewall"
                | "iptables"
                | "nftables"
                | "csrf"
                | "cors"
                | "csp"
                | "xss"
                | "ssrf"
                | "sanitize"
                | "sanitizer"
                | "encrypt"
                | "encryption"
                | "decrypt"
                | "decryption"
                | "password"
                | "passwd"
                | "keystore"
                | "signing"
        )
    })
}

fn is_security_sensitive_task(task: &Task) -> bool {
    let title_tokens: Vec<String> = task
        .title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();

    let has_llm_token_context = title_tokens.iter().any(|t| {
        matches!(
            t.as_str(),
            "llm"
                | "cached"
                | "uncached"
                | "reasoning"
                | "usage"
                | "budget"
                | "prompt"
                | "context"
                | "input"
                | "output"
                | "burn"
                | "count"
                | "counts"
        )
    });

    if title_tokens.iter().any(|tok| match tok.as_str() {
        "security" | "auth" | "authn" | "authz" | "oauth" | "oidc" | "bearer" | "jwt" | "csrf"
        | "xss" | "ssrf" | "rce" | "cve" | "crypto" | "tls" | "ssl" | "ssh" | "sudo" | "secret"
        | "secrets" | "credential" | "credentials" | "keychain" | "vault" | "permission"
        | "permissions" | "privilege" | "privileges" | "sandbox" | "firewall" | "injection"
        | "traversal" | "bypass" | "vulnerability" | "vuln" | "harden" | "hardening" | "redact"
        | "redaction" | "encrypt" | "encryption" | "decrypt" | "decryption" | "hmac" | "acl"
        | "rbac" | "guard" | "guards" | "cors" | "csp" | "sanitize" | "password" | "passwd" => true,
        "token" | "tokens" => !has_llm_token_context,
        _ => false,
    }) {
        return true;
    }

    let body_lower = task.body.to_ascii_lowercase();
    [
        "cve-",
        "auth bypass",
        "privilege escalation",
        "command injection",
        "sql injection",
        "path traversal",
        "secret leak",
        "security-sensitive",
    ]
    .iter()
    .any(|needle| body_lower.contains(needle))
}

/// Sanitizes control characters rejected by server `check_text` (keeping `\n`, `\r`, `\t`)
/// and clips a run reason to the server's `RUN_REASON_CHARS_MAX` limit.
fn sanitize_run_reason(reason: &str) -> String {
    let cleaned: String = reason
        .chars()
        .map(|c| {
            if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect();
    clip_run_reason(&cleaned)
}

/// Reports a completed or deferred deterministic `Merge` poll back to `board-server`:
/// - For poll-coalesceable deferrals (`should_wait_external()`), transitions the run to
///   `waiting_external` with updated poll metadata.
/// - If the server fails the `waiting_external` PATCH or leaves conflict locks intact
///   (older server), falls back immediately to `succeeded` so `repo:<repo>:merge` and
///   deploy locks are never left held.
async fn finish_claimed_merge_run(
    client: &Client,
    run_id: &RunId,
    outcome: &MergeOutcome,
    prev_outcome: Option<&serde_json::Value>,
    had_conflict_keys: bool,
    now: Timestamp,
) {
    let outcome_json = outcome.to_outcome_json_with_poll(prev_outcome, now);
    if outcome.should_wait_external() {
        let wait_update = UpdateRun {
            state: Some(RunState::WaitingExternal),
            outcome: Some(Some(outcome_json.clone())),
            reason: outcome.deferred_reason().map(sanitize_run_reason),
            ..UpdateRun::default()
        };
        let needs_fallback = match client.update_run(run_id, &wait_update).await {
            Ok(updated) => had_conflict_keys && !updated.conflict_locks.is_empty(),
            Err(err) => {
                tracing::warn!(
                    "parking merge run {run_id} in waiting_external failed ({err:#}); falling back to succeeded"
                );
                true
            }
        };
        if needs_fallback {
            let fallback = UpdateRun {
                state: Some(RunState::Succeeded),
                outcome: Some(Some(outcome_json)),
                ..UpdateRun::default()
            };
            if let Err(err) = client.update_run(run_id, &fallback).await {
                tracing::warn!("closing fallback deferred merge run {run_id}: {err:#}");
            }
        }
    } else {
        let update = UpdateRun {
            state: Some(RunState::Succeeded),
            outcome: Some(Some(outcome_json)),
            ..UpdateRun::default()
        };
        if let Err(err) = client.update_run(run_id, &update).await {
            tracing::warn!("marking merge run {run_id} succeeded: {err:#}");
        }
    }
}

fn clip_run_reason(reason: &str) -> String {
    const MAX: usize = 500;
    if reason.chars().count() <= MAX {
        return reason.to_owned();
    }
    let mut cut: String = reason.chars().take(MAX - 1).collect();
    cut.push('…');
    cut
}

fn map_runner_event_kind(kind: RunEventKind) -> ProtoRunEventKind {
    match kind {
        RunEventKind::Phase => ProtoRunEventKind::Phase,
        RunEventKind::Tool => ProtoRunEventKind::Tool,
        RunEventKind::Note => ProtoRunEventKind::Note,
        RunEventKind::Message => ProtoRunEventKind::Message,
        RunEventKind::Pr => ProtoRunEventKind::Pr,
        RunEventKind::Error => ProtoRunEventKind::Error,
        RunEventKind::Cancelled => ProtoRunEventKind::Cancelled,
        RunEventKind::Done => ProtoRunEventKind::Done,
        RunEventKind::Unknown => ProtoRunEventKind::Note,
    }
}

fn valid_transcript_uri(uri: &str) -> bool {
    let trimmed = uri.trim();
    !trimmed.is_empty()
        && trimmed == uri
        && trimmed.chars().count() <= TRANSCRIPT_URI_MAX_CHARS
        && !trimmed.chars().any(char::is_control)
}

fn archive_retry_delay(attempts: u32) -> TimeDelta {
    let shift = attempts.min(8);
    let secs = (ARCHIVE_RETRY_FIRST_SECS.saturating_mul(1i64 << shift)).min(ARCHIVE_RETRY_MAX_SECS);
    TimeDelta::seconds(secs)
}

fn compute_file_sha256(path: &Path) -> Result<String> {
    use std::io::Read as _;
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65_536];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(crate::config::hex(&hasher.finalize()))
}

async fn run_archive_command(cfg: &ArchiveConfig, job: &TranscriptArchiveJob) -> Result<String> {
    let board_seg = job
        .board
        .as_ref()
        .map(BoardSlug::as_str)
        .unwrap_or("unassigned");
    let default_uri = cfg.uri_prefix.as_deref().map(|prefix| {
        let trimmed = prefix.trim_end_matches('/');
        format!("{trimmed}/{board_seg}/{}.jsonl", job.run_id)
    });
    let Some(template) = cfg.command.as_ref().filter(|c| !c.is_empty()) else {
        anyhow::bail!(
            "[archive] enabled = true requires a non-empty `command` template in config.toml"
        );
    };
    let file_str = job.file.to_string_lossy();
    let uri_placeholder = default_uri.as_deref().unwrap_or("");
    let substitute = |part: &str| -> String {
        part.replace("{file}", &file_str)
            .replace("{run_id}", job.run_id.as_str())
            .replace("{board}", board_seg)
            .replace("{uri}", uri_placeholder)
    };
    let program = substitute(&template[0]);
    let args: Vec<String> = template[1..].iter().map(|a| substitute(a)).collect();
    let output = tokio::time::timeout(
        ARCHIVE_CMD_TIMEOUT,
        tokio::process::Command::new(&program)
            .args(&args)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "archive command {:?} timed out after {}s",
            program,
            ARCHIVE_CMD_TIMEOUT.as_secs()
        )
    })?
    .with_context(|| format!("spawning archive command {program:?}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let snippet: String = stderr.trim().chars().take(300).collect();
        anyhow::bail!(
            "archive command {:?} exited with {}: {}",
            program,
            output.status,
            snippet
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let printed_uri = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned);
    printed_uri.or(default_uri).ok_or_else(|| {
        anyhow::anyhow!(
            "archive command {:?} printed no URI on stdout and [archive] uri_prefix is not set",
            program
        )
    })
}

fn health_supports_dispatch_claim(health: &serde_json::Value) -> bool {
    health["features"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some("dispatch-claim-v1")))
}

fn lookup_project_repo(config: &Config, repo: &str) -> Option<(Option<String>, PathBuf)> {
    if let Some(path) = config.projects.get(repo) {
        return Some((Some(repo.to_owned()), path.clone()));
    }
    let target = normalize_repo(repo)?;
    config.projects.iter().find_map(|(key, path)| {
        normalize_repo(key)
            .filter(|norm| norm == &target)
            .map(|norm| (Some(norm), path.clone()))
    })
}

fn format_sticky_wake_message(wake: &StickyWake) -> String {
    if wake.kind == "answered" {
        let ask_id = wake.payload["ask_id"].as_i64().unwrap_or(0);
        let about = wake.payload["task_id"]
            .as_str()
            .map(|t| format!(" about {t}"))
            .unwrap_or_default();
        let raw_answer = wake.payload["answer"].as_str().unwrap_or_default();
        let answer = serde_json::to_string(raw_answer)
            .unwrap_or_else(|_| format!("{raw_answer:?}"))
            .replace('<', "\\u003c")
            .replace('>', "\\u003e");
        return format!("the user answered ask #{ask_id}{about}: {answer}\n{WAKE_END}");
    }
    if let Some(text) = wake
        .payload
        .get("message")
        .or_else(|| wake.payload.get("text"))
        .and_then(|v| v.as_str())
    {
        return format!("{text}\n{WAKE_END}");
    }
    format!(
        "sticky wake #{} ({}): {}\n{WAKE_END}",
        wake.id, wake.kind, wake.payload
    )
}

/// The body of an event stream, ending in an error once no bytes (events or keep-alives)
/// came for `idle`: a half-open connection then reconnects instead of waiting forever.
fn idle_limited(
    resp: reqwest::Response,
    idle: Duration,
) -> impl futures_util::Stream<Item = std::io::Result<impl AsRef<[u8]>>> {
    let body = Box::pin(resp.bytes_stream());
    futures_util::stream::unfold(Some(body), move |body| async move {
        let mut body = body?;
        match tokio::time::timeout(idle, body.next()).await {
            Ok(Some(Ok(bytes))) => Some((Ok(bytes), Some(body))),
            Ok(Some(Err(err))) => Some((Err(std::io::Error::other(err)), Some(body))),
            Ok(None) => None,
            Err(_) => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("nothing received for {}s", idle.as_secs_f32()),
                )),
                None,
            )),
        }
    })
}

/// The order ready tasks start in: the highest priority first, inside it the ranked tasks by
/// rank, then the rest, oldest first.
pub(crate) fn start_order(task: &Task) -> (board_proto::Priority, bool, Option<u32>, Timestamp) {
    (
        task.priority,
        task.rank.is_none(),
        task.rank,
        task.created_at,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use board_proto::ApiError;
    use reqwest::StatusCode;

    fn server_err(status: StatusCode, code: ErrorCode) -> anyhow::Error {
        ServerError {
            status,
            error: ApiError {
                code,
                message: "boom".into(),
                next: None,
            },
        }
        .into()
    }

    #[test]
    fn is_transient_classifies_network_and_5xx_only_not_local_parse_or_logic_errors() {
        // Transient server/proxy/transport errors.
        assert!(is_transient(&server_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal
        )));
        assert!(is_transient(&server_err(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal
        )));
        assert!(is_transient(&server_err(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::Internal
        )));
        assert!(is_transient(&server_err(
            StatusCode::REQUEST_TIMEOUT,
            ErrorCode::Internal
        )));
        assert!(is_transient(&anyhow::Error::from(TimedOut(
            "t3 send".into()
        ))));
        assert!(is_transient(&anyhow::Error::from(
            RelayFailure::Unreachable
        )));
        assert!(is_transient(&anyhow::Error::from(RelayFailure::Slow)));
        assert!(is_transient(&anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset"
        ))));
        assert!(is_transient(&anyhow::anyhow!(
            "server answered 502 Bad Gateway: <html>"
        )));
        assert!(is_transient(
            &server_err(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Internal)
                .context("reading ask #1")
        ));

        // Non-transient: 4xx client errors, local JSON parse errors, logic errors, refused relay.
        assert!(!is_transient(&server_err(
            StatusCode::BAD_REQUEST,
            ErrorCode::Invalid
        )));
        assert!(!is_transient(&server_err(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound
        )));
        let json_err: anyhow::Error = serde_json::from_str::<serde_json::Value>("{bad")
            .unwrap_err()
            .into();
        assert!(!is_transient(
            &json_err.context("unexpected answer from the server")
        ));
        assert!(!is_transient(&anyhow::anyhow!("invalid task id")));
        assert!(!is_transient(&anyhow::Error::from(RelayFailure::Refused)));
        assert!(!is_transient(&anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "corrupt"
        ))));
    }

    #[test]
    fn is_heartbeat_unsupported_matches_json_and_plain_404_or_405() {
        assert!(is_heartbeat_unsupported(&server_err(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound
        )));
        assert!(is_heartbeat_unsupported(&server_err(
            StatusCode::METHOD_NOT_ALLOWED,
            ErrorCode::Invalid
        )));
        assert!(is_heartbeat_unsupported(&anyhow::anyhow!(
            "server answered 404 Not Found: "
        )));
        assert!(is_heartbeat_unsupported(&anyhow::anyhow!(
            "server answered 405 Method Not Allowed: "
        )));
        assert!(!is_heartbeat_unsupported(&server_err(
            StatusCode::FORBIDDEN,
            ErrorCode::Forbidden
        )));
        assert!(!is_heartbeat_unsupported(&server_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal
        )));
    }

    #[test]
    fn review_wait_backoff_ignores_unrelated_and_stale_events_until_target_updates_or_deadline() {
        let mut backoff = ReviewWaitBackoff::default();
        let t0: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let until = t0 + REVIEW_PR_WAIT;
        let waiting_task: TaskId = "agent-board-10".parse().unwrap();
        let other_task: TaskId = "agent-board-11".parse().unwrap();

        let make_event = |event_type: EventType, task: Option<TaskId>, at: Timestamp| Event {
            seq: 1,
            at,
            board: Some("agent-board".parse().unwrap()),
            task,
            actor: "t3:worker".to_owned(),
            event_type,
            payload: serde_json::json!({}),
        };

        backoff.record(waiting_task.clone(), until, t0);
        assert!(backoff.has_active_wait(t0 + TimeDelta::seconds(5)));

        // Events on UNRELATED tasks must never clear the wait (prevents spin-claim-cancel loop).
        for kind in [
            EventType::TaskUpdated,
            EventType::NoteAdded,
            EventType::TaskDone,
            EventType::TaskClaimed,
            EventType::TaskTaken,
            EventType::TaskReleased,
        ] {
            let ev = make_event(kind, Some(other_task.clone()), t0 + TimeDelta::seconds(10));
            assert!(!backoff.on_event(&ev));
            assert!(backoff.has_active_wait(t0 + TimeDelta::seconds(10)));
        }

        // Stale/lagged SSE event on the SAME task (at <= observed_at) must not clear the wait.
        let stale_update = make_event(EventType::TaskUpdated, Some(waiting_task.clone()), t0);
        assert!(!backoff.on_event(&stale_update));
        assert!(backoff.has_active_wait(t0 + TimeDelta::seconds(15)));

        // Non-update events (like RunUpdated from our own cancel) must not clear the wait.
        let run_cancel = make_event(
            EventType::RunUpdated,
            Some(waiting_task.clone()),
            t0 + TimeDelta::seconds(16),
        );
        assert!(!backoff.on_event(&run_cancel));
        assert!(backoff.has_active_wait(t0 + TimeDelta::seconds(16)));

        // A NEWER NoteAdded on the waiting task (e.g. PR URL posted) clears it and requests reconcile.
        let pr_note = make_event(
            EventType::NoteAdded,
            Some(waiting_task.clone()),
            t0 + TimeDelta::seconds(20),
        );
        assert!(backoff.on_event(&pr_note));
        assert!(!backoff.has_active_wait(t0 + TimeDelta::seconds(20)));

        // Claiming/taking/releasing the waiting task out of review clears it immediately.
        for kind in [
            EventType::TaskClaimed,
            EventType::TaskTaken,
            EventType::TaskReleased,
        ] {
            backoff.record(waiting_task.clone(), until, t0 + TimeDelta::seconds(20));
            let ev = make_event(
                kind,
                Some(waiting_task.clone()),
                t0 + TimeDelta::seconds(25),
            );
            assert!(backoff.on_event(&ev));
            assert!(!backoff.has_active_wait(t0 + TimeDelta::seconds(25)));
        }

        // Re-record wait and verify deadline expiry clears it once `now >= until`.
        backoff.record(waiting_task.clone(), until, t0 + TimeDelta::seconds(20));
        assert!(backoff.has_active_wait(until - TimeDelta::seconds(1)));
        assert!(!backoff.has_active_wait(until));
    }

    type UsageStubResponses = BTreeMap<(String, String), (StatusCode, serde_json::Value)>;

    struct UsageStubServer {
        addr: std::net::SocketAddr,
        requests: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
        bodies: std::sync::Arc<std::sync::Mutex<Vec<(String, String, serde_json::Value)>>>,
        responses: std::sync::Arc<std::sync::Mutex<UsageStubResponses>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl UsageStubServer {
        async fn start() -> Self {
            use axum::{Router, extract::State, routing::any};
            use std::sync::{Arc, Mutex};

            #[derive(Clone, Default)]
            struct StubState {
                requests: Arc<Mutex<Vec<(String, String)>>>,
                bodies: Arc<Mutex<Vec<(String, String, serde_json::Value)>>>,
                responses: Arc<Mutex<UsageStubResponses>>,
            }

            async fn handler(
                State(state): State<StubState>,
                req: axum::extract::Request,
            ) -> (StatusCode, axum::Json<serde_json::Value>) {
                let key = (
                    req.method().as_str().to_owned(),
                    req.uri().path().to_owned(),
                );
                let bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
                    .await
                    .unwrap_or_default();
                let parsed_body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                state.requests.lock().unwrap().push(key.clone());
                state
                    .bodies
                    .lock()
                    .unwrap()
                    .push((key.0.clone(), key.1.clone(), parsed_body));
                if let Some((status, body)) = state.responses.lock().unwrap().get(&key).cloned() {
                    (status, axum::Json(body))
                } else {
                    (
                        StatusCode::NOT_FOUND,
                        axum::Json(
                            serde_json::json!({ "code": "not_found", "message": "missing" }),
                        ),
                    )
                }
            }

            let state = StubState::default();
            let app = Router::new()
                .route("/{*path}", any(handler))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self {
                addr,
                requests: state.requests,
                bodies: state.bodies,
                responses: state.responses,
                task,
            }
        }

        fn make_daemon(&self, home: &Home, mut cfg: Config, source: UsageSource) -> Daemon<T3Cli> {
            cfg.server = Some(format!("http://{}", self.addr));
            cfg.device = Some("test-mac".parse().unwrap());
            let client = Client::new(
                cfg.server.as_deref().unwrap(),
                "test-token".to_owned(),
                None,
                None,
            )
            .unwrap();
            let ctx = Ctx {
                home: home.clone(),
                config: cfg,
                device: "test-mac".parse().unwrap(),
                repo: crate::identity::RepoInfo::default(),
                known: None,
                client,
            };
            let mut daemon = Daemon::new(ctx, T3Cli::locate(None));
            daemon.usage_source = source;
            daemon
        }
    }

    impl Drop for UsageStubServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn refresh_usage_routes_by_source_avoids_fallback_follows_trap_and_downgrades_on_404() {
        let srv = UsageStubServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let antigravity_cache = dir.path().join("antigravity.json");
        let claude_cache = dir.path().join("claudeAgent.json");

        // 1. Antigravity daemon with no antigravity/default reading yet falls back to
        //    anthropic/default on poll, sets usage_from_fallback=true, and then STILL accepts
        //    a genuine lower-usage Antigravity local snapshot (not blocked by follows()).
        srv.responses.lock().unwrap().insert(
            ("GET".to_owned(), "/v1/usage/buckets".to_owned()),
            (
                StatusCode::OK,
                serde_json::json!([{
                    "provider": "anthropic",
                    "account": "default",
                    "five_hour": { "percent": 85.0, "resets_at": "2026-11-24T16:00:00Z" },
                    "seven_day": { "percent": 60.0, "resets_at": "2026-11-29T12:00:00Z" },
                    "read_at": "2026-11-24T11:59:00Z",
                    "device": "other-mac",
                    "reserve": 5.0
                }]),
            ),
        );
        srv.responses.lock().unwrap().insert(
            ("PUT".to_owned(), "/v1/usage/antigravity/default".to_owned()),
            (
                StatusCode::OK,
                serde_json::json!({
                    "provider": "antigravity",
                    "account": "default",
                    "five_hour": { "percent": 10.0, "resets_at": "2026-11-24T15:00:00Z" },
                    "seven_day": { "percent": 20.0, "resets_at": "2026-11-27T12:00:00Z" },
                    "read_at": "2026-11-24T11:59:30Z",
                    "device": "test-mac",
                    "reserve": 5.0
                }),
            ),
        );

        let ag_cfg = Config {
            thread_model: Some(crate::config::ThreadModel {
                provider: "antigravity".to_owned(),
                model: "gemini-4-neon".to_owned(),
                options: BTreeMap::new(),
            }),
            ..Config::default()
        };
        let mut daemon = srv.make_daemon(
            &home,
            ag_cfg.clone(),
            UsageSource::Antigravity {
                cli_bin: None,
                snapshots: vec![antigravity_cache.clone()],
            },
        );
        daemon.refresh_usage(now).await;
        assert!(daemon.usage_from_fallback);
        assert_eq!(
            daemon.usage.as_ref().unwrap().five_hour.unwrap().percent,
            85.0
        );

        // Now write a lower-usage local Antigravity snapshot (10% / 20%, earlier reset).
        // Because usage_from_fallback is true, follows() must NOT reject it!
        std::fs::write(
            &antigravity_cache,
            serde_json::json!({
                "driver": "antigravity",
                "instanceId": "antigravity",
                "usageLimits": {
                    "checkedAt": "2026-11-24T11:59:30Z",
                    "credentialFingerprint": "fp-test",
                    "windows": [
                        { "id": "gemini-5h", "kind": "session", "usedPercent": 10.0, "resetsAt": "2026-11-24T15:00:00Z" },
                        { "id": "gemini-weekly", "kind": "weekly", "usedPercent": 20.0, "resetsAt": "2026-11-27T12:00:00Z" }
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();
        daemon.refresh_usage(now).await;
        assert!(!daemon.usage_from_fallback);
        assert_eq!(
            daemon.usage.as_ref().unwrap().five_hour.unwrap().percent,
            10.0
        );
        assert!(
            srv.requests
                .lock()
                .unwrap()
                .contains(&("PUT".to_owned(), "/v1/usage/antigravity/default".to_owned())),
            "expected PUT /v1/usage/antigravity/default: {:?}",
            srv.requests.lock().unwrap()
        );

        // 2. A ClaudeCode usage source on a non-anthropic config must report to
        //    /v1/usage/anthropic/default, NEVER poisoning /v1/usage/antigravity/default.
        srv.requests.lock().unwrap().clear();
        srv.responses.lock().unwrap().insert(
            ("PUT".to_owned(), "/v1/usage/anthropic/default".to_owned()),
            (
                StatusCode::OK,
                serde_json::json!({
                    "provider": "anthropic",
                    "account": "default",
                    "five_hour": { "percent": 42.0, "resets_at": "2026-11-24T16:00:00Z" },
                    "read_at": "2026-11-24T11:59:40Z",
                    "device": "test-mac"
                }),
            ),
        );
        std::fs::write(
            &claude_cache,
            serde_json::json!({
                "driver": "claudeAgent",
                "auth": { "workspaceId": "ws-1" },
                "usageLimits": {
                    "checkedAt": "2026-11-24T11:59:40Z",
                    "windows": [
                        { "id": "five_hour", "usedPercent": 42.0, "resetsAt": "2026-11-24T16:00:00Z" }
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut mixed = srv.make_daemon(
            &home,
            ag_cfg.clone(),
            UsageSource::ClaudeCode {
                url: format!("http://{}/unused", srv.addr),
                credentials: crate::usage::Credentials::File(dir.path().join("missing-creds")),
                snapshots: vec![claude_cache],
            },
        );
        mixed.usage_polled = Some(now);
        mixed.refresh_usage(now).await;
        let reqs = srv.requests.lock().unwrap().clone();
        assert!(
            reqs.contains(&("PUT".to_owned(), "/v1/usage/anthropic/default".to_owned())),
            "ClaudeCode source must upload to anthropic/default: {reqs:?}"
        );
        assert!(
            !reqs.contains(&("PUT".to_owned(), "/v1/usage/antigravity/default".to_owned())),
            "ClaudeCode source must NEVER upload to antigravity/default: {reqs:?}"
        );

        // 3. 404 on /v1/usage/antigravity/default downgrades to legacy PUT /v1/usage and
        //    caches usage_buckets_unsupported = true.
        srv.requests.lock().unwrap().clear();
        srv.responses
            .lock()
            .unwrap()
            .remove(&("PUT".to_owned(), "/v1/usage/antigravity/default".to_owned()));
        srv.responses.lock().unwrap().insert(
            ("PUT".to_owned(), "/v1/usage".to_owned()),
            (
                StatusCode::OK,
                serde_json::json!({
                    "five_hour": { "percent": 10.0, "resets_at": "2026-11-24T15:00:00Z" },
                    "seven_day": { "percent": 20.0, "resets_at": "2026-11-27T12:00:00Z" },
                    "read_at": "2026-11-24T11:59:30Z",
                    "device": "test-mac"
                }),
            ),
        );
        let mut legacy_srv = srv.make_daemon(
            &home,
            ag_cfg,
            UsageSource::Antigravity {
                cli_bin: None,
                snapshots: vec![antigravity_cache],
            },
        );
        legacy_srv.refresh_usage(now).await;
        assert!(legacy_srv.usage_buckets_unsupported);
        let reqs = srv.requests.lock().unwrap().clone();
        assert!(
            reqs.contains(&("PUT".to_owned(), "/v1/usage".to_owned())),
            "404 on bucket endpoint must fall back to legacy PUT /v1/usage: {reqs:?}"
        );
    }

    #[tokio::test]
    async fn antigravity_uploads_ten_minute_old_snapshot_even_when_near_line() {
        let srv = UsageStubServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let antigravity_cache = dir.path().join("antigravity.json");

        let ag_cfg = Config {
            thread_model: Some(crate::config::ThreadModel {
                provider: "antigravity".to_owned(),
                model: "gemini-4-neon".to_owned(),
                options: BTreeMap::new(),
            }),
            ..Config::default()
        };
        let mut daemon = srv.make_daemon(
            &home,
            ag_cfg,
            UsageSource::Antigravity {
                cli_bin: None,
                snapshots: vec![antigravity_cache.clone()],
            },
        );
        // Seed a fresh known Antigravity reading that is near the daily pace line (within 2 pts),
        // so `usage_poll_every(now) == USAGE_POLL_NEAR` (1m).
        daemon.usage = Some(Usage {
            five_hour: Some(board_proto::UsageWindow {
                percent: 10.0,
                resets_at: Some(now + TimeDelta::hours(4)),
            }),
            seven_day: Some(board_proto::UsageWindow {
                percent: 14.5,
                resets_at: Some(now + TimeDelta::days(5)),
            }),
            read_at: now - TimeDelta::minutes(15),
            device: "test-mac".parse().unwrap(),
            pace: Some(board_proto::UsagePace {
                reserve: 4.0,
                day_start: now - TimeDelta::hours(2),
                anchor: 0.0,
                line: 16.0,
                projected: 15.0,
                hard_since: None,
            }),
            reserve_until: None,
            reserve_prev: None,
        });
        assert_eq!(daemon.usage_poll_every(now), USAGE_POLL_NEAR);

        // Write a newer snapshot dated 10 minutes ago (older than every + slack = 2m,
        // but well within USAGE_FRESH_MINUTES = 30m).
        let ten_min_ago = now - TimeDelta::minutes(10);
        srv.responses.lock().unwrap().insert(
            ("PUT".to_owned(), "/v1/usage/antigravity/default".to_owned()),
            (
                StatusCode::OK,
                serde_json::json!({
                    "provider": "antigravity",
                    "account": "default",
                    "five_hour": { "percent": 12.0, "resets_at": "2026-11-24T16:00:00Z" },
                    "seven_day": { "percent": 32.6, "resets_at": "2026-11-29T12:00:00Z" },
                    "read_at": ten_min_ago.to_rfc3339(),
                    "device": "test-mac",
                    "reserve": 4.0
                }),
            ),
        );
        std::fs::write(
            &antigravity_cache,
            serde_json::json!({
                "driver": "antigravity",
                "instanceId": "antigravity",
                "usageLimits": {
                    "checkedAt": ten_min_ago.to_rfc3339(),
                    "credentialFingerprint": "fp-test",
                    "windows": [
                        { "id": "gemini-5h", "kind": "session", "usedPercent": 12.0, "resetsAt": "2026-11-24T16:00:00Z" },
                        { "id": "gemini-weekly", "kind": "weekly", "usedPercent": 32.6, "resetsAt": "2026-11-29T12:00:00Z" }
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();

        daemon.refresh_usage(now).await;
        let reqs = srv.requests.lock().unwrap().clone();
        assert!(
            reqs.contains(&("PUT".to_owned(), "/v1/usage/antigravity/default".to_owned())),
            "10-minute-old Antigravity snapshot must upload even when near_line (every=1m): {reqs:?}"
        );
        assert_eq!(
            daemon.usage.as_ref().unwrap().seven_day.unwrap().percent,
            32.6
        );

        // A snapshot older than USAGE_FRESH_MINUTES (31m) must NOT be uploaded.
        srv.requests.lock().unwrap().clear();
        srv.responses.lock().unwrap().insert(
            ("GET".to_owned(), "/v1/usage/buckets".to_owned()),
            (StatusCode::OK, serde_json::json!([])),
        );
        let too_old = now + TimeDelta::minutes(35);
        std::fs::write(
            &antigravity_cache,
            serde_json::json!({
                "driver": "antigravity",
                "instanceId": "antigravity",
                "usageLimits": {
                    "checkedAt": (too_old - TimeDelta::minutes(board_proto::USAGE_FRESH_MINUTES + 1)).to_rfc3339(),
                    "credentialFingerprint": "fp-test",
                    "windows": [
                        { "id": "gemini-5h", "kind": "session", "usedPercent": 15.0, "resetsAt": "2026-11-24T16:00:00Z" },
                        { "id": "gemini-weekly", "kind": "weekly", "usedPercent": 35.0, "resetsAt": "2026-11-29T12:00:00Z" }
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();
        daemon.refresh_usage(too_old).await;
        let reqs = srv.requests.lock().unwrap().clone();
        assert!(
            !reqs.contains(&("PUT".to_owned(), "/v1/usage/antigravity/default".to_owned())),
            "snapshot older than USAGE_FRESH_MINUTES must not upload: {reqs:?}"
        );
    }

    #[tokio::test]
    async fn usage_poll_every_accelerates_after_pace_day_rollover_until_reset_or_refusal() {
        let srv = UsageStubServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let mut daemon = srv.make_daemon(&home, Config::default(), UsageSource::Server);

        let day_start: Timestamp = "2026-11-26T07:04:00Z".parse().unwrap();
        let next_day = day_start + TimeDelta::hours(24);
        let resets_at = day_start + TimeDelta::days(7);
        let read_at = next_day - TimeDelta::minutes(2);

        // Reading is above the day's line (not near) taken 2m before next_day.
        daemon.usage = Some(Usage {
            five_hour: Some(board_proto::UsageWindow {
                percent: 2.0,
                resets_at: Some(read_at + TimeDelta::hours(3)),
            }),
            seven_day: Some(board_proto::UsageWindow {
                percent: 23.4,
                resets_at: Some(resets_at),
            }),
            read_at,
            device: "test-mac".parse().unwrap(),
            pace: Some(board_proto::UsagePace {
                reserve: 5.0,
                day_start,
                anchor: 0.4,
                line: 13.9,
                projected: 23.4,
                hard_since: None,
            }),
            reserve_until: None,
            reserve_prev: None,
        });

        // Before next_day (past line, not near): normal 5m cadence.
        assert_eq!(daemon.usage_poll_every(read_at), USAGE_POLL_EVERY);

        // 1m past next_day (reading still fresh, week not reset): accelerates to 1m to anchor new day.
        let after_boundary = next_day + TimeDelta::minutes(1);
        assert_eq!(daemon.usage_poll_every(after_boundary), USAGE_POLL_NEAR);

        // After a recent 429 refusal: stays at 5m.
        daemon.usage_refused = Some(after_boundary - TimeDelta::minutes(5));
        assert_eq!(daemon.usage_poll_every(after_boundary), USAGE_POLL_EVERY);
        daemon.usage_refused = None;

        // Past weekly reset (even on a fresh reading): stays at 5m.
        daemon
            .usage
            .as_mut()
            .unwrap()
            .seven_day
            .as_mut()
            .unwrap()
            .resets_at = Some(after_boundary);
        assert_eq!(daemon.usage_poll_every(after_boundary), USAGE_POLL_EVERY);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn antigravity_cli_polls_on_cadence_and_dedupes_fresh_server_readings() {
        use std::os::unix::fs::PermissionsExt;

        let srv = UsageStubServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let now: Timestamp = "2026-11-26T07:40:00Z".parse().unwrap();
        let counter_file = dir.path().join("calls.txt");
        let fake_bin = dir.path().join("antigravity");
        std::fs::write(
            &fake_bin,
            format!(
                r#"#!/bin/sh
set -eu
printf 'call\n' >> '{}'
printf '%s\n' '{{"status":"SUCCESS","command":{{"name":"usage","data":{{"groups":[{{"buckets":[{{"id":"gemini-5h","remaining_fraction":0.9,"reset_time":"2026-11-26T12:00:00Z"}},{{"id":"gemini-weekly","remaining_fraction":0.95,"reset_time":"2026-12-03T07:00:00Z"}}]}}]}}}}}}'
"#,
                counter_file.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();

        srv.responses.lock().unwrap().insert(
            ("PUT".to_owned(), "/v1/usage/antigravity/default".to_owned()),
            (
                StatusCode::OK,
                serde_json::json!({
                    "provider": "antigravity",
                    "account": "default",
                    "five_hour": { "percent": 10.0, "resets_at": "2026-11-26T12:00:00Z" },
                    "seven_day": { "percent": 5.0, "resets_at": "2026-12-03T07:00:00Z" },
                    "read_at": now.to_rfc3339(),
                    "device": "test-mac",
                    "reserve": 5.0
                }),
            ),
        );
        srv.responses.lock().unwrap().insert(
            ("GET".to_owned(), "/v1/usage/buckets".to_owned()),
            (
                StatusCode::OK,
                serde_json::json!([{
                    "provider": "antigravity",
                    "account": "default",
                    "five_hour": { "percent": 10.0, "resets_at": "2026-11-26T12:00:00Z" },
                    "seven_day": { "percent": 5.0, "resets_at": "2026-12-03T07:00:00Z" },
                    "read_at": now.to_rfc3339(),
                    "device": "other-mac",
                    "reserve": 5.0
                }]),
            ),
        );

        let ag_cfg = Config {
            thread_model: Some(crate::config::ThreadModel {
                provider: "antigravity".to_owned(),
                model: "gemini-4-neon".to_owned(),
                options: BTreeMap::new(),
            }),
            ..Config::default()
        };
        let mut daemon = srv.make_daemon(
            &home,
            ag_cfg,
            UsageSource::Antigravity {
                cli_bin: Some(fake_bin),
                snapshots: Vec::new(),
            },
        );

        // 1. First refresh invokes the official CLI and uploads to antigravity/default.
        daemon.refresh_usage(now).await;
        assert_eq!(
            std::fs::read_to_string(&counter_file)
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert!(
            srv.requests
                .lock()
                .unwrap()
                .contains(&("PUT".to_owned(), "/v1/usage/antigravity/default".to_owned()))
        );
        assert_eq!(
            daemon.usage.as_ref().unwrap().seven_day.unwrap().percent,
            5.0
        );

        // 2. Two minutes later (< USAGE_POLL_EVERY = 5m), even if usage_polled were cleared,
        //    the fresh known reading from the server prevents a redundant CLI call.
        daemon.usage_polled = None;
        daemon.refresh_usage(now + TimeDelta::minutes(2)).await;
        assert_eq!(
            std::fs::read_to_string(&counter_file)
                .unwrap()
                .lines()
                .count(),
            1
        );

        // 3. Six minutes later (>= USAGE_POLL_EVERY = 5m), the CLI is invoked again.
        daemon.refresh_usage(now + TimeDelta::minutes(6)).await;
        assert_eq!(
            std::fs::read_to_string(&counter_file)
                .unwrap()
                .lines()
                .count(),
            2
        );

        // 4. A CLI failure sets a USAGE_POLL_EVERY backoff so the next reconcile does not hammer it.
        let fail_counter = dir.path().join("fail-calls.txt");
        let failing_bin = dir.path().join("antigravity-fail");
        std::fs::write(
            &failing_bin,
            format!(
                "#!/bin/sh\nprintf 'call\\n' >> '{}'\necho 'transient error' >&2\nexit 1\n",
                fail_counter.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&failing_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&failing_bin, perms).unwrap();
        daemon.usage_source = UsageSource::Antigravity {
            cli_bin: Some(failing_bin),
            snapshots: Vec::new(),
        };
        daemon.usage = None;
        daemon.usage_polled = None;
        daemon.usage_retry_at = None;
        let t_fail = now + TimeDelta::minutes(12);
        daemon.refresh_usage(t_fail).await;
        assert_eq!(daemon.usage_retry_at, Some(t_fail + USAGE_POLL_EVERY));
        assert_eq!(
            std::fs::read_to_string(&fail_counter)
                .unwrap()
                .lines()
                .count(),
            1
        );
        // 1 minute later (still inside backoff): must not re-invoke CLI.
        daemon.refresh_usage(t_fail + TimeDelta::minutes(1)).await;
        assert_eq!(
            std::fs::read_to_string(&fail_counter)
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn merge_backoff_scopes_resets_to_matching_task_and_newer_events() {
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let t1: TaskId = "dotfiles-355".parse().unwrap();
        let t2: TaskId = "dotfiles-356".parse().unwrap();

        let mut backoff = MergeBackoff::default();
        backoff.record(t1.clone(), now + TimeDelta::minutes(2), now);
        assert!(backoff.has_active_wait(now));

        let make_event = |task: &TaskId, event_type: EventType, at: Timestamp| Event {
            seq: 1,
            at,
            board: Some("dotfiles".parse().unwrap()),
            actor: "t3:other".to_owned(),
            event_type,
            task: Some(task.clone()),
            payload: serde_json::json!({}),
        };

        // Unrelated events on another task must NOT clear t1's backoff (`agent-board-699`).
        assert!(!backoff.on_event(&make_event(
            &t2,
            EventType::TaskUpdated,
            now + TimeDelta::seconds(5)
        )));
        assert!(!backoff.on_event(&make_event(
            &t2,
            EventType::NoteAdded,
            now + TimeDelta::seconds(5)
        )));
        assert!(!backoff.on_event(&make_event(
            &t2,
            EventType::TaskDone,
            now + TimeDelta::seconds(5)
        )));
        assert!(backoff.has_active_wait(now + TimeDelta::seconds(10)));

        // A NoteAdded at or before observed_at on t1 does NOT clear it.
        assert!(!backoff.on_event(&make_event(&t1, EventType::NoteAdded, now)));
        assert!(backoff.has_active_wait(now + TimeDelta::seconds(10)));

        // A strictly newer NoteAdded on t1 DOES clear it.
        assert!(backoff.on_event(&make_event(
            &t1,
            EventType::NoteAdded,
            now + TimeDelta::seconds(15)
        )));
        assert!(!backoff.has_active_wait(now + TimeDelta::seconds(15)));

        // Deadline expiry clears it even without an event.
        backoff.record(t1.clone(), now + TimeDelta::minutes(2), now);
        assert!(backoff.has_active_wait(now + TimeDelta::seconds(119)));
        assert!(!backoff.has_active_wait(now + TimeDelta::minutes(2)));
    }

    #[tokio::test]
    async fn renew_runs_attaches_token_usage_outcome_on_terminal_transition() {
        use crate::runner::{FakeRunner, RunnerStatus};

        let srv = UsageStubServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let now: Timestamp = "2026-11-25T03:00:00Z".parse().unwrap();

        let run_id: RunId = "run-telemetry-1".parse().unwrap();
        let board: BoardSlug = "factory".parse().unwrap();
        srv.responses.lock().unwrap().insert(
            ("PATCH".to_owned(), format!("/v1/runs/{run_id}")),
            (
                StatusCode::OK,
                serde_json::json!({
                    "id": run_id,
                    "board": board,
                    "kind": "triage",
                    "origin": "managed",
                    "runner": "acp",
                    "state": "succeeded",
                    "cost_units": 1,
                    "created_at": now.to_rfc3339(),
                    "updated_at": now.to_rfc3339()
                }),
            ),
        );

        let fake = Arc::new(FakeRunner::new("acp"));
        fake.set_status(
            "sess-1",
            RunnerStatus {
                state: RunnerState::Idle,
                runnable: true,
                busy: false,
                runtime: Some(Runtime::Antigravity),
                workdir: Some(dir.path().to_path_buf()),
                runs: Some(1),
            },
        );
        fake.set_token_usage(
            "sess-1",
            RunTokenUsage {
                tokens_in: 120_000,
                cached_tokens: 90_000,
                tokens_out: 2_400,
                reasoning_tokens: 600,
            },
        );

        let mut daemon = srv.make_daemon(
            &home,
            Config::default(),
            UsageSource::Antigravity {
                cli_bin: None,
                snapshots: vec![],
            },
        );
        daemon.runners.register(fake);
        daemon.state.managed_runs.insert(
            run_id.clone(),
            ManagedRun {
                run_id: run_id.clone(),
                board,
                kind: RunKind::Triage,
                task_id: None,
                tasks: vec![],
                runner: "acp".to_owned(),
                native_id: Some("sess-1".to_owned()),
                agent_id: Some("antigravity:sess-1".parse().unwrap()),
                worktree_id: None,
                worktree_path: Some(dir.path().to_path_buf()),
                branch: None,
                cost_units: 1,
                title: "triage factory".to_owned(),
                started_at: now - TimeDelta::seconds(30),
                last_renewed_at: Some(now - TimeDelta::seconds(10)),
                lease_expires_at: Some(now + TimeDelta::minutes(2)),
                noted: false,
                waiting_external: false,
                last_event_seq: 0,
            },
        );

        daemon.renew_runs(now).await.unwrap();
        let patches: Vec<serde_json::Value> = srv
            .bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _)| m == "PATCH" && p == &format!("/v1/runs/{run_id}"))
            .map(|(_, _, b)| b.clone())
            .collect();
        assert_eq!(patches.len(), 1, "expected one terminal PATCH: {patches:?}");
        assert_eq!(patches[0]["state"], "succeeded");
        assert_eq!(
            patches[0]["outcome"],
            serde_json::json!({
                "tokens_in": 120_000,
                "cached_tokens": 90_000,
                "tokens_out": 2_400,
                "reasoning_tokens": 600,
                "warm_cache_hit": true,
            })
        );
    }

    #[tokio::test]
    async fn backfill_missing_run_tokens_recovers_orphaned_acp_and_interactive_t3_runs() {
        let srv = UsageStubServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let now: Timestamp = "2026-11-28T05:30:00Z".parse().unwrap();

        let orphan_id: RunId = "run-orphan-acp-1".parse().unwrap();
        let interactive_id: RunId = "run-interactive-deadbeef0001".parse().unwrap();
        let blind_id: RunId = "run-orphan-blind-1".parse().unwrap();
        let merge_id: RunId = "run-merge-det-1".parse().unwrap();

        // 1. Write local ACP transcript for the orphaned worker run.
        let transcript_dir = home.state_dir().join("transcripts");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::write(
            transcript_dir.join(format!("{orphan_id}.jsonl")),
            concat!(
                r#"{"dir":"agent","payload":{"id":2,"result":{"stopReason":"end_turn","usage":{"inputTokens":20000,"cachedReadTokens":50000,"outputTokens":1100,"thoughtTokens":250,"totalTokens":71350}}}}"#, "\n",
            ),
        )
        .unwrap();

        // 2. Write local T3 SQLite projection DB for the completed interactive T3 run.
        let t3_db = dir.path().join("statev2.sqlite");
        {
            let conn = rusqlite::Connection::open(&t3_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE orchestration_v2_projection_provider_turns (
                    provider_turn_id TEXT PRIMARY KEY,
                    thread_id TEXT NOT NULL,
                    started_at TEXT,
                    completed_at TEXT,
                    payload_json TEXT NOT NULL
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO orchestration_v2_projection_provider_turns
                 (provider_turn_id, thread_id, started_at, completed_at, payload_json)
                 VALUES ('turn-1', 'thr-inter-1', '2026-11-28T05:05:00Z', '2026-11-28T05:15:00Z', ?1)",
                [r#"{"turnTokenUsage":{"inputTokens":64000,"cachedInputTokens":48000,"outputTokens":1800,"reasoningTokens":400}}"#],
            )
            .unwrap();
        }

        // Stub `GET /v1/runs` returning all 4 terminal runs on this device ("air").
        srv.responses.lock().unwrap().insert(
            ("GET".to_owned(), "/v1/runs".to_owned()),
            (
                StatusCode::OK,
                serde_json::json!([
                    {
                        "id": orphan_id,
                        "board": "factory",
                        "task_id": "factory-1",
                        "kind": "worker",
                        "origin": "managed",
                        "device": "air",
                        "runner": "acp",
                        "state": "orphaned",
                        "cost_units": 2,
                        "outcome": { "quota_bucket": "antigravity/default" },
                        "created_at": "2026-11-28T04:50:00Z",
                        "started_at": "2026-11-28T04:51:00Z",
                        "ended_at": "2026-11-28T05:00:00Z",
                        "updated_at": "2026-11-28T05:00:00Z"
                    },
                    {
                        "id": interactive_id,
                        "board": "factory",
                        "kind": "interactive",
                        "origin": "interactive",
                        "device": "air",
                        "runner": "t3",
                        "agent_id": "t3:thr-inter-1",
                        "state": "succeeded",
                        "cost_units": 2,
                        "created_at": "2026-11-28T05:00:00Z",
                        "started_at": "2026-11-28T05:00:00Z",
                        "ended_at": "2026-11-28T05:20:00Z",
                        "updated_at": "2026-11-28T05:20:00Z"
                    },
                    {
                        "id": blind_id,
                        "board": "factory",
                        "kind": "worker",
                        "origin": "managed",
                        "device": "air",
                        "runner": "acp",
                        "state": "orphaned",
                        "cost_units": 2,
                        "created_at": "2026-11-28T04:00:00Z",
                        "started_at": "2026-11-28T04:01:00Z",
                        "ended_at": "2026-11-28T04:10:00Z",
                        "updated_at": "2026-11-28T04:10:00Z"
                    },
                    {
                        "id": merge_id,
                        "board": "factory",
                        "kind": "merge",
                        "origin": "managed",
                        "device": "air",
                        "runner": "deterministic",
                        "state": "succeeded",
                        "cost_units": 1,
                        "created_at": "2026-11-28T05:10:00Z",
                        "started_at": "2026-11-28T05:10:00Z",
                        "ended_at": "2026-11-28T05:11:00Z",
                        "updated_at": "2026-11-28T05:11:00Z"
                    }
                ]),
            ),
        );
        for rid in [&orphan_id, &interactive_id] {
            srv.responses.lock().unwrap().insert(
                ("PATCH".to_owned(), format!("/v1/runs/{rid}")),
                (
                    StatusCode::OK,
                    serde_json::json!({
                        "id": rid,
                        "board": "factory",
                        "kind": "worker",
                        "origin": "managed",
                        "device": "air",
                        "runner": "acp",
                        "state": "orphaned",
                        "cost_units": 2,
                        "created_at": now.to_rfc3339(),
                        "updated_at": now.to_rfc3339()
                    }),
                ),
            );
        }

        let mut daemon = srv
            .make_daemon(
                &home,
                Config::default(),
                UsageSource::Antigravity {
                    cli_bin: None,
                    snapshots: vec![],
                },
            )
            .with_t3_state_db_path(Some(t3_db));

        let count = daemon.backfill_missing_run_tokens(now).await;
        assert_eq!(
            count, 2,
            "must backfill both orphaned ACP run and interactive T3 run, skipping blind & deterministic runs"
        );
        // Second call is an idempotent no-op because checked runs are remembered.
        assert_eq!(daemon.backfill_missing_run_tokens(now).await, 0);

        let bodies = srv.bodies.lock().unwrap().clone();
        let orphan_patch = bodies
            .iter()
            .find(|(m, p, _)| m == "PATCH" && p == &format!("/v1/runs/{orphan_id}"))
            .map(|(_, _, b)| b.clone())
            .expect("expected PATCH for orphaned ACP run");
        assert_eq!(
            orphan_patch["outcome"],
            serde_json::json!({
                "quota_bucket": "antigravity/default",
                "tokens_in": 70_000,
                "cached_tokens": 50_000,
                "tokens_out": 1_100,
                "reasoning_tokens": 250,
                "warm_cache_hit": true,
            })
        );
        let inter_patch = bodies
            .iter()
            .find(|(m, p, _)| m == "PATCH" && p == &format!("/v1/runs/{interactive_id}"))
            .map(|(_, _, b)| b.clone())
            .expect("expected PATCH for interactive T3 run");
        assert_eq!(
            inter_patch["outcome"],
            serde_json::json!({
                "tokens_in": 64_000,
                "cached_tokens": 48_000,
                "tokens_out": 1_800,
                "reasoning_tokens": 400,
                "warm_cache_hit": true,
            })
        );
        assert!(
            !bodies
                .iter()
                .any(|(m, p, _)| m == "PATCH" && p == &format!("/v1/runs/{blind_id}")),
            "missing transcript must no-op cleanly without patching"
        );
        assert!(
            !bodies
                .iter()
                .any(|(m, p, _)| m == "PATCH" && p == &format!("/v1/runs/{merge_id}")),
            "deterministic merge runs must not be patched"
        );
    }

    #[test]
    fn tcc_repo_backoff_exponential_schedule_filtering_expiry_and_reset() {
        let t0: Timestamp = "2026-11-26T07:06:00Z".parse().unwrap();
        let mut backoff = TccRepoBackoff::default();
        let vps_repo = "github.com/org/vps".to_owned();
        let healthy_repo = "github.com/org/healthy".to_owned();
        let vps_path = PathBuf::from("/Users/u/Documents/vps");
        let repos = vec![vps_repo.clone()];

        // Attempt 1: 5m.
        let (delay1, until1, attempts1) = backoff.next_schedule(&repos, &vps_path, t0);
        assert_eq!(delay1, TCC_REPO_BACKOFF_FIRST);
        assert_eq!(until1, t0 + TimeDelta::minutes(5));
        assert_eq!(attempts1, 1);
        backoff.record(
            &repos,
            vps_path.clone(),
            delay1,
            until1,
            attempts1,
            "Interrupted system call".to_owned(),
        );

        let projects = BTreeMap::from([
            (
                healthy_repo.clone(),
                PathBuf::from("/Users/u/Developer/healthy"),
            ),
            (vps_repo.clone(), vps_path.clone()),
            ("github.com/org/vps-alias".to_owned(), vps_path.clone()),
        ]);

        // During backoff, the affected repo AND any sibling key sharing its path are excluded;
        // the healthy sibling remains claimable.
        assert!(backoff.is_repo_backed_off(&vps_repo, t0 + TimeDelta::minutes(4)));
        assert!(!backoff.is_repo_backed_off(&healthy_repo, t0 + TimeDelta::minutes(4)));
        assert_eq!(
            backoff.filter_projects(&projects, t0 + TimeDelta::minutes(4)),
            vec![healthy_repo.clone()]
        );
        assert!(
            backoff
                .active_for(&[], &vps_path, t0 + TimeDelta::minutes(4))
                .is_some(),
            "path lookup matches even without repo key"
        );

        // Once expired at `until1`, repo and sibling alias are eligible again.
        assert!(!backoff.is_repo_backed_off(&vps_repo, until1));
        assert_eq!(
            backoff.filter_projects(&projects, until1),
            vec![
                healthy_repo.clone(),
                vps_repo.clone(),
                "github.com/org/vps-alias".to_owned(),
            ]
        );

        // Consecutive failures double up to TCC_REPO_BACKOFF_MAX (5m -> 10m -> 20m -> 30m).
        let (delay2, until2, attempts2) = backoff.next_schedule(&repos, &vps_path, until1);
        assert_eq!(delay2, TimeDelta::minutes(10));
        assert_eq!(attempts2, 2);
        backoff.record(
            &repos,
            vps_path.clone(),
            delay2,
            until2,
            attempts2,
            "eintr".into(),
        );

        let (delay3, until3, attempts3) = backoff.next_schedule(&repos, &vps_path, until2);
        assert_eq!(delay3, TimeDelta::minutes(20));
        assert_eq!(attempts3, 3);
        backoff.record(
            &repos,
            vps_path.clone(),
            delay3,
            until3,
            attempts3,
            "eintr".into(),
        );

        let (delay4, until4, attempts4) = backoff.next_schedule(&repos, &vps_path, until3);
        assert_eq!(delay4, TCC_REPO_BACKOFF_MAX);
        assert_eq!(attempts4, 4);
        backoff.record(
            &repos,
            vps_path.clone(),
            delay4,
            until4,
            attempts4,
            "eintr".into(),
        );

        // Long after expiry (>= until + MAX), schedule resets cleanly to attempt 1.
        let later = until4 + TCC_REPO_BACKOFF_MAX;
        let (delay_reset, _, attempts_reset) = backoff.next_schedule(&repos, &vps_path, later);
        assert_eq!(delay_reset, TCC_REPO_BACKOFF_FIRST);
        assert_eq!(attempts_reset, 1);

        // Explicit clear_for (after a healthy git call) also resets immediately.
        backoff.clear_for(&repos, &vps_path);
        assert!(backoff.active_for(&repos, &vps_path, t0).is_none());
        let (delay_cleared, _, attempts_cleared) = backoff.next_schedule(&repos, &vps_path, t0);
        assert_eq!(delay_cleared, TCC_REPO_BACKOFF_FIRST);
        assert_eq!(attempts_cleared, 1);
    }

    #[tokio::test]
    async fn tcc_repo_backoff_records_trail_omits_repo_from_claims_and_cancels_precheck() {
        use crate::runner::FakeRunner;

        let srv = UsageStubServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let vps_dir = dir.path().join("Documents").join("vps");
        let healthy_dir = dir.path().join("Developer").join("healthy");
        std::fs::create_dir_all(&vps_dir).unwrap();
        std::fs::create_dir_all(&healthy_dir).unwrap();
        let t0: Timestamp = "2026-11-26T07:06:00Z".parse().unwrap();

        let board_slug: BoardSlug = "vps".parse().unwrap();
        let task_id: TaskId = "vps-316".parse().unwrap();
        let run_id: RunId = "run-tcc-1".parse().unwrap();
        let vps_repo = "github.com/org/vps".to_owned();
        let healthy_repo = "github.com/org/healthy".to_owned();

        let board_json = serde_json::json!({
            "slug": board_slug,
            "title": "VPS",
            "goal": "",
            "repos": [vps_repo],
            "status": "active",
            "autopilot": true,
            "coordinator": null,
            "home_devices": ["test-dev"],
            "counts": {
                "open": 1,
                "doing": 0,
                "blocked": 0,
                "review": 0,
                "done": 0,
                "dropped": 0,
                "ready": 1,
                "ready_auto": 1,
            },
            "open_asks": 0,
            "created_at": t0.to_rfc3339(),
            "updated_at": t0.to_rfc3339(),
        });
        srv.responses.lock().unwrap().insert(
            ("GET".to_owned(), "/v1/boards".to_owned()),
            (StatusCode::OK, serde_json::json!([board_json])),
        );
        srv.responses.lock().unwrap().insert(
            ("POST".to_owned(), "/v1/dispatch/claim".to_owned()),
            (
                StatusCode::OK,
                serde_json::json!({ "claimed": [], "holds_summary": [] }),
            ),
        );
        srv.responses.lock().unwrap().insert(
            ("GET".to_owned(), "/v1/devices/test-dev/wakes".to_owned()),
            (StatusCode::OK, serde_json::json!([])),
        );
        srv.responses.lock().unwrap().insert(
            ("GET".to_owned(), format!("/v1/tasks/{task_id}")),
            (
                StatusCode::OK,
                serde_json::json!({
                    "task": {
                        "id": task_id,
                        "board": board_slug,
                        "parent": null,
                        "title": "Fix nginx",
                        "body": "",
                        "acceptance": "",
                        "priority": "p2",
                        "status": "open",
                        "blocked_reason": null,
                        "owner": null,
                        "owner_stale": false,
                        "blockers": [],
                        "ready": true,
                        "auto": true,
                        "repo": vps_repo,
                        "version": 1,
                        "created_by": "t3:user",
                        "created_at": t0.to_rfc3339(),
                        "updated_at": t0.to_rfc3339(),
                    },
                    "notes": [],
                    "evidence": [],
                    "blocks": [],
                    "children": [],
                    "history": [],
                }),
            ),
        );
        srv.responses.lock().unwrap().insert(
            ("PATCH".to_owned(), format!("/v1/runs/{run_id}")),
            (
                StatusCode::OK,
                serde_json::json!({
                    "id": run_id,
                    "board": board_slug,
                    "kind": "worker",
                    "origin": "managed",
                    "runner": "acp",
                    "state": "cancelled",
                    "cost_units": 2,
                    "created_at": t0.to_rfc3339(),
                    "updated_at": t0.to_rfc3339(),
                }),
            ),
        );

        let mut cfg = Config {
            dispatch_mode: DispatchMode::Pull,
            ..Config::default()
        };
        cfg.projects.insert(vps_repo.clone(), vps_dir.clone());
        cfg.projects
            .insert(healthy_repo.clone(), healthy_dir.clone());

        let mut daemon = srv
            .make_daemon(&home, cfg, UsageSource::Server)
            .with_runner(FakeRunner::new("acp"));
        daemon.set_dispatch_claim_supported(true);

        // Initially both repos are advertised in heartbeats.
        assert_eq!(
            daemon.build_heartbeat_at(t0).repos,
            vec![healthy_repo.clone(), vps_repo.clone()]
        );

        // Record a macOS TCC privacy refusal on `vps_repo`.
        let detail = format!(
            "git rev-parse failed in {}: fatal: Unable to read current working directory: \
             Interrupted system call{}",
            vps_dir.display(),
            crate::worktree::PRIVACY_HINT
        );
        let (until, trail_path) = daemon.record_tcc_repo_backoff(
            &board_slug,
            Some(&task_id),
            Some(&run_id),
            std::slice::from_ref(&vps_repo),
            &vps_dir,
            &detail,
            t0,
        );
        assert_eq!(until, t0 + TCC_REPO_BACKOFF_FIRST);
        let trail_path = trail_path.expect("Trail Rule record written");
        assert!(trail_path.exists());
        let trail_text = std::fs::read_to_string(&trail_path).unwrap();
        let entry: serde_json::Value = serde_json::from_str(trail_text.trim()).unwrap();
        assert_eq!(entry["kind"], "tcc_repo_backoff");
        assert_eq!(entry["board"], "vps");
        assert_eq!(entry["task"], "vps-316");
        assert_eq!(entry["subject"]["backoff_secs"], 300);
        assert_eq!(entry["subject"]["attempts"], 1);
        assert_eq!(entry["subject"]["repos"], serde_json::json!([vps_repo]));
        assert!(
            entry["cause"]
                .as_str()
                .unwrap()
                .contains("Privacy & Security > Files and Folders")
        );

        // Heartbeat and pull-claim omit the backed-off repo while keeping the healthy one.
        let t1 = t0 + TimeDelta::minutes(1);
        assert_eq!(
            daemon.build_heartbeat_at(t1).repos,
            vec![healthy_repo.clone()]
        );
        daemon.reconcile(t1).await.unwrap();
        let claim_bodies: Vec<serde_json::Value> = srv
            .bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _)| m == "POST" && p == "/v1/dispatch/claim")
            .map(|(_, _, b)| b.clone())
            .collect();
        assert_eq!(claim_bodies.len(), 1);
        assert_eq!(claim_bodies[0]["repos"], serde_json::json!([healthy_repo]));

        // Pre-check in `spawn_claimed_llm_run` immediately cancels any run for a backed-off
        // repo/path without running git and requests stopping the pull tick.
        let claimed = ClaimedRun {
            run: board_proto::Run {
                id: run_id.clone(),
                board: Some(board_slug.clone()),
                task_id: Some(task_id.clone()),
                agent_id: None,
                device: "test-dev".parse().unwrap(),
                kind: RunKind::Worker,
                origin: board_proto::RunOrigin::Managed,
                runner: "acp".to_owned(),
                state: RunState::Leased,
                cost_units: 2,
                policy_version: None,
                title: Some("vps-316 Fix nginx".to_owned()),
                worktree_branch: None,
                cwd: None,
                attach_hint: None,
                transcript_uri: None,
                transcript_sha256: None,
                conflict_locks: vec![],
                outcome: None,
                lease_expires_at: Some(t1 + TimeDelta::minutes(3)),
                started_at: None,
                ended_at: None,
                created_at: t1,
                updated_at: t1,
            },
            board: board_slug.clone(),
            task_id: Some(task_id.clone()),
            tasks: vec![],
            score: 1.0,
            score_breakdown: board_proto::ScoreBreakdown::default(),
            conflict_keys: vec![],
            housekeeping_action: None,
        };
        let mut merge_deferred = BTreeMap::new();
        let exec = daemon
            .execute_claimed_run(claimed, t1, &mut merge_deferred)
            .await;
        assert!(exec.stop_pull_tick);
        let patches: Vec<serde_json::Value> = srv
            .bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _)| m == "PATCH" && p == &format!("/v1/runs/{run_id}"))
            .map(|(_, _, b)| b.clone())
            .collect();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0]["state"], "cancelled");
        assert!(
            patches[0]["reason"]
                .as_str()
                .unwrap()
                .contains("macOS TCC privacy refusal")
        );
        // Pre-check cancellation must also record a Trail Rule entry before cancelling.
        let trail_lines: Vec<serde_json::Value> = std::fs::read_to_string(&trail_path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(
            trail_lines
                .iter()
                .any(|v| v["kind"] == "tcc_repo_backoff_cancelled"
                    && v["subject"]["run_id"] == run_id.as_str()),
            "expected tcc_repo_backoff_cancelled trail record: {trail_lines:?}"
        );

        // A sibling `[projects]` key pointing to the same TCC-refused checkout path must also
        // be omitted from heartbeats and dispatch/claim.
        let alias_repo = "github.com/org/vps-alias".to_owned();
        daemon
            .ctx
            .config
            .projects
            .insert(alias_repo.clone(), vps_dir.clone());
        assert_eq!(
            daemon.build_heartbeat_at(t1).repos,
            vec![healthy_repo.clone()]
        );
        daemon.ctx.config.projects.remove(&alias_repo);

        // Single-repo device where ALL configured repos are backed off (and no healthy
        // `[board_projects]` exists) must withhold repo-requiring kinds (asking only for
        // `housekeeping`) so an empty `req.repos` cannot fall back to stale `device.repos`.
        daemon.ctx.config.projects.remove(&healthy_repo);
        srv.bodies.lock().unwrap().clear();
        daemon.reconcile(t1).await.unwrap();
        let single_repo_claims: Vec<serde_json::Value> = srv
            .bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _)| m == "POST" && p == "/v1/dispatch/claim")
            .map(|(_, _, b)| b.clone())
            .collect();
        assert_eq!(single_repo_claims.len(), 1);
        assert!(
            single_repo_claims[0]
                .get("repos")
                .is_none_or(|v| v == &serde_json::json!([])),
            "backed-off repo must be omitted from dispatch/claim: {:?}",
            single_repo_claims[0]
        );
        assert_eq!(
            single_repo_claims[0]["supported_kinds"],
            serde_json::json!(["housekeeping"])
        );

        // If a healthy `[board_projects]` board exists alongside the backed-off `[projects]`
        // repo, LLM kinds stay enabled so the healthy `[board_projects]` board is not starved.
        let feedback_slug: BoardSlug = "board-feedback".parse().unwrap();
        daemon
            .ctx
            .config
            .board_projects
            .insert(feedback_slug.clone(), healthy_dir.clone());
        srv.bodies.lock().unwrap().clear();
        daemon.reconcile(t1).await.unwrap();
        let board_proj_claims: Vec<serde_json::Value> = srv
            .bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _)| m == "POST" && p == "/v1/dispatch/claim")
            .map(|(_, _, b)| b.clone())
            .collect();
        assert_eq!(board_proj_claims.len(), 1);
        assert!(
            board_proj_claims[0]["supported_kinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k == "triage"),
            "healthy [board_projects] must not be starved: {:?}",
            board_proj_claims[0]
        );
        daemon.ctx.config.board_projects.remove(&feedback_slug);

        // Editing `[projects]` in config.toml and calling `reload_config()` clears the backoff.
        let moved_dir = dir.path().join("Developer").join("vps");
        std::fs::create_dir_all(&moved_dir).unwrap();
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(
            home.config_path(),
            format!(
                "dispatch_mode = \"pull\"\n[projects]\n\"{vps_repo}\" = \"{}\"\n",
                moved_dir.display()
            ),
        )
        .unwrap();
        daemon.reload_config();
        assert_eq!(daemon.build_heartbeat_at(t1).repos, vec![vps_repo]);
    }

    #[test]
    fn should_self_restart_requires_known_differing_commits() {
        assert!(!should_self_restart("unknown", Some("abcdef012345")));
        assert!(!should_self_restart("", Some("abcdef012345")));
        assert!(!should_self_restart("abcdef012345", None));
        assert!(!should_self_restart("abcdef012345", Some("unknown")));
        assert!(!should_self_restart("abcdef012345", Some("")));
        assert!(!should_self_restart("abcdef012345", Some("abcdef012345")));
        assert!(should_self_restart("111111111111", Some("222222222222")));
    }

    #[tokio::test]
    async fn after_merge_deploy_finishes_task_and_run_before_triggering_self_restart() {
        use std::sync::Mutex;

        use board_proto::{CreateBoard, CreateRun, CreateTask, Priority, RegisterAgent, RunOrigin};
        use board_server::{AppState, Db, ManualClock, ServerConfig};

        struct FakeGh;

        impl Gh for FakeGh {
            fn view<'a>(
                &'a self,
                _project: &'a Path,
                url: &'a str,
            ) -> BoxFuture<'a, Result<crate::gh::PullRequest>> {
                let url = url.to_owned();
                Box::pin(async move {
                    Ok(crate::gh::PullRequest {
                        url,
                        state: PrState::Merged,
                        draft: false,
                        base_ref: "main".to_owned(),
                        head_ref: "task/deploy".to_owned(),
                        head_oid: "1111111111111111111111111111111111111111".to_owned(),
                        cross_repository: false,
                        merge_commit: Some("2222222222222222222222222222222222222222".to_owned()),
                        merge_state: crate::gh::MergeState::Clean,
                        review_decision: crate::gh::ReviewDecision::Approved,
                        checks: crate::gh::Checks::Passed,
                        passed_checks: vec!["rust".to_owned()],
                    })
                })
            }

            fn ready<'a>(&'a self, _project: &'a Path, _pr: &'a str) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }

            fn update_branch<'a>(
                &'a self,
                _project: &'a Path,
                _pr: &'a str,
            ) -> BoxFuture<'a, Result<crate::gh::Updated>> {
                Box::pin(async { Ok(crate::gh::Updated::Done) })
            }

            fn merge<'a>(
                &'a self,
                _project: &'a Path,
                _pr: &'a str,
                _head: &'a str,
            ) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }

            fn commit_check_status<'a>(
                &'a self,
                _project: &'a Path,
                _pr_url: Option<&'a str>,
                _sha: &'a str,
                _check_name: &'a str,
            ) -> BoxFuture<'a, Result<crate::gh::Checks>> {
                Box::pin(async { Ok(crate::gh::Checks::Passed) })
            }
        }

        struct FakeDeployer {
            events: Arc<Mutex<Vec<String>>>,
            installed: Arc<Mutex<Option<String>>>,
            next_installed: Option<String>,
        }

        impl UpgradeDeployer for FakeDeployer {
            fn run_upgrade<'a>(
                &'a self,
                _project: &'a Path,
                sha: &'a str,
            ) -> BoxFuture<'a, Result<UpgradeRunOutput>> {
                self.events.lock().unwrap().push(format!("upgrade:{sha}"));
                if let Some(ref commit) = self.next_installed {
                    *self.installed.lock().unwrap() = Some(commit.clone());
                }
                Box::pin(async {
                    Ok(UpgradeRunOutput {
                        exit_code: 0,
                        output_tail: "done: local restart deferred".to_owned(),
                    })
                })
            }
        }

        struct FakeRestarter {
            running: String,
            installed: Arc<Mutex<Option<String>>>,
            events: Arc<Mutex<Vec<String>>>,
            server_db_path: PathBuf,
            expected_task: TaskId,
            expected_run_id: Arc<Mutex<Option<RunId>>>,
            local_db: LocalDb,
        }

        impl SelfRestarter for FakeRestarter {
            fn running_commit(&self) -> &str {
                &self.running
            }

            fn installed_commit(&self) -> BoxFuture<'_, Option<String>> {
                let commit = self.installed.lock().unwrap().clone();
                Box::pin(async move { commit })
            }

            fn trigger_restart(&self, running: &str, installed: &str) -> Result<()> {
                // Verify ordering invariant: at the moment self-restart triggers, the task MUST
                // already be Done on the server, the server Run MUST already be Succeeded, and
                // local daemon.db MUST already record the run as Finished.
                let conn = rusqlite::Connection::open(&self.server_db_path).unwrap();
                let task_status: String = conn
                    .query_row(
                        "SELECT status FROM tasks WHERE id = ?1",
                        [self.expected_task.as_str()],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    task_status, "done",
                    "task must be marked done BEFORE self-restart triggers"
                );
                let run_id = self
                    .expected_run_id
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("expected run id");
                let run_state: String = conn
                    .query_row(
                        "SELECT state FROM runs WHERE id = ?1",
                        [run_id.as_str()],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    run_state, "succeeded",
                    "after-merge-deploy run must be Succeeded on server BEFORE self-restart triggers"
                );
                let local_rec = self
                    .local_db
                    .get_local_run(run_id.as_str())
                    .unwrap()
                    .expect("local run record");
                assert_eq!(
                    local_rec.state,
                    LocalRunState::Finished,
                    "local daemon.db run must be Finished BEFORE self-restart triggers"
                );
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("restart:{running}->{installed}"));
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("board.db");
        let db = Db::open(&db_path).unwrap();
        let t0: Timestamp = "2026-11-27T00:00:00Z".parse().unwrap();
        let clock = Arc::new(ManualClock::new(t0));
        let state = AppState::new(db, &ServerConfig::new(db_path.clone()), clock);
        let dev: DeviceName = "mac-air".parse().unwrap();
        let token = state.create_device(&dev, true).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv_state = state.clone();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, board_server::app(srv_state)).await;
        });

        let base_url = format!("http://{addr}");
        let client_as =
            |agent: Option<AgentId>| Client::new(&base_url, token.clone(), agent, None).unwrap();
        let worker_id: AgentId = "t3:worker-1".parse().unwrap();
        let review_id: AgentId = "antigravity:rev-1".parse().unwrap();
        let dispatch_id: AgentId = "dispatch:mac-air".parse().unwrap();
        let admin_client = client_as(Some(dispatch_id.clone()));
        let worker_client = client_as(Some(worker_id.clone()));
        let review_client = client_as(Some(review_id.clone()));
        for (c, id, role) in [
            (&worker_client, &worker_id, AgentRole::Worker),
            (&review_client, &review_id, AgentRole::Worker),
            (&admin_client, &dispatch_id, AgentRole::Coordinator),
        ] {
            c.register(&RegisterAgent {
                id: id.clone(),
                runtime: Runtime::Antigravity,
                device: dev.clone(),
                role,
                title: None,
                repo: Some("github.com/o/r".into()),
                branch: None,
                cwd: None,
                native_ids: BTreeMap::new(),
                forked_from: None,
            })
            .await
            .unwrap();
        }
        let _board = admin_client
            .create_board(&CreateBoard {
                slug: "factory".parse().unwrap(),
                title: "Factory".into(),
                goal: "Ship".into(),
                autopilot: true,
                temporary: false,
                repos: vec!["github.com/o/r".into()],
                home_devices: vec![dev.clone()],
            })
            .await
            .unwrap();

        let sha = "2222222222222222222222222222222222222222";
        let setup_deploy_task = |title: &str| {
            let admin_client = admin_client.clone();
            let worker_client = worker_client.clone();
            let review_client = review_client.clone();
            let worker_id = worker_id.clone();
            let review_id = review_id.clone();
            let dispatch_id = dispatch_id.clone();
            let dev = dev.clone();
            let title = title.to_owned();
            async move {
                let created = worker_client
                    .create_task(&CreateTask {
                        board: "factory".parse().unwrap(),
                        title,
                        body: "Deploy".into(),
                        acceptance: "deploy/upgrade.sh --yes".into(),
                        priority: Priority::P1,
                        parent: None,
                        blockers: vec![],
                        auto: Some(true),
                        not_before: None,
                        rank: None,
                        rank_top: false,
                        claim: true,
                        agent: worker_id.clone(),
                        repo: Some("github.com/o/r".into()),
                        weight: None,
                        resource_units: None,
                        requires: vec![],
                        tolerations: vec![],
                        conflict_keys: vec![],
                        affinity: None,
                        preferred_runner: None,
                        origin: None,
                        size: None,
                        before: None,
                        after: None,
                    })
                    .await
                    .unwrap();
                worker_client
                    .update_task(
                        &created.id,
                        &UpdateTask {
                            agent: Some(worker_id.clone().into()),
                            version: created.version,
                            status: Some(TaskStatus::Review),
                            title: None,
                            body: None,
                            acceptance: None,
                            priority: None,
                            blockers: None,
                            blocked_reason: None,
                            auto: None,
                            not_before: None,
                            rank: None,
                            owner: None,
                            reason: None,
                            repo: None,
                            weight: None,
                            resource_units: None,
                            requires: None,
                            tolerations: None,
                            conflict_keys: None,
                            affinity: None,
                            preferred_runner: None,
                            size: None,
                            cooled: None,
                            before: None,
                            after: None,
                        },
                    )
                    .await
                    .unwrap();
                let rev_run = admin_client
                    .create_run(&CreateRun {
                        task_id: Some(created.id.clone()),
                        board: Some("factory".parse().unwrap()),
                        kind: RunKind::Review,
                        origin: RunOrigin::Managed,
                        device: dev,
                        runner: "acp".into(),
                        agent_id: Some(review_id.clone()),
                        state: Some(RunState::Running),
                        lease_ttl_secs: Some(300),
                        lease_expires_at: None,
                        worktree_branch: None,
                        cwd: None,
                        title: Some("review".into()),
                        attach_hint: None,
                        cost_units: 2,
                        policy_version: None,
                        conflict_keys: vec![],
                    })
                    .await
                    .unwrap();
                let pass_note = review_client
                    .add_note(
                        &created.id,
                        &AddNote {
                            agent: review_id.clone(),
                            kind: NoteKind::Decision,
                            text: "review: pass. Clean diff.\npr: https://github.com/o/r/pull/99\nafter merge: deploy/upgrade.sh --yes".into(),
                        },
                    )
                    .await
                    .unwrap();
                admin_client
                    .update_run(
                        &rev_run.id,
                        &UpdateRun {
                            state: Some(RunState::Succeeded),
                            outcome: Some(Some(serde_json::json!({"status": "review_pass"}))),
                            ..UpdateRun::default()
                        },
                    )
                    .await
                    .unwrap();
                let ready_text = format!(
                    "{} review pass #{}. pr: https://github.com/o/r/pull/99; merge_commit: {sha}.\n\
                     after merge: deploy/upgrade.sh --yes",
                    judges::AFTER_MERGE_READY_NOTE,
                    pass_note.id
                );
                admin_client
                    .add_note(
                        &created.id,
                        &AddNote {
                            agent: dispatch_id.clone(),
                            kind: NoteKind::Note,
                            text: ready_text,
                        },
                    )
                    .await
                    .unwrap();
                created.id
            }
        };

        let proj_dir = dir.path().join("repo");
        std::fs::create_dir_all(&proj_dir).unwrap();

        // Scenario 1: Binary changes ("111111111111" -> "222222222222") ->
        // upgrade runs, task closes, run succeeds, daemon.db updates, and ONLY THEN self-restart fires.
        let task_1 = setup_deploy_task("Upgrade binary").await;
        let home_1 = Home::at(dir.path().join("home-1"));
        let mut cfg_1 = Config {
            server: Some(base_url.clone()),
            device: Some(dev.clone()),
            dispatch_mode: DispatchMode::Pull,
            ..Config::default()
        };
        cfg_1
            .projects
            .insert("github.com/o/r".into(), proj_dir.clone());
        let ctx_1 = Ctx {
            home: home_1,
            config: cfg_1,
            client: client_as(None),
            device: dev.clone(),
            repo: crate::identity::RepoInfo::default(),
            known: None,
        };
        let events_1 = Arc::new(Mutex::new(Vec::<String>::new()));
        let installed_1 = Arc::new(Mutex::new(Some("111111111111".to_owned())));
        let expected_run_1 = Arc::new(Mutex::new(None::<RunId>));
        let mut daemon_1 = Daemon::new(ctx_1, T3Cli::locate(None))
            .with_runner(crate::runner::FakeRunner::new("acp"))
            .with_upgrade_conf(true)
            .with_gh(FakeGh)
            .with_upgrade_deployer(FakeDeployer {
                events: Arc::clone(&events_1),
                installed: Arc::clone(&installed_1),
                next_installed: Some("222222222222".to_owned()),
            });
        let local_db_1 = daemon_1.db().clone();
        daemon_1 = daemon_1.with_self_restarter(FakeRestarter {
            running: "111111111111".to_owned(),
            installed: Arc::clone(&installed_1),
            events: Arc::clone(&events_1),
            server_db_path: db_path.clone(),
            expected_task: task_1.clone(),
            expected_run_id: Arc::clone(&expected_run_1),
            local_db: local_db_1,
        });

        daemon_1.send_heartbeat(t0).await.unwrap();
        daemon_1.reconcile(t0).await.unwrap();
        assert_eq!(
            daemon_1.bg_merges.len(),
            1,
            "after-merge-deploy must spawn into bg_merges"
        );
        let run_id_1 = daemon_1.bg_merges.keys().next().unwrap().clone();
        *expected_run_1.lock().unwrap() = Some(run_id_1.clone());

        // Wait for the background deploy task to finish `upgrade.sh` + `task_done` + `update_run`.
        for _ in 0..100 {
            if daemon_1.bg_merges.values().all(|(_, h)| h.is_finished()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            daemon_1.bg_merges.values().all(|(_, h)| h.is_finished()),
            "bg deploy task did not complete in time"
        );
        // Before `renew_runs` reaps it, self-restart has not triggered yet.
        assert_eq!(*events_1.lock().unwrap(), vec![format!("upgrade:{sha}")]);

        // Reaping updates daemon.db and triggers self-restart (whose assertions prove server
        // task=Done, server run=Succeeded, and daemon.db=Finished all completed first).
        daemon_1
            .renew_runs(t0 + TimeDelta::seconds(5))
            .await
            .unwrap();
        assert_eq!(
            *events_1.lock().unwrap(),
            vec![
                format!("upgrade:{sha}"),
                "restart:111111111111->222222222222".to_owned(),
            ]
        );
        assert!(daemon_1.self_restart_triggered_at.is_some());

        // Scenario 2: Binary unchanged ("222222222222" == "222222222222") ->
        // task closes and run succeeds cleanly, and NO self-restart is triggered.
        let task_2 = setup_deploy_task("No-op binary deploy").await;
        let home_2 = Home::at(dir.path().join("home-2"));
        let mut cfg_2 = Config {
            server: Some(base_url.clone()),
            device: Some(dev.clone()),
            dispatch_mode: DispatchMode::Pull,
            ..Config::default()
        };
        cfg_2.projects.insert("github.com/o/r".into(), proj_dir);
        let ctx_2 = Ctx {
            home: home_2,
            config: cfg_2,
            client: client_as(None),
            device: dev,
            repo: crate::identity::RepoInfo::default(),
            known: None,
        };
        let events_2 = Arc::new(Mutex::new(Vec::<String>::new()));
        let installed_2 = Arc::new(Mutex::new(Some("222222222222".to_owned())));
        let expected_run_2 = Arc::new(Mutex::new(None::<RunId>));
        let mut daemon_2 = Daemon::new(ctx_2, T3Cli::locate(None))
            .with_runner(crate::runner::FakeRunner::new("acp"))
            .with_upgrade_conf(true)
            .with_gh(FakeGh)
            .with_upgrade_deployer(FakeDeployer {
                events: Arc::clone(&events_2),
                installed: Arc::clone(&installed_2),
                next_installed: None,
            });
        let local_db_2 = daemon_2.db().clone();
        daemon_2 = daemon_2.with_self_restarter(FakeRestarter {
            running: "222222222222".to_owned(),
            installed: Arc::clone(&installed_2),
            events: Arc::clone(&events_2),
            server_db_path: db_path,
            expected_task: task_2.clone(),
            expected_run_id: Arc::clone(&expected_run_2),
            local_db: local_db_2,
        });

        let t2 = t0 + TimeDelta::seconds(30);
        daemon_2.send_heartbeat(t2).await.unwrap();
        daemon_2.reconcile(t2).await.unwrap();
        assert_eq!(daemon_2.bg_merges.len(), 1);
        let run_id_2 = daemon_2.bg_merges.keys().next().unwrap().clone();
        *expected_run_2.lock().unwrap() = Some(run_id_2.clone());
        for _ in 0..100 {
            if daemon_2.bg_merges.values().all(|(_, h)| h.is_finished()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        daemon_2
            .renew_runs(t2 + TimeDelta::seconds(5))
            .await
            .unwrap();
        assert_eq!(
            admin_client.task(&task_2).await.unwrap().task.status,
            TaskStatus::Done
        );
        assert_eq!(
            admin_client.get_run(&run_id_2).await.unwrap().state,
            RunState::Succeeded
        );
        assert_eq!(
            *events_2.lock().unwrap(),
            vec![format!("upgrade:{sha}")],
            "unchanged binary must NOT trigger self-restart"
        );
        assert!(daemon_2.self_restart_triggered_at.is_none());

        state.shutdown();
        server_handle.abort();
    }

    #[test]
    fn dispatch_runtime_maps_configured_provider() {
        let mut cfg = Config::default();
        assert_eq!(
            Daemon::<crate::t3::T3Cli>::runtime_for_config(&cfg),
            Runtime::Claude
        );

        // Judge-only host with `[judge_model] provider = "antigravity"` and no `[thread_model]`.
        cfg.judge_model = Some(crate::config::ThreadModel {
            provider: "antigravity".into(),
            model: "gemini-4-neon".into(),
            options: BTreeMap::new(),
        });
        assert_eq!(
            Daemon::<crate::t3::T3Cli>::runtime_for_config(&cfg),
            Runtime::Antigravity
        );

        // `[thread_model]` takes precedence over `[judge_model]`.
        cfg.thread_model = Some(crate::config::ThreadModel {
            provider: "codex".into(),
            model: "gpt-5".into(),
            options: BTreeMap::new(),
        });
        assert_eq!(
            Daemon::<crate::t3::T3Cli>::runtime_for_config(&cfg),
            Runtime::Codex
        );

        cfg.thread_model = Some(crate::config::ThreadModel {
            provider: "opencode".into(),
            model: "default".into(),
            options: BTreeMap::new(),
        });
        assert_eq!(
            Daemon::<crate::t3::T3Cli>::runtime_for_config(&cfg),
            Runtime::Opencode
        );

        cfg.thread_model = Some(crate::config::ThreadModel {
            provider: "claudeAgent".into(),
            model: "claude-sonnet".into(),
            options: BTreeMap::new(),
        });
        assert_eq!(
            Daemon::<crate::t3::T3Cli>::runtime_for_config(&cfg),
            Runtime::Claude
        );

        // ACP-only fallback respects `acp_runner.model` and requires a resolvable binary.
        let mut acp_cfg = Config {
            acp_runner: Some(crate::config::AcpRunnerConfig {
                bin: Some(PathBuf::from("/bin/sh")),
                model: Some("claude-sonnet".into()),
                ..Default::default()
            }),
            ..Config::default()
        };
        assert_eq!(
            Daemon::<crate::t3::T3Cli>::runtime_for_config(&acp_cfg),
            Runtime::Claude
        );
        acp_cfg.acp_runner.as_mut().unwrap().model = Some("gemini-4-neon".into());
        assert_eq!(
            Daemon::<crate::t3::T3Cli>::runtime_for_config(&acp_cfg),
            Runtime::Antigravity
        );
    }

    #[test]
    fn classify_review_effort_covers_size_diff_security_and_after_merge_branches() {
        use crate::config::ReviewEffort;
        use board_proto::{Priority, TaskSize};

        let make_task = |title: &str, body: &str, size: Option<TaskSize>| -> Task {
            let now: Timestamp = "2026-11-27T10:00:00Z".parse().unwrap();
            Task {
                id: "factory-1".parse().unwrap(),
                board: "factory".parse().unwrap(),
                title: title.to_owned(),
                body: body.to_owned(),
                acceptance: String::new(),
                status: TaskStatus::Review,
                priority: Priority::P2,
                owner: None,
                owner_handle: None,
                owner_stale: false,
                created_by: "claude:worker".parse().unwrap(),
                parent: None,
                child_count: 0,
                blockers: Vec::new(),
                blocked_reason: None,
                auto: true,
                not_before: None,
                ready: false,
                blockers_closed: false,
                proposal: false,
                feedback: None,
                rank: None,
                summary: None,
                version: 1,
                created_at: now,
                updated_at: now,
                snippet: None,
                any_word: false,
                repo: Some("github.com/o/r".to_owned()),
                weight: None,
                resource_units: None,
                requires: Vec::new(),
                tolerations: Vec::new(),
                conflict_keys: Vec::new(),
                affinity: None,
                preferred_runner: None,
                origin: None,
                size,
                cooled_at: None,
                ready_at: None,
                report_count: 0,
                effective_priority: None,
            }
        };
        let files =
            |list: &[&str]| -> Vec<String> { list.iter().map(|s| (*s).to_owned()).collect() };

        // 1. Substantive code PRs (s, m, l, unknown, None) stay High.
        for size in [
            Some(TaskSize::S),
            Some(TaskSize::M),
            Some(TaskSize::L),
            Some(TaskSize::Unknown),
            None,
        ] {
            let task = make_task("Refactor dispatch scoring", "", size);
            assert_eq!(
                classify_review_effort(
                    &task,
                    false,
                    &files(&["crates/board/src/daemon.rs"]),
                    false
                ),
                ReviewEffort::High,
                "size={size:?} code PR must stay High"
            );
        }

        // 2. Non-security `xs` PR drops to Medium (with code files or when diff is unavailable).
        let xs_task = make_task("Fix typo in render output", "", Some(TaskSize::Xs));
        assert_eq!(
            classify_review_effort(
                &xs_task,
                false,
                &files(&["crates/board/src/render.rs"]),
                false
            ),
            ReviewEffort::Medium
        );
        assert_eq!(
            classify_review_effort(&xs_task, false, &[], false),
            ReviewEffort::Medium
        );
        // LLM-token wording in an xs title is NOT misclassified as auth-token security work.
        let xs_llm_tokens = make_task(
            "Reduce cached token usage in review prepack",
            "",
            Some(TaskSize::Xs),
        );
        assert_eq!(
            classify_review_effort(
                &xs_llm_tokens,
                false,
                &files(&["crates/board/src/daemon/prepack.rs"]),
                false
            ),
            ReviewEffort::Medium
        );

        // 3. Security-sensitive PRs ALWAYS stay High even when size is `xs`.
        for sec_path in [
            "crates/board-server/src/auth.rs",
            "crates/board/src/mcp_http/oauth.rs",
            ".github/workflows/ci.yml",
            "deploy/upgrade.sh",
            ".env.production",
            "hooks/guard-pr-merge.sh",
            "Dockerfile",
        ] {
            assert_eq!(
                classify_review_effort(&xs_task, false, &files(&[sec_path]), false),
                ReviewEffort::High,
                "security path {sec_path} must override xs"
            );
        }
        for (title, body) in [
            ("Fix bearer token validation on /v1/admin", ""),
            ("Harden sandbox permission check", ""),
            (
                "Fix path handling",
                "Prevents path traversal when resolving worktree",
            ),
            ("Patch parser", "Mitigates CVE-2026-12345"),
        ] {
            let sec_task = make_task(title, body, Some(TaskSize::Xs));
            assert_eq!(
                classify_review_effort(&sec_task, false, &files(&["src/api.rs"]), false),
                ReviewEffort::High,
                "security task {title:?} must override xs"
            );
        }

        // 4. Docs / prompt-only diffs drop to Medium even for larger/unestimated tasks.
        let m_docs_task = make_task("Document auth token rotation", "", Some(TaskSize::M));
        assert_eq!(
            classify_review_effort(
                &m_docs_task,
                false,
                &files(&[
                    "docs/DEPLOY.md",
                    "prompts/judges/review.md",
                    "README.md",
                    "docs/notes.txt",
                    "LICENSE",
                ]),
                false
            ),
            ReviewEffort::Medium
        );
        // Mixed docs + code is NOT docs-only -> stays High for s/m/l/None.
        assert_eq!(
            classify_review_effort(
                &m_docs_task,
                false,
                &files(&["docs/DESIGN.md", "crates/board/src/daemon.rs"]),
                false
            ),
            ReviewEffort::High
        );
        // Build/dependency `.txt` files are NOT docs-only.
        let s_task = make_task("Bump dependencies", "", Some(TaskSize::S));
        assert_eq!(
            classify_review_effort(&s_task, false, &files(&["requirements.txt"]), false),
            ReviewEffort::High
        );
        assert_eq!(
            classify_review_effort(&s_task, false, &files(&["CMakeLists.txt"]), false),
            ReviewEffort::High
        );
        // Truncated file lists must NOT be assumed docs-only.
        assert_eq!(
            classify_review_effort(&m_docs_task, false, &files(&["docs/DESIGN.md"]), true),
            ReviewEffort::High
        );
        // Empty file list with unestimated size stays High conservatively.
        let unestimated = make_task("Review something", "", None);
        assert_eq!(
            classify_review_effort(&unestimated, false, &[], false),
            ReviewEffort::High
        );

        // 5. After-merge verification reviews drop to Medium regardless of task size/title.
        let large_sec = make_task("Harden OAuth token flow", "", Some(TaskSize::L));
        assert_eq!(
            classify_review_effort(&large_sec, true, &files(&["src/oauth.rs"]), false),
            ReviewEffort::Medium
        );
    }

    #[tokio::test]
    async fn pull_dispatch_review_applies_adaptive_thought_level_end_to_end() {
        use board_proto::{
            AddNote, CreateBoard, CreateRun, CreateTask, NoteKind, Priority, RegisterAgent,
            RunOrigin, TaskSize, UpdateTask,
        };
        use board_server::{AppState, Db, ManualClock, ServerConfig};
        use tokio::net::TcpListener;

        let pr_map: Arc<std::sync::Mutex<BTreeMap<String, crate::gh::PullRequest>>> =
            Arc::new(std::sync::Mutex::new(BTreeMap::new()));

        struct MapGh(Arc<std::sync::Mutex<BTreeMap<String, crate::gh::PullRequest>>>);

        impl Gh for MapGh {
            fn view<'a>(
                &'a self,
                _project: &'a Path,
                url: &'a str,
            ) -> BoxFuture<'a, Result<crate::gh::PullRequest>> {
                let found = self.0.lock().unwrap().get(url).cloned();
                let url = url.to_owned();
                Box::pin(
                    async move { found.ok_or_else(|| anyhow::anyhow!("unknown test PR {url}")) },
                )
            }

            fn ready<'a>(&'a self, _project: &'a Path, _pr: &'a str) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }

            fn update_branch<'a>(
                &'a self,
                _project: &'a Path,
                _pr: &'a str,
            ) -> BoxFuture<'a, Result<crate::gh::Updated>> {
                Box::pin(async { Ok(crate::gh::Updated::Done) })
            }

            fn merge<'a>(
                &'a self,
                _project: &'a Path,
                _pr: &'a str,
                _head: &'a str,
            ) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }

            fn commit_check_status<'a>(
                &'a self,
                _project: &'a Path,
                _pr_url: Option<&'a str>,
                _sha: &'a str,
                _check_name: &'a str,
            ) -> BoxFuture<'a, Result<crate::gh::Checks>> {
                Box::pin(async { Ok(crate::gh::Checks::Passed) })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("server.db");
        let db = Db::open(&db_path).unwrap();
        let t0: Timestamp = "2026-11-27T10:00:00Z".parse().unwrap();
        let clock = Arc::new(ManualClock::new(t0));
        let state = AppState::new(db, &ServerConfig::new(db_path), clock);
        let dev: DeviceName = "mac-review".parse().unwrap();
        let token = state.create_device(&dev, true).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let srv_state = state.clone();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, board_server::app(srv_state)).await;
        });

        let client_as =
            |agent: Option<AgentId>| Client::new(&base_url, token.clone(), agent, None).unwrap();
        let worker_id: AgentId = "claude:worker-rev".parse().unwrap();
        let review_id: AgentId = "antigravity:run-prev-review".parse().unwrap();
        let dispatch_id: AgentId = format!("dispatch:{dev}").parse().unwrap();
        let admin_client = client_as(Some(dispatch_id.clone()));
        let worker_client = client_as(Some(worker_id.clone()));
        let review_client = client_as(Some(review_id.clone()));

        for (c, id, role) in [
            (&worker_client, &worker_id, AgentRole::Worker),
            (&review_client, &review_id, AgentRole::Worker),
            (&admin_client, &dispatch_id, AgentRole::Coordinator),
        ] {
            c.register(&RegisterAgent {
                id: id.clone(),
                device: dev.clone(),
                runtime: Runtime::Antigravity,
                role,
                repo: Some("github.com/o/r".into()),
                branch: None,
                cwd: None,
                title: None,
                forked_from: None,
                native_ids: BTreeMap::new(),
            })
            .await
            .unwrap();
        }

        admin_client
            .create_board(&CreateBoard {
                slug: "factory".parse().unwrap(),
                title: "Factory".into(),
                goal: String::new(),
                repos: vec!["github.com/o/r".into()],
                autopilot: true,
                home_devices: vec![dev.clone()],
                temporary: false,
            })
            .await
            .unwrap();

        // Set up a real local git repository + bare origin with distinct PR branches.
        let origin = dir.path().join("origin.git");
        let repo_dir = dir.path().join("repo");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::create_dir_all(&repo_dir).unwrap();
        let run_git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        run_git(&origin, &["init", "--bare", "-b", "main"]);
        run_git(&repo_dir, &["init", "-b", "main"]);
        run_git(
            &repo_dir,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        std::fs::create_dir_all(repo_dir.join("src")).unwrap();
        std::fs::create_dir_all(repo_dir.join("docs")).unwrap();
        std::fs::create_dir_all(repo_dir.join("prompts/judges")).unwrap();
        std::fs::write(repo_dir.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(
            repo_dir.join("src/auth.rs"),
            "pub fn check() -> bool { true }\n",
        )
        .unwrap();
        std::fs::write(repo_dir.join("docs/DESIGN.md"), "# Design\n").unwrap();
        std::fs::write(repo_dir.join("prompts/judges/review.md"), "# Review\n").unwrap();
        run_git(&repo_dir, &["add", "."]);
        run_git(&repo_dir, &["commit", "-m", "init"]);
        let main_sha = run_git(&repo_dir, &["rev-parse", "HEAD"]);
        run_git(&repo_dir, &["push", "-u", "origin", "main"]);

        let make_branch = |branch: &str, file: &str, contents: &str| -> String {
            run_git(&repo_dir, &["checkout", "-b", branch, "main"]);
            std::fs::write(repo_dir.join(file), contents).unwrap();
            run_git(&repo_dir, &["add", file]);
            run_git(&repo_dir, &["commit", "-m", branch]);
            let sha = run_git(&repo_dir, &["rev-parse", "HEAD"]);
            run_git(&repo_dir, &["push", "-u", "origin", branch]);
            run_git(&repo_dir, &["checkout", "main"]);
            sha
        };

        let create_review_task = |title: &str, size: Option<TaskSize>| {
            let worker_client = worker_client.clone();
            let worker_id = worker_id.clone();
            let title = title.to_owned();
            async move {
                let created = worker_client
                    .create_task(&CreateTask {
                        agent: worker_id.clone(),
                        board: "factory".parse().unwrap(),
                        title,
                        body: String::new(),
                        acceptance: String::new(),
                        priority: Priority::P2,
                        parent: None,
                        blockers: vec![],
                        auto: Some(true),
                        claim: true,
                        not_before: None,
                        rank: None,
                        rank_top: false,
                        repo: Some("github.com/o/r".into()),
                        weight: None,
                        resource_units: None,
                        requires: vec![],
                        tolerations: vec![],
                        conflict_keys: vec![],
                        affinity: None,
                        preferred_runner: None,
                        origin: None,
                        size,
                        before: None,
                        after: None,
                    })
                    .await
                    .unwrap();
                worker_client
                    .update_task(
                        &created.id,
                        &UpdateTask {
                            agent: Some(worker_id.clone().into()),
                            version: created.version,
                            status: Some(TaskStatus::Review),
                            title: None,
                            body: None,
                            acceptance: None,
                            priority: None,
                            blockers: None,
                            blocked_reason: None,
                            auto: None,
                            not_before: None,
                            rank: None,
                            owner: None,
                            reason: None,
                            repo: None,
                            weight: None,
                            resource_units: None,
                            requires: None,
                            tolerations: None,
                            conflict_keys: None,
                            affinity: None,
                            preferred_runner: None,
                            size: None,
                            cooled: None,
                            before: None,
                            after: None,
                        },
                    )
                    .await
                    .unwrap();
                created.id
            }
        };

        // Mark worker idle so pre-merge reviews are immediately eligible.
        worker_client
            .set_state(
                &worker_id,
                &board_proto::AgentStateUpdate {
                    state: AgentState::Idle,
                    paused_until: None,
                    said: None,
                    token_usage: None,
                },
            )
            .await
            .unwrap();

        let fake_runner = crate::runner::FakeRunner::new("acp");
        let mut daemon_cfg = Config {
            server: Some(base_url.clone()),
            device: Some(dev.clone()),
            dispatch_mode: DispatchMode::Pull,
            judge_models: crate::config::JudgeModels {
                review: Some(crate::config::ThreadModel {
                    provider: "antigravity".into(),
                    model: "gemini-4-neon".into(),
                    options: [("thought_level".into(), "high".into())].into(),
                }),
                ..Default::default()
            },
            ..Config::default()
        };
        daemon_cfg
            .projects
            .insert("github.com/o/r".into(), repo_dir.clone());
        let ctx = Ctx {
            home: Home::at(dir.path().join("home")),
            config: daemon_cfg,
            client: client_as(None),
            device: dev.clone(),
            repo: crate::identity::RepoInfo::default(),
            known: None,
        };
        let mut daemon = Daemon::new(ctx, T3Cli::locate(None))
            .with_runner(fake_runner.clone())
            .with_gh(MapGh(Arc::clone(&pr_map)));
        daemon.send_heartbeat(t0).await.unwrap();

        let user_client = client_as(None);

        // Helper: drive one review task, read its spawned thought_level, then close task & run.
        async fn run_case(
            daemon: &mut Daemon<T3Cli>,
            fake_runner: &crate::runner::FakeRunner,
            user_client: &Client,
            task_id: TaskId,
            t: Timestamp,
        ) -> Option<String> {
            let before = fake_runner.spawns().len();
            daemon.reconcile(t).await.unwrap();
            let spawns = fake_runner.spawns();
            assert_eq!(
                spawns.len(),
                before + 1,
                "expected 1 spawned review for {task_id}"
            );
            let spec = &spawns[before].spec;
            let thought = spec
                .model
                .as_ref()
                .and_then(|m| m.options.get("thought_level"))
                .cloned();
            // Cancel the server run and drop the task so the next case runs cleanly.
            let run_id: RunId = spec.run_id.parse().unwrap();
            user_client
                .update_run(
                    &run_id,
                    &UpdateRun {
                        state: Some(RunState::Cancelled),
                        ..UpdateRun::default()
                    },
                )
                .await
                .unwrap();
            let _ = daemon.db().update_local_run_state(
                run_id.as_str(),
                LocalRunState::Cancelled,
                None,
                t,
            );
            let _ = daemon.db().remove_resource(&format!("run:{run_id}"));
            let detail = user_client.task(&task_id).await.unwrap();
            user_client
                .update_task(
                    &task_id,
                    &UpdateTask {
                        agent: None,
                        version: detail.task.version,
                        status: Some(TaskStatus::Dropped),
                        reason: Some("done testing case".into()),
                        title: None,
                        body: None,
                        acceptance: None,
                        priority: None,
                        blockers: None,
                        blocked_reason: None,
                        auto: None,
                        not_before: None,
                        rank: None,
                        owner: None,
                        repo: None,
                        weight: None,
                        resource_units: None,
                        requires: None,
                        tolerations: None,
                        conflict_keys: None,
                        affinity: None,
                        preferred_runner: None,
                        size: None,
                        cooled: None,
                        before: None,
                        after: None,
                    },
                )
                .await
                .unwrap();
            thought
        }

        let register_pr_for_task =
            |task_id: &TaskId, pr_num: u32, head_ref: &str, head_oid: String| {
                let url = format!("https://github.com/o/r/pull/{pr_num}");
                pr_map.lock().unwrap().insert(
                    url.clone(),
                    crate::gh::PullRequest {
                        url: url.clone(),
                        state: PrState::Open,
                        draft: true,
                        base_ref: "main".to_owned(),
                        head_ref: head_ref.to_owned(),
                        head_oid,
                        cross_repository: false,
                        merge_commit: None,
                        merge_state: crate::gh::MergeState::Clean,
                        review_decision: crate::gh::ReviewDecision::None,
                        checks: crate::gh::Checks::Passed,
                        passed_checks: vec!["rust".to_owned()],
                    },
                );
                let _ = task_id;
                url
            };

        let park_worker_idle = || {
            let worker_client = worker_client.clone();
            let worker_id = worker_id.clone();
            async move {
                worker_client
                    .set_state(
                        &worker_id,
                        &board_proto::AgentStateUpdate {
                            state: AgentState::Idle,
                            paused_until: None,
                            said: None,
                            token_usage: None,
                        },
                    )
                    .await
                    .unwrap();
            }
        };

        // Case A: Size::M code PR -> thought_level = "high".
        let t_m = create_review_task("Implement scheduler rule", Some(TaskSize::M)).await;
        let b_m = format!("task/{t_m}");
        let sha_m = make_branch(&b_m, "src/lib.rs", "pub fn a() { 1; }\n");
        let pr_m = register_pr_for_task(&t_m, 101, &b_m, sha_m);
        let _ = worker_client
            .add_note(
                &t_m,
                &AddNote {
                    agent: worker_id.clone(),
                    kind: NoteKind::Note,
                    text: format!("pr: {pr_m}"),
                },
            )
            .await
            .unwrap();
        park_worker_idle().await;
        assert_eq!(
            run_case(&mut daemon, &fake_runner, &user_client, t_m, t0)
                .await
                .as_deref(),
            Some("high"),
            "m code PR must stay high"
        );

        // Case B: Size::Xs security-sensitive PR (src/auth.rs) -> thought_level = "high".
        let t_sec = create_review_task("Tweak check helper", Some(TaskSize::Xs)).await;
        let b_sec = format!("task/{t_sec}");
        let sha_sec = make_branch(&b_sec, "src/auth.rs", "pub fn check() -> bool { false }\n");
        let pr_sec = register_pr_for_task(&t_sec, 102, &b_sec, sha_sec);
        let _ = worker_client
            .add_note(
                &t_sec,
                &AddNote {
                    agent: worker_id.clone(),
                    kind: NoteKind::Note,
                    text: format!("pr: {pr_sec}"),
                },
            )
            .await
            .unwrap();
        park_worker_idle().await;
        assert_eq!(
            run_case(
                &mut daemon,
                &fake_runner,
                &user_client,
                t_sec,
                t0 + TimeDelta::minutes(1)
            )
            .await
            .as_deref(),
            Some("high"),
            "xs security-sensitive diff must stay high"
        );

        // Case C: Size::Xs non-security code PR -> thought_level = "medium".
        let t_xs = create_review_task("Fix tiny formatting bug", Some(TaskSize::Xs)).await;
        let b_xs = format!("task/{t_xs}");
        let sha_xs = make_branch(&b_xs, "src/lib.rs", "pub fn a() { 2; }\n");
        let pr_xs = register_pr_for_task(&t_xs, 103, &b_xs, sha_xs);
        let _ = worker_client
            .add_note(
                &t_xs,
                &AddNote {
                    agent: worker_id.clone(),
                    kind: NoteKind::Note,
                    text: format!("pr: {pr_xs}"),
                },
            )
            .await
            .unwrap();
        park_worker_idle().await;
        assert_eq!(
            run_case(
                &mut daemon,
                &fake_runner,
                &user_client,
                t_xs,
                t0 + TimeDelta::minutes(2)
            )
            .await
            .as_deref(),
            Some("medium"),
            "xs non-security PR must downgrade to medium"
        );

        // Case D: Size::M docs/prompt-only PR -> thought_level = "medium".
        let t_docs = create_review_task("Rewrite review guidance", Some(TaskSize::M)).await;
        let b_docs = format!("task/{t_docs}");
        let sha_docs = make_branch(
            &b_docs,
            "prompts/judges/review.md",
            "# Updated Review Prompt\n",
        );
        let pr_docs = register_pr_for_task(&t_docs, 104, &b_docs, sha_docs);
        let _ = worker_client
            .add_note(
                &t_docs,
                &AddNote {
                    agent: worker_id.clone(),
                    kind: NoteKind::Note,
                    text: format!("pr: {pr_docs}"),
                },
            )
            .await
            .unwrap();
        park_worker_idle().await;
        assert_eq!(
            run_case(
                &mut daemon,
                &fake_runner,
                &user_client,
                t_docs,
                t0 + TimeDelta::minutes(3)
            )
            .await
            .as_deref(),
            Some("medium"),
            "docs/prompt-only PR must downgrade to medium"
        );

        // Case E: After-merge verification review -> thought_level = "medium".
        let t_after = create_review_task("Harden OAuth service", Some(TaskSize::L)).await;
        park_worker_idle().await;
        let prev_run = admin_client
            .create_run(&CreateRun {
                task_id: Some(t_after.clone()),
                board: Some("factory".parse().unwrap()),
                kind: RunKind::Review,
                origin: RunOrigin::Managed,
                device: dev.clone(),
                runner: "acp".into(),
                agent_id: Some(review_id.clone()),
                state: Some(RunState::Running),
                lease_ttl_secs: Some(300),
                lease_expires_at: None,
                worktree_branch: None,
                cwd: None,
                title: Some("review".into()),
                attach_hint: None,
                cost_units: 2,
                policy_version: None,
                conflict_keys: vec![],
            })
            .await
            .unwrap();
        let pass_note = review_client
            .add_note(
                &t_after,
                &AddNote {
                    agent: review_id.clone(),
                    kind: NoteKind::Decision,
                    text: "review: pass. Clean.\npr: none\nafter merge: verify live metrics".into(),
                },
            )
            .await
            .unwrap();
        admin_client
            .update_run(
                &prev_run.id,
                &UpdateRun {
                    state: Some(RunState::Succeeded),
                    ..UpdateRun::default()
                },
            )
            .await
            .unwrap();
        admin_client
            .add_note(
                &t_after,
                &AddNote {
                    agent: dispatch_id.clone(),
                    kind: NoteKind::Note,
                    text: format!(
                        "{} review pass #{}. pr: none; merge_commit: {main_sha}.\nafter merge: verify live metrics",
                        judges::AFTER_MERGE_READY_NOTE,
                        pass_note.id
                    ),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            run_case(
                &mut daemon,
                &fake_runner,
                &user_client,
                t_after,
                t0 + TimeDelta::minutes(4)
            )
            .await
            .as_deref(),
            Some("medium"),
            "after-merge verification review must downgrade to medium"
        );

        state.shutdown();
        server_handle.abort();
    }

    #[test]
    fn llm_spawn_backoff_exponential_schedule_and_older_server_hold_clear() {
        let t0 = chrono::DateTime::parse_from_rfc3339("2026-11-27T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let board: BoardSlug = "factory".parse().unwrap();
        let task_id: TaskId = "factory-42".parse().unwrap();
        let claimed = ClaimedRun {
            run: board_proto::Run {
                id: "run-000000000001".parse().unwrap(),
                task_id: Some(task_id.clone()),
                board: Some(board.clone()),
                kind: RunKind::Review,
                origin: board_proto::RunOrigin::Managed,
                device: "mac-pro".parse().unwrap(),
                runner: "t3".to_owned(),
                agent_id: None,
                state: RunState::Leased,
                lease_expires_at: Some(t0 + TimeDelta::minutes(3)),
                worktree_branch: None,
                cwd: None,
                title: Some("review factory-42".to_owned()),
                attach_hint: None,
                cost_units: 2,
                policy_version: None,
                outcome: None,
                transcript_uri: None,
                transcript_sha256: None,
                conflict_locks: Vec::new(),
                created_at: t0,
                started_at: None,
                ended_at: None,
                updated_at: t0,
            },
            board: board.clone(),
            task_id: Some(task_id.clone()),
            tasks: vec![task_id.clone()],
            score: 1.0,
            score_breakdown: board_proto::ScoreBreakdown::default(),
            conflict_keys: Vec::new(),
            housekeeping_action: None,
        };
        let stamp = Some((t0, 10));
        let mut backoff = LlmSpawnBackoff::default();

        // Exponential schedule: 30s -> 60s -> 120s -> 300s -> 300s.
        let (d1, u1, a1) = backoff.record_failure(&claimed, stamp, t0);
        assert_eq!((d1.num_seconds(), a1), (30, 1));
        assert!(
            !backoff.is_kind_held(RunKind::Review, t0),
            "kind hold stays off against servers with server-side backoff"
        );
        // Older server re-offers the unchanged task inside backoff -> engages kind hold.
        assert_eq!(
            backoff.duplicate_in_backoff(&claimed, stamp, t0 + TimeDelta::seconds(5)),
            Some((u1, 1))
        );
        assert!(backoff.is_kind_held(RunKind::Review, t0 + TimeDelta::seconds(5)));
        assert!(!backoff.is_kind_held(RunKind::Review, u1));

        // 2nd, 3rd, 4th, 5th failures scale 60s -> 120s -> 300s -> 300s.
        let (d2, u2, a2) = backoff.record_failure(&claimed, stamp, u1);
        assert_eq!((d2.num_seconds(), a2), (60, 2));
        let (d3, u3, a3) = backoff.record_failure(&claimed, stamp, u2);
        assert_eq!((d3.num_seconds(), a3), (120, 3));
        let (d4, u4, a4) = backoff.record_failure(&claimed, stamp, u3);
        assert_eq!((d4.num_seconds(), a4), (300, 4));
        let (d5, _u5, a5) = backoff.record_failure(&claimed, stamp, u4);
        assert_eq!((d5.num_seconds(), a5), (300, 5));

        // Engage kind hold, then verify a newer NoteAdded event clears it and wakes reconcile.
        assert!(
            backoff
                .duplicate_in_backoff(&claimed, stamp, u4 + TimeDelta::seconds(1))
                .is_some()
        );
        assert!(backoff.is_kind_held(RunKind::Review, u4 + TimeDelta::seconds(1)));
        let note_ev = Event {
            seq: 99,
            at: u4 + TimeDelta::seconds(2),
            board: Some(board),
            actor: "t3:worker".to_owned(),
            event_type: EventType::NoteAdded,
            task: Some(task_id),
            payload: serde_json::json!({}),
        };
        assert!(backoff.on_event(&note_ev));
        assert!(!backoff.is_kind_held(RunKind::Review, u4 + TimeDelta::seconds(2)));

        // Runner success also clears pending entries for that runner.
        let _ = backoff.record_failure(&claimed, stamp, u4 + TimeDelta::seconds(5));
        assert!(
            backoff
                .duplicate_in_backoff(&claimed, stamp, u4 + TimeDelta::seconds(6))
                .is_some()
        );
        backoff.clear_runner("t3");
        assert!(!backoff.is_kind_held(RunKind::Review, u4 + TimeDelta::seconds(6)));
    }
}
