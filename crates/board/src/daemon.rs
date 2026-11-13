//! `board watch`: the device daemon.
//!
//! It follows the server's event stream and, on this device:
//! - dispatches each autopilot board that lists this device in `home_devices`: it holds the
//!   board's coordinator lease as `dispatch:<device>` and starts a worker thread for each
//!   next ready task while one of the board's `parallel_tasks` slots is free (see
//!   [`Worker`]). No long-lived coordinator thread holds work;
//! - starts the board's judges (review, verdict, goal and triage), merges the pull request
//!   of a task whose review passed and finishes that task (see [`judges`]);
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
use std::time::Duration;

use anyhow::{Context as _, Result};
use board_proto::{
    AddNote, Agent, AgentId, AgentRole, AgentSource, AgentState, Board, BoardSlug, BoardStatus,
    ClaimCoordinator, DeviceName, ErrorCode, Event, EventType, NoteKind, RegisterAgent, Runtime,
    Task, TaskId, TaskStatus, Timestamp, Usage,
};
use chrono::{TimeDelta, Utc};
use eventsource_stream::Eventsource as _;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::client::{Client, ServerError, TaskQuery, error_code, is_unreachable};
use crate::config::{Config, Home};
use crate::gh::{Gh, GhCli};
use crate::ops::Ctx;
use crate::state;
use crate::t3::{T3, T3Cli, TimedOut, folder_gone};
use crate::usage::UsageSource;

pub mod device;
pub mod judges;

pub use judges::{
    JUDGE_REGISTER_WAIT, JUDGE_RETRY, JUDGE_START_RETRY, JUDGE_TASKS_AT_ONCE, JUDGES_AT_ONCE,
    JudgeRetry, JudgeRun, Merge, PR_READ_EVERY,
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
/// How long a task whose worker never claimed it, whose start failed, or that its worker
/// gave back to `open` while no other task was in progress, waits before the dispatcher
/// starts another worker for it, so a worker that stops at once (a failing session) is not
/// started again on every reconcile. One given back while other tasks were in progress, as
/// a lease overlap does, waits instead until one of those ends ([`WatchState::worker_skips`]).
pub const WORKER_RETRY: TimeDelta = TimeDelta::minutes(30);
/// How long a usage-limit stop is remembered: it holds a further worker back only while a
/// task in progress started before it.
pub const USAGE_STOP_KEPT: TimeDelta = TimeDelta::days(7);

/// How long after the server's repeat idle notice the dispatcher waits before it sets the
/// task of an owner still away `open`: the notice also wakes the owner, which may come
/// back.
pub const RELEASE_GRACE: TimeDelta = TimeDelta::minutes(5);
/// How many tasks in `doing` the dispatcher reads per board to count its slots.
const DOING_READ: u32 = 100;
const STATE_NAME: &str = "watch";
/// Delay before starting workers on a board again after one failed to start, doubled per failure up to the cap.
pub const START_RETRY_FIRST: TimeDelta = TimeDelta::minutes(1);
pub const START_RETRY_MAX: TimeDelta = TimeDelta::minutes(30);
/// Events whose handling failed on a transient error are retried for this long.
pub const RETRY_EVENTS_FOR: TimeDelta = TimeDelta::minutes(30);
const RETRY_EVENTS_MAX: usize = 500;
/// The cap of the event stream's reconnect delay, which doubles per failed or short-lived
/// connection from [`Timing::reconnect_first`].
const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// A connection that stayed open this long resets the reconnect delay.
const STREAM_HEALTHY: Duration = Duration::from_secs(60);

/// How often the daemon acts, and how long each step may take, so a server or `t3` that
/// stops answering holds up only that step. Tests shorten them.
#[derive(Debug, Clone)]
pub struct Timing {
    /// The boards are reconciled this often.
    pub reconcile_every: Duration,
    /// Queued wake-ups are sent this often.
    pub flush_every: Duration,
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
    /// Tasks whose worker gave them back to `open` while other tasks were in progress (a
    /// lease overlap, `prompts/worker.md` step 3), with those tasks: no worker starts for
    /// one until a task in its set is no longer in progress. Read leniently like `wakes`.
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
    /// Boards this device dispatches where a task was finished since the last goal
    /// review: their goal review is due once nothing is ready or in progress.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub goal_due: BTreeSet<BoardSlug>,
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
    /// Its thread; `None` until `t3 thread start` returns. A start the daemon did not live
    /// to finish is found by `title` among the threads created since `started_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    pub title: String,
    /// The branch of its worktree, when it runs in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
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
}

impl WakeAbout {
    /// Its kind, as `watch.log` names it.
    fn kind(&self) -> &'static str {
        match self {
            Self::Unblocked { .. } => "unblocked",
            Self::Answered { .. } => "answered",
            Self::Idle { .. } => "idle",
        }
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
    /// An ended T3 agent on this device whose thread T3 has not been asked about yet. T3
    /// closes a Claude session that sits idle (after 30 minutes), and the session's end marks
    /// the agent ended, but the thread lives on and a message resumes it.
    fn needs_thread_check(&self, device: &DeviceName) -> bool {
        self.agent.state == AgentState::Ended
            && self.thread == ThreadCheck::NotAsked
            && &self.agent.device == device
            && self.agent.id.source() == AgentSource::T3
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
    fn of(seen: &Seen, device: &DeviceName, full_until: Option<Timestamp>, now: Timestamp) -> Self {
        match Self::alone(seen, device, now) {
            // Codex and opencode agents spend other limits.
            Self::Now | Self::Held
                if full_until.is_some() && seen.agent.runtime == Runtime::Claude =>
            {
                Self::Full
            }
            reach => reach,
        }
    }

    /// By the agent's own state, whatever the account's usage.
    fn alone(seen: &Seen, device: &DeviceName, now: Timestamp) -> Self {
        let agent = &seen.agent;
        if &agent.device != device {
            return Self::Never(Unreachable::Elsewhere);
        }
        if agent.id.source() != AgentSource::T3 {
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
    /// Known; `read` when just read from the server. `rechecks` holds the line of each
    /// wake-up still fresh, in order, when the agent can be woken now and no failed send
    /// is backing off.
    Known {
        seen: Box<Seen>,
        read: bool,
        rechecks: Option<Vec<Result<Option<String>>>>,
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

/// The last line of a wake-up message, before its marker. The lines above it name what
/// changed, and the agent's prompt hook adds the board delta (alerts and changed counts) to
/// the same prompt, so the message asks for no snapshot: that call would cost the agent one
/// more turn for what the prompt already holds.
const WAKE_END: &str = "Continue from this and the board delta in this prompt, if there is one.";

/// What this device's daemon last saved.
pub fn load_watch_state(home: &Home) -> WatchState {
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

pub struct Daemon<T: T3> {
    ctx: Ctx,
    t3: T,
    /// How the dispatcher reads, updates and merges pull requests.
    gh: Box<dyn Gh>,
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
    /// The boards that wait for one of the device's worker slots, and for a judge slot.
    worker_turns: device::Turns,
    judge_turns: device::Turns,
    /// The time of the last reconcile.
    last_pass: Option<Timestamp>,
    /// The threads with a run in progress on this device, read from T3 at most once in a
    /// reconcile, when a board first wants a slot; `None` before that.
    running: Option<BTreeSet<String>>,
    /// The tasks that carry a note of the device limit holding them, until they start.
    held_noted: BTreeSet<TaskId>,
    /// The holds by the device limit that the device's trail file has, until their task or
    /// judge starts: `worker <task>` and `judge <title>`.
    held: BTreeSet<String>,
}

impl<T: T3> Daemon<T> {
    /// A daemon acting as the device of `ctx` (whose client must carry no agent id).
    pub fn new(ctx: Ctx, t3: T) -> Self {
        // A file it cannot read is kept aside rather than lost on the next save.
        let state = state::load_named_keeping_unreadable(&ctx.home, STATE_NAME);
        let config_text = std::fs::read_to_string(ctx.home.config_path()).ok();
        Self {
            ctx,
            t3,
            gh: Box::new(GhCli::locate()),
            state,
            boards: BTreeMap::new(),
            failures: BTreeMap::new(),
            agents: BTreeMap::new(),
            sending: Vec::new(),
            config_text,
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
            worker_turns: device::Turns::default(),
            judge_turns: device::Turns::default(),
            last_pass: None,
            running: None,
            held_noted: BTreeSet::new(),
            held: BTreeSet::new(),
        }
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

    /// Whether the runs recorded for `slug` take slots of this device: it hosts the board.
    /// A paused board counts, since its threads still run. A run
    /// recorded for a board that left this device takes no slot: nothing here follows it
    /// to its end.
    fn counts_runs(&self, slug: &BoardSlug) -> bool {
        self.boards.get(slug).is_some_and(|b| self.hosts(b))
    }

    /// The workers this device has now, on every board.
    fn worker_runs(&self) -> usize {
        let workers = self.state.workers.keys();
        workers.filter(|t| self.counts_runs(&t.board())).count()
    }

    /// The runs of the dispatcher on this device now, workers and judges, on every board.
    fn own_runs(&self) -> usize {
        let judges = self.state.judges.iter();
        self.worker_runs() + judges.filter(|r| self.counts_runs(&r.board)).count()
    }

    /// The threads with a run in progress that are no run of the dispatcher counted in
    /// [`Self::own_runs`]: the user's threads, the threads agents launched, and the
    /// dispatcher's runs for a board this device no longer hosts. Empty until
    /// [`Self::read_running`] read them in this reconcile.
    fn other_turns(&self) -> Vec<&str> {
        let workers = self.state.workers.iter();
        let workers = workers.filter(|(task, _)| self.counts_runs(&task.board()));
        let judges = self.state.judges.iter();
        let judges = judges.filter(|run| self.counts_runs(&run.board));
        let own: BTreeSet<&str> = workers
            .filter_map(|(_, worker)| worker.thread.as_deref())
            .chain(judges.filter_map(|run| run.thread.as_deref()))
            .collect();
        let running = self.running.iter().flatten().map(String::as_str);
        running.filter(|thread| !own.contains(thread)).collect()
    }

    /// What takes the device's slots now: the dispatcher's runs, and the turns of every
    /// other thread that runs on the device.
    fn runs(&self) -> usize {
        self.own_runs() + self.other_turns().len()
    }

    /// Reads the threads that run on this device, once in a reconcile: before the first
    /// start it decides on. A device without a limit reads none.
    async fn read_running(&mut self) {
        if self.running.is_none() && self.run_limit() != usize::MAX {
            self.running = Some(device::running_threads(&self.t3, &self.ctx.config).await);
        }
    }

    /// What takes the device's slots now, in words: for the log and for the note on a task
    /// the limit holds.
    fn load(&self) -> String {
        let others = self.other_turns();
        let mut text = format!(
            "device {} has {} of {} runs: {} of the dispatcher, {} turns of other threads",
            self.ctx.device,
            self.runs(),
            self.run_limit(),
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
        text
    }

    /// Reads, updates and merges pull requests through `gh` instead of the `gh` CLI.
    #[must_use]
    pub fn with_gh(mut self, gh: impl Gh + 'static) -> Self {
        self.gh = Box::new(gh);
        self
    }

    /// Applies an edited `config.toml`: its `[projects]`, `[board_projects]` and
    /// `feedback_board` take effect at once, and boards whose start failed are tried again,
    /// since a missing project may be what failed. `runs_at_once` takes effect at once
    /// too. `server`, `device`, `t3_bin` and
    /// `[thread_model]` are fixed while the daemon runs, so changing them asks for a
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
        if (&new.server, &new.device, &new.t3_bin, &new.thread_model)
            != (
                &current.server,
                &current.device,
                &current.t3_bin,
                &current.thread_model,
            )
        {
            tracing::warn!(
                "{} changes server, device, t3_bin or thread_model: restart board watch to apply them",
                path.display()
            );
        }
        if new.runs_at_once != current.runs_at_once {
            current.runs_at_once = new.runs_at_once;
            tracing::info!("re-read {}: runs_at_once updated", path.display());
        }
        if (&new.projects, &new.board_projects, &new.feedback_board)
            != (
                &current.projects,
                &current.board_projects,
                &current.feedback_board,
            )
        {
            current.projects = new.projects;
            current.board_projects = new.board_projects;
            current.feedback_board = new.feedback_board;
            self.failures.clear();
            tracing::info!(
                "re-read {}: projects and feedback board updated",
                path.display()
            );
        }
    }

    /// Adds the start of a hold by the device limit to the device's trail
    /// (`state/device-holds.jsonl`), once until the held task or judge starts: the time,
    /// the board, what is held and the device's load. It is the record of a held judge,
    /// which gets no note, and of a held task whose note the server did not take.
    fn record_hold(
        &mut self,
        kind: &str,
        board: &BoardSlug,
        subject: &str,
        limit: usize,
        now: Timestamp,
    ) {
        let key = format!("{kind} {subject}");
        if self.held.contains(&key) {
            return;
        }
        let record = serde_json::json!({
            "at": now,
            "kind": kind,
            "board": board,
            "device": self.ctx.device,
            "subject": subject,
            "limit": limit,
            "cause": self.load(),
            "others": self.other_turns(),
        });
        match state::append_hold(&self.ctx.home, &record) {
            Ok(()) => {
                self.held.insert(key);
            }
            Err(err) => tracing::warn!("recording that the device limit holds {key}: {err:#}"),
        }
    }

    /// Records on `task` that the device limit holds it, once until it starts, so that a
    /// reader of the board sees why a ready task did not start and what ran instead. A
    /// note that fails is logged and tried again; the hold is in the device's trail file
    /// either way.
    async fn note_held(&mut self, task: &TaskId, agent: &AgentId, workers: usize, load: &str) {
        if !self.held_noted.insert(task.clone()) {
            return;
        }
        let text = clip_note(format!(
            "The device limit holds this task: {load}; workers may take {workers}. The \
             dispatcher starts it when a slot is free, after the boards that waited longer. \
             The device keeps each hold in state/device-holds.jsonl."
        ));
        let client = self.client().with_agent(Some(agent.clone()));
        if let Err(err) = add_note(&client, task, agent, NoteKind::Note, text).await {
            self.held_noted.remove(task);
            tracing::warn!("noting on {task} that the device limit holds it: {err:#}");
        }
    }

    pub fn save(&self) -> Result<()> {
        state::save_named(&self.ctx.home, STATE_NAME, &self.state)
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

    /// Dispatches the boards this device runs: each one's lease, workers, judges and merges.
    pub async fn reconcile(&mut self, now: Timestamp) -> Result<()> {
        // A board that did not ask for a slot again in the last pass lost its place.
        if let Some(last) = self.last_pass.replace(now) {
            self.worker_turns.keep_asked_since(last);
            self.judge_turns.keep_asked_since(last);
        }
        self.running = None;
        self.reload_config();
        let boards = self.client().boards().await?;
        self.boards = boards.iter().map(|b| (b.slug.clone(), b.clone())).collect();
        self.refresh_usage(now).await;
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
                continue;
            }
            if let Err(err) = self.dispatch(board, now).await {
                tracing::warn!("dispatcher of {}: {err:#}", board.slug);
            }
        }
        let boards = &self.boards;
        self.failures.retain(|slug, _| boards.contains_key(slug));
        self.state
            .workers
            .retain(|task, _| boards.contains_key(&task.board()));
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
        self.save()
    }

    /// Whether T3 has `thread` and can run it: not archived, and its folder (its worktree,
    /// else `project` when given) still exists, since T3 fails every run of a thread whose
    /// folder is gone.
    async fn thread_live(&self, thread: &str, project: Option<&Path>) -> Result<bool> {
        let limit = self.timing.thread_check;
        match tokio::time::timeout(limit, self.t3.thread_info(thread)).await {
            Ok(info) => Ok(info?.is_some_and(|t| t.runnable(project))),
            Err(_) => Err(TimedOut(format!("t3 thread show {thread}")).into()),
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

    /// `thread`'s [`crate::t3::RECENT_MESSAGES`] latest messages, read within
    /// [`Timing::thread_check`]; `None` when T3 does not know the thread.
    async fn read_messages(&self, thread: &str) -> Result<Option<crate::t3::RecentMessages>> {
        tokio::time::timeout(self.timing.thread_check, self.t3.recent_messages(thread))
            .await
            .unwrap_or_else(|_| Err(TimedOut(format!("t3 thread show {thread}")).into()))
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
            look.read = Some(match self.read_messages(agent.native()).await {
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

    /// Runs a home board: holds its coordinator lease as this
    /// device's dispatch agent, does the steps it owes for agents no device can wake, follows
    /// the workers it started there, and starts workers for the next ready tasks while slots
    /// are free, the board's usage allows new work and no failed start waits for its retry
    /// (`docs/DESIGN.md`, "Daemon dispatch", Slots).
    async fn dispatch(&mut self, board: &Board, now: Timestamp) -> Result<()> {
        let slug = &board.slug;
        let agent = self.dispatch_agent();
        // A device that cannot start workers leaves the lease to a home device that can.
        if let Err(err) = self.host_folder(board) {
            if board.coordinator.as_ref() == Some(&agent) {
                self.release(slug).await;
            }
            self.dispatch_renewed.remove(slug);
            return Err(err);
        }
        if !self.hold_dispatch_lease(board, &agent, now).await? {
            return Ok(());
        }
        self.pay_owed(slug, &agent, now).await;
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
        let mut in_progress: BTreeMap<TaskId, InProgress> = doing
            .into_iter()
            .filter(|t| t.status == TaskStatus::Doing)
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
        let mut left = Vec::new();
        for (task, followed) in followed {
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
        // A worker that gave its task back while other tasks were in progress stopped on a
        // lease overlap, by its brief: that task waits until one of them ends. With nothing
        // else in progress it stopped for another reason, and waits out the retry.
        for task in left {
            let others: BTreeSet<TaskId> = in_progress
                .keys()
                .filter(|t| **t != task)
                .cloned()
                .collect();
            if others.is_empty() {
                tracing::warn!(
                    "the worker of {task} left it open; no new worker for it before {}",
                    now + WORKER_RETRY
                );
                self.state.worker_retry.insert(task, now + WORKER_RETRY);
            } else {
                tracing::info!(
                    "the worker of {task} left it open beside {}; it starts again once one of them ends",
                    others
                        .iter()
                        .map(|t| t.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                self.state.worker_skips.insert(task, others);
            }
        }
        self.state.worker_skips.retain(|task, others| {
            task.board() != *slug || others.iter().all(|t| in_progress.contains_key(t))
        });
        self.state
            .idle_notices
            .retain(|task, _| task.board() != *slug || in_progress.contains_key(task));
        self.save()?;
        // A failure here leaves the workers their turn.
        if let Err(err) = self
            .judge_and_close(board, &agent, in_progress.len(), now)
            .await
        {
            tracing::warn!("the judges and reviewed tasks of {slug}: {err:#}");
        }
        self.save()?;
        // The board asks again below if it still wants one of the device's slots.
        let mut waited = self.worker_turns.take(slug);
        let slots = self.pace_limit(board.parallel_tasks.max(1), now) as usize;
        if in_progress.len() >= slots || self.usage_holds(slug, now) {
            return Ok(());
        }
        if let Some(failure) = self.failures.get(slug)
            && failure.config == HostingConfig::of(board)
            && now < failure.retry_at
        {
            return Ok(());
        }
        if !in_progress.is_empty() && !self.may_start_beside(slug, &in_progress).await? {
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
            let limit = device::worker_limit(self.run_limit());
            if self.worker_runs() >= limit
                || self.runs() >= self.run_limit()
                || self.worker_turns.another_first(slug, waited)
            {
                let load = self.load();
                if self.worker_turns.wait(slug, waited, now) {
                    tracing::info!(
                        "{slug}: {} waits for one of the {limit} worker slots of this device; {load}",
                        task.id
                    );
                }
                // Each task the limit holds gets its record, not only the one that
                // started the board's wait.
                self.record_hold("worker", slug, task.id.as_str(), limit, now);
                self.note_held(&task.id, &agent, limit, &load).await;
                break;
            }
            self.held_noted.remove(&task.id);
            self.held.remove(&format!("worker {}", task.id));
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
    /// first of them started.
    async fn may_start_beside(
        &self,
        slug: &BoardSlug,
        in_progress: &BTreeMap<TaskId, InProgress>,
    ) -> Result<bool> {
        if in_progress.values().any(|p| p.task.is_none()) {
            return Ok(false);
        }
        let Some(first) = in_progress.values().map(|p| p.since).min() else {
            return Ok(true);
        };
        for (agent, at) in &self.state.usage_stops {
            if *at < first {
                continue;
            }
            let member = match self.client().agent_info(agent).await {
                Ok(info) => info.boards.contains(slug),
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => false,
                Err(err) => return Err(err.context("reading an agent that stopped on usage")),
            };
            if member {
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
                self.boards.insert(slug.clone(), updated);
                self.dispatch_renewed.insert(slug.clone(), now);
                Ok(true)
            }
            Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => {
                self.dispatch_renewed.remove(slug);
                Ok(false)
            }
            Err(err) => {
                // The server may have lost the agent: registered again on the next try.
                self.dispatch_registered = false;
                Err(err)
            }
        }
    }

    async fn register_dispatch(&self, agent: &AgentId) -> Result<()> {
        self.client()
            .with_agent(Some(agent.clone()))
            .register(&RegisterAgent {
                id: agent.clone(),
                // The runtime of the threads it starts.
                runtime: Runtime::Claude,
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
        let worker_agent = thread_agent(&thread)?;
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
            if let Err(err) = self.t3.send(&found.id, &brief).await {
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
        let link = match self.t3.thread_link(thread).await {
            Ok(Some(link)) => link,
            Ok(None) => format!("thread {thread}"),
            Err(err) => {
                tracing::debug!("reading the link of thread {thread}: {err:#}");
                format!("thread {thread}")
            }
        };
        let branch = worker
            .branch
            .as_ref()
            .map(|b| format!(" on branch {b}"))
            .unwrap_or_default();
        let text = format!("note: the dispatcher started a worker for this task{branch}: {link}");
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
            .filter(|t| t.ready && t.auto && !is_proposal(t) && t.feedback.is_none())
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

    /// Starts a worker thread for `task`, in a new worktree on `task/<id>` when the project
    /// folder is a repository's top, with the worker brief; records it before the start, so
    /// a start the daemon does not live to finish is found again, then notes it on the task.
    async fn start_worker(
        &mut self,
        board: &Board,
        task: &Task,
        agent: &AgentId,
        now: Timestamp,
    ) -> Result<()> {
        let project = self.host_folder(board)?;
        let worktree = crate::worktree::worker_worktree(&project, &task.id, now)
            .await
            .context("preparing the worker's worktree")?;
        let title = format!("{} {}", task.id, task.title);
        let brief = crate::brief::worker_brief(&board.slug, &task.id, crate::brief::WORKER_CLOSER);
        let mut worker = Worker {
            thread: None,
            title,
            branch: worktree.as_ref().map(|w| w.branch.clone()),
            started_at: now,
            claimed: false,
            noted: false,
        };
        // On disk before T3 can create the thread.
        self.state.workers.insert(task.id.clone(), worker.clone());
        self.save()?;
        let thread = self
            .t3
            .start_thread(&project, &worker.title, &brief, worktree.as_ref())
            .await?;
        // A thread id that names no agent could never be followed, and would hold the slot.
        thread_agent(&thread)
            .with_context(|| format!("t3 started thread {thread:?}; archive it in T3"))?;
        tracing::info!(
            "started worker thread {thread} for {} on {}",
            task.id,
            board.slug
        );
        worker.thread = Some(thread.clone());
        self.state.workers.insert(task.id.clone(), worker.clone());
        self.save()?;
        match self.note_worker(&task.id, &thread, &worker, agent).await {
            Ok(()) => {
                worker.noted = true;
                self.state.workers.insert(task.id.clone(), worker);
            }
            // Noted on a later reconcile.
            Err(err) => tracing::warn!("noting worker thread {thread} on {}: {err:#}", task.id),
        }
        Ok(())
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
            }
        }
        let (mut reconcile, wakes) = self.examine(event).await?;
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
                if asker.source() == AgentSource::T3 || asker == dispatcher {
                    return false;
                }
                self.owe(Owed::Answer {
                    board: slug.clone(),
                    ask,
                    asker,
                    check_gone: true,
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
                if !repeat {
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
                self.state.goal_due.insert(slug.clone());
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

    /// Does the steps owed on `slug` as its dispatch agent `agent`. A step that waits, or
    /// fails on a transient error, is tried again on the next reconcile, until [`WAKE_TTL`]
    /// after it was owed. Each step stays in the state until it is done, so a reconcile cut
    /// short loses none.
    async fn pay_owed(&mut self, slug: &BoardSlug, agent: &AgentId, now: Timestamp) {
        let steps: Vec<Owed> = (self.state.owed.iter())
            .filter(|s| s.board() == slug)
            .cloned()
            .collect();
        for mut step in steps {
            let keep = if now - step.at() >= WAKE_TTL {
                tracing::warn!("dispatcher of {slug}: {step:?} was not done in time; dropped");
                false
            } else {
                match self.pay(&mut step, agent, now).await {
                    Ok(Paid::Opened(task)) => {
                        // It starts again at once, with a new worker.
                        self.state.workers.remove(&task);
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
        }
    }

    /// Does one owed step, as of `now`.
    async fn pay(&self, step: &mut Owed, agent: &AgentId, now: Timestamp) -> Result<Paid> {
        let client = self.client().with_agent(Some(agent.clone()));
        match step {
            Owed::Answer {
                ask,
                asker,
                check_gone,
                noted,
                ..
            } => {
                let found = client
                    .ask(*ask)
                    .await
                    .with_context(|| format!("reading ask #{ask}"))?;
                let (Some(task), Some(answer)) = (found.task, found.answer) else {
                    return Ok(Paid::Nothing);
                };
                // An asker still present may read the answer, or end without it.
                if *check_gone && !gone(&client, asker).await? {
                    return Ok(Paid::Later);
                }
                if !*noted {
                    let text = clip_note(format!(
                        "decision: the user answered ask #{ask} from {asker}, which cannot be woken: {answer}"
                    ));
                    add_note(&client, &task, agent, NoteKind::Decision, text).await?;
                    *noted = true;
                }
                let current = match client.task(&task).await {
                    Ok(detail) => detail.task,
                    Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                        return Ok(Paid::Nothing);
                    }
                    Err(err) => return Err(err),
                };
                let held = matches!(
                    current.status,
                    TaskStatus::Doing | TaskStatus::Blocked | TaskStatus::Review
                ) && current.owner.as_ref().is_none_or(|o| o == asker);
                if !held {
                    return Ok(Paid::Nothing);
                }
                client
                    .task_action(&task, "release")
                    .await
                    .with_context(|| format!("setting {task} open after ask #{ask}"))?;
                tracing::info!("{task}: ask #{ask} answered for gone {asker}; set open");
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
                // The repeat notice also wakes the owner: it may still come back.
                if now - *noticed < RELEASE_GRACE {
                    return Ok(Paid::Later);
                }
                let current = match client.task(task).await {
                    Ok(detail) => detail.task,
                    Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                        return Ok(Paid::Nothing);
                    }
                    Err(err) => return Err(err),
                };
                if current.status != TaskStatus::Doing || current.owner.as_ref() != Some(away) {
                    return Ok(Paid::Nothing);
                }
                if self.back_since(away, *noticed).await? {
                    return Ok(Paid::Nothing);
                }
                if !*noted {
                    let info = match client.agent_info(away).await {
                        Ok(info) => Some(info),
                        Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => None,
                        Err(err) => return Err(err),
                    };
                    let thread = info
                        .as_ref()
                        .and_then(|i| i.native_ids.get("t3_link"))
                        .map_or_else(|| away.to_string(), |link| format!("{away} ({link})"));
                    let branch = self
                        .state
                        .workers
                        .get(task)
                        .and_then(|w| w.branch.clone())
                        .or_else(|| info.and_then(|i| i.branch))
                        .map_or_else(|| "none recorded".to_owned(), |b| format!("`{b}`"));
                    let text = clip_note(format!(
                        "note: the owner was away from this task since {} and still was after the \
                         server's repeat notice, or its thread is gone, so the dispatcher set it \
                         open for a new worker. Old thread: {thread}. Branch: {branch}.",
                        since.format("%Y-%m-%d %H:%MZ")
                    ));
                    add_note(&client, task, agent, NoteKind::Note, text).await?;
                    *noted = true;
                }
                client
                    .task_action(task, "release")
                    .await
                    .with_context(|| format!("setting {task} open from away {away}"))?;
                tracing::info!("{task}: {away} away since {since}; set open");
                Ok(Paid::Opened(task.clone()))
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
    }

    /// What one event calls for: a reconcile, and wake-ups by agent, each with the board
    /// it is about. Only reads.
    async fn examine(&self, event: &Event) -> Result<(bool, Vec<(BoardSlug, AgentId, WakeAbout)>)> {
        let mut wakes = Vec::new();
        let Some(slug) = &event.board else {
            return Ok((false, wakes));
        };
        // On a daemon-dispatch board run here, a task that changes may become ready or free
        // a slot: the reconcile it asks for starts the next worker.
        let mut reconcile = self.dispatches_here(slug)
            && matches!(
                event.event_type,
                EventType::TaskCreated
                    | EventType::TaskReleased
                    | EventType::TaskUpdated
                    | EventType::TaskDone
            );
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
                if let Some(to) = self.wakeable(Some(&asker), event).cloned() {
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

    /// `agent` when it is a T3 thread that did not cause `event`. Whether it is on this
    /// device and awake is checked when the wake-up is sent.
    fn wakeable<'a>(&self, agent: Option<&'a AgentId>, event: &Event) -> Option<&'a AgentId> {
        agent.filter(|a| a.as_str() != event.actor && a.source() == AgentSource::T3)
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
        for (agent, unsure) in &mut self.state.unsure_wakes {
            unsure.messages.retain(|m| {
                let live = now - m.since < WAKE_TTL;
                if !live {
                    tracing::warn!(
                        "waking {agent}: a timed-out message was not found in time; what it \
                         carried is dropped"
                    );
                }
                live
            });
        }
        self.state
            .unsure_wakes
            .retain(|_, unsure| !unsure.messages.is_empty());
        let agents: BTreeSet<&AgentId> = self
            .state
            .wakes
            .keys()
            .chain(self.state.unsure_wakes.keys())
            .collect();
        let queues: Vec<(AgentId, WakeQueue, Option<UnsureWakes>)> = agents
            .into_iter()
            .map(|agent| {
                (
                    agent.clone(),
                    self.state.wakes.get(agent).cloned().unwrap_or_default(),
                    self.state.unsure_wakes.get(agent).cloned(),
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
                    let checked = this.check(agent, queue, now).await;
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
        self.sending = outgoing.iter().map(|out| out.agent.clone()).collect();
        let sends = outgoing
            .iter()
            .map(|out| self.send_wake(&out.agent, &out.message));
        let results = futures_util::future::join_all(sends).await;
        self.sending.clear();
        for (out, sent) in outgoing.into_iter().zip(results) {
            self.settle(out, sent, now);
        }
        let wakes = &self.state.wakes;
        self.agents.retain(|agent, _| wakes.contains_key(agent));
    }

    /// Reads what a flush needs to decide on `agent`'s `queue`: the agent, and each line
    /// when it can be sent now. Changes nothing.
    async fn check(&self, agent: &AgentId, queue: &WakeQueue, now: Timestamp) -> Checked {
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
                    };
                    (seen, true)
                }
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                    return Checked::Unknown;
                }
                Err(err) => return Checked::LookupFailed(err),
            },
        };
        // Asked once per state change, also while a failed send backs off. A thread that goes
        // later is found by the next send that fails, which asks T3 again.
        if seen.needs_thread_check(&self.ctx.device) {
            // Its folder: the worktree T3 names, else the one its session registered from.
            let registered = seen.agent.cwd.as_deref().map(Path::new);
            seen.thread = match self.thread_live(agent.native(), registered).await {
                Ok(true) => ThreadCheck::Live,
                Ok(false) => ThreadCheck::Gone,
                Err(err) => {
                    tracing::warn!(
                        "waking {agent}: checking its ended thread: {err:#}; trying the send"
                    );
                    ThreadCheck::NoAnswer
                }
            };
            read = true;
        }
        let due = backing_off.is_none() && matches!(self.reach(&seen, now), Reach::Now);
        let rechecks = if due {
            let waited_until = self.waited_until(&seen);
            let mut lines = Vec::new();
            for wake in queue
                .wakes
                .iter()
                .filter(|w| is_fresh(w, waited_until, now))
            {
                lines.push(self.recheck(agent, wake, now).await);
            }
            Some(lines)
        } else {
            None
        };
        Checked::Known {
            seen: Box::new(seen),
            read,
            rechecks,
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
        let (seen, rechecks) = match checked {
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
                return None;
            }
            Checked::LookupFailed(err) => {
                if is_transient(&err) {
                    let delay = back_off(&mut queue, now, true);
                    tracing::warn!("waking {agent}: {err:#}; trying again in {delay}");
                    self.requeue(agent.clone(), queue);
                } else {
                    tracing::warn!("waking {agent}: {err:#}; dropped");
                }
                return None;
            }
            Checked::Known {
                seen,
                read,
                rechecks,
            } => {
                if read {
                    self.agents.insert(agent.clone(), (*seen).clone());
                }
                (*seen, rechecks)
            }
        };
        let waited_until = self.waited_until(&seen);
        queue.wakes.retain(|w| {
            let fresh = is_fresh(w, waited_until, now);
            if !fresh {
                tracing::warn!("waking {agent}: not delivered in time; dropped");
            }
            fresh
        });
        match self.reach(&seen, now) {
            Reach::Now => {}
            // Kept whole for the agent going idle, or for the reset of the full usage
            // window.
            Reach::Held | Reach::Full => {
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
        for (wake, line) in std::mem::take(&mut queue.wakes).into_iter().zip(rechecks) {
            match line {
                Ok(Some(line)) => {
                    if !lines.contains(&line) {
                        lines.push(line);
                    }
                    sent.push(wake);
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!("waking {agent}: {err:#}");
                    if is_transient(&err) {
                        queue.wakes.push(wake);
                    }
                }
            }
        }
        self.requeue(agent.clone(), queue);
        if lines.is_empty() {
            return None;
        }
        Some(Outgoing {
            agent: agent.clone(),
            message: format!("{}\n{WAKE_END}", lines.join("\n")),
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

    /// Sends `message` to `agent`'s thread; on a failure, asks T3 whether the thread is gone.
    async fn send_wake(&self, agent: &AgentId, message: &str) -> Sent {
        let thread = agent.native();
        let err = match tokio::time::timeout(WAKE_SEND_TIMEOUT, self.t3.send(thread, message)).await
        {
            Ok(Ok(())) => return Sent::Delivered,
            Ok(Err(err)) if err.downcast_ref::<TimedOut>().is_some() => {
                return Sent::MaybeDelivered(err);
            }
            Ok(Err(err)) => err,
            Err(_) => {
                return Sent::MaybeDelivered(TimedOut(format!("t3 thread send {thread}")).into());
            }
        };
        match tokio::time::timeout(WAKE_SEND_TIMEOUT, self.t3.thread_info(thread)).await {
            Ok(Ok(None)) => Sent::Gone(err),
            Ok(Ok(Some(info))) if !info.runnable(None) => Sent::Gone(err),
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
        Reach::of(seen, &self.ctx.device, self.usage_full_until(now), now)
    }

    /// The reset `seen`'s wake-ups last waited for, from which they live: its own
    /// ([`paused_until`]), or for a Claude agent that of a full usage window, the later one.
    fn waited_until(&self, seen: &Seen) -> Option<Timestamp> {
        let full = self
            .state
            .usage_full_until
            .filter(|_| seen.agent.runtime == Runtime::Claude);
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

    /// How many of a board's `slots` the weekly pace leaves at `now`.
    fn pace_limit(&self, slots: u32, now: Timestamp) -> u32 {
        self.usage
            .as_ref()
            .map_or(slots, |u| u.pace_limit(slots, now))
    }

    /// How long after its last reading the daemon reads the usage endpoint again: every
    /// minute near a pace line, unless the endpoint refused a reading in the last 30
    /// minutes; else every 5 minutes.
    fn usage_poll_every(&self, now: Timestamp) -> TimeDelta {
        let refused = self
            .usage_refused
            .is_some_and(|at| now - at < USAGE_REFUSED_BACKOFF);
        let near = self.usage.as_ref().is_some_and(|u| u.near_line(now));
        if near && !refused {
            USAGE_POLL_NEAR
        } else {
            USAGE_POLL_EVERY
        }
    }

    /// Takes a new usage reading and reports it to the server: T3 Code's, while one of its
    /// files holds one that is new enough and can follow the reading known here
    /// ([`crate::usage::follows`]), the newest such; else the endpoint's, when [`Self::usage_poll_every`] has
    /// passed since this daemon last read it. While T3 Code's readings stand in, the endpoint
    /// is read only every [`USAGE_CHECK_EVERY`], unless the pace is near a line. After a
    /// refusal that says how long to wait, the endpoint is read again then. With no new
    /// reading, takes the newest any device reported. A failure keeps the last reading, which goes stale and
    /// then holds nothing.
    async fn refresh_usage(&mut self, now: Timestamp) {
        let mut reading = None;
        if let UsageSource::ClaudeCode {
            url,
            credentials,
            snapshots,
        } = &self.usage_source
        {
            let every = self.usage_poll_every(now);
            let snapshot = crate::usage::read_snapshots(snapshots, now)
                .await
                .into_iter()
                .filter(|s| now - s.read_at < every + USAGE_SNAPSHOT_SLACK)
                .filter(|s| {
                    self.usage
                        .as_ref()
                        .is_none_or(|known| crate::usage::follows(s, known))
                })
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
                        && self.usage.as_ref().is_none_or(|k| s.read_at > k.read_at)
                })
            {
                self.usage_snapshot_at = Some(snapshot.read_at);
                reading = Some(snapshot);
            }
        }
        let mut reported = None;
        if let Some(report) = reading {
            reported = Some(match self.client().report_usage(&report).await {
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
                        pace: self.usage.as_ref().and_then(|u| u.pace),
                    }
                }
            });
        }
        match reported {
            Some(usage) => self.usage = Some(usage),
            None => match self.client().usage().await {
                Ok(Some(usage)) => self.usage = Some(usage),
                Ok(None) => {}
                Err(err) => tracing::debug!("reading the usage from the server: {err:#}"),
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
            // Queued for another agent than the one it is about.
            WakeAbout::Idle { .. } | WakeAbout::Answered { .. } => None,
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
                    }
                } else {
                    tracing::warn!("event #{}: {err:#}", event.seq);
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
        self.state.retry.retain(|event| {
            let keep = now - event.at <= RETRY_EVENTS_FOR;
            if !keep {
                tracing::warn!("event #{}: still failing; dropped", event.seq);
            }
            keep
        });
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
fn clip_note(text: String) -> String {
    let max = crate::ops::limits().note_chars;
    if text.chars().count() <= max {
        return text;
    }
    let mut cut: String = text.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

async fn add_note(
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

/// Is this a failure that may pass on its own: no answer, or a server-side error?
fn is_transient(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<ServerError>() {
        Some(server) => !server.status.is_client_error(),
        None => true,
    }
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
    let t3 = T3Cli::locate(ctx.config.t3_bin.as_deref())
        .with_thread_model(ctx.config.thread_model.clone());
    tracing::info!(
        "watching as device {} with {}",
        ctx.device,
        t3.bin.display()
    );
    // Hooks of this home reach the server through the relay while the daemon runs; without
    // it they call the server directly, so a relay that cannot start costs only speed.
    let _relay = if once { None } else { start_relay(&ctx) };
    let usage_source =
        UsageSource::claude_code(t3.thread_model.as_ref().map(|m| m.provider.as_str()));
    let host_memory = device::host_memory();
    if host_memory.is_none() {
        tracing::warn!(
            "the memory of this host could not be read; without `runs_at_once` in config.toml the device runs {} agents at once",
            device::FALLBACK_RUNS
        );
    }
    let mut daemon =
        Daemon::new(ctx, t3).with_default_run_limit(device::default_run_limit(host_memory));
    daemon.usage_source = usage_source;
    if daemon.state.last_seq.is_none() {
        // Start from now: history is not news.
        let health = daemon.client().health().await?;
        daemon.state.last_seq = health["seq"].as_i64();
    }
    if once {
        daemon.reconcile(Utc::now()).await?;
        let mut reconcile = daemon.retry_failed(Utc::now()).await;
        reconcile |= daemon.catch_up(Utc::now()).await?;
        if reconcile {
            daemon.reconcile(Utc::now()).await?;
        }
        daemon.flush(Utc::now()).await;
        return daemon.save();
    }
    daemon.follow(&mut shutdown).await
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
}

impl<T: T3> Daemon<T> {
    /// Reconciles at once, then follows the event stream, flushing and reconciling on their
    /// ticks, also while the stream is down, until `shutdown` asks it to stop. Each step
    /// races the request to stop, so a step that waits on a server or `t3` never delays the
    /// exit; every step is safe to cut short.
    pub async fn follow(&mut self, shutdown: &mut Shutdown) -> Result<()> {
        let first = self.timing.reconnect_first;
        let mut delay = first;
        let mut reconcile_tick = tokio::time::interval(self.timing.reconcile_every);
        let mut flush_tick = tokio::time::interval(self.timing.flush_every);
        let mut reconcile_needed = false;
        if self
            .run_step(Step::Reconcile, &mut reconcile_needed, shutdown)
            .await?
        {
            return self.stop().await;
        }
        reconcile_tick.reset();
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
        match step {
            Step::Event(data) => match serde_json::from_str::<Event>(&data) {
                Ok(event) => *reconcile_needed |= self.handle(&event, Utc::now()).await,
                Err(err) => tracing::warn!("unreadable event: {err}"),
            },
            Step::Flush => {
                if std::mem::take(reconcile_needed) {
                    self.reconcile_logged().await;
                }
                self.flush(Utc::now()).await;
                self.save()?;
            }
            Step::Reconcile => {
                *reconcile_needed |= self.retry_failed(Utc::now()).await;
                self.reconcile_logged().await;
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
        if tokio::time::timeout(budget, self.flush(Utc::now()))
            .await
            .is_err()
        {
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
        match tokio::time::timeout(budget, self.reconcile(Utc::now())).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!("reconcile: {err:#}"),
            Err(_) => tracing::warn!(
                "reconcile: still running after {}s; stopped until the next tick",
                budget.as_secs()
            ),
        }
    }
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
