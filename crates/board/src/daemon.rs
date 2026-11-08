//! `board watch`: the device daemon.
//!
//! It follows the server's event stream and, on this device:
//! - starts a coordinator thread for each autopilot board that lists this device in
//!   `home_devices` and has no live coordinator (claim the lease as the device, start a T3
//!   thread with `prompts/coordinator.md`, plus `prompts/triage.md` on the device's feedback
//!   board, claim again as the thread's agent);
//! - renews the coordinator lease while the thread is live;
//! - wakes the coordinator when a task becomes ready or a blocker clears (the coordinator
//!   of the waiting task's board, which may differ from the blocker's), the owner of a
//!   blocked task when its last blocker is done, and the asker (worker or coordinator) when
//!   its ask is answered, or the coordinator when the asker cannot be woken;
//! - never wakes a paused agent before its `paused_until`;
//! - wakes an ended agent like an idle one while T3 still has its thread: T3 closes a
//!   session left idle, which marks its agent ended, and a message resumes the thread. One
//!   whose session closed while it waited for a usage-limit reset keeps that reset, and
//!   waits for it like a paused agent.
//!
//! Wake-ups are queued as what they are about (a task, an ask), not as text. Each flush
//! checks them again before sending: a task no longer ready and auto drops its line. Lines
//! for a running agent are held until its `agent_state_changed` event says it stopped, at
//! most [`WAKE_HOLD_MAX`]; a paused agent's are kept until its reset. A failed send backs
//! off, a gone thread drops its lines, and a send that timed out is not repeated.
//!
//! Wake-up messages contain only board-generated text and the user's own answers.
//!
//! A started thread is recorded before it is registered or named coordinator, so a failed
//! step is retried with the same thread; a new thread is started only once the recorded one
//! is gone. A board whose coordinator cannot be started here is retried with a growing delay
//! until its repos, home devices, autopilot switch or status change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use board_proto::{
    Agent, AgentId, AgentRole, AgentSource, AgentState, Board, BoardSlug, BoardStatus,
    ClaimCoordinator, DeviceName, ErrorCode, Event, EventType, Priority, RegisterAgent, Runtime,
    Task, TaskId, TaskStatus, Timestamp,
};
use chrono::{TimeDelta, Utc};
use eventsource_stream::Eventsource as _;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::client::{Client, ServerError, error_code, is_unreachable};
use crate::config::{Config, Home};
use crate::ops::Ctx;
use crate::state;
use crate::t3::{T3, T3Cli, TimedOut};

/// The coordinator's standing instructions.
pub const COORDINATOR_PROMPT: &str = include_str!("../../../prompts/coordinator.md");
/// The feedback board coordinator's triage instructions, after [`COORDINATOR_PROMPT`].
pub const TRIAGE_PROMPT: &str = include_str!("../../../prompts/triage.md");

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
/// How long one wake-up send may take; one that runs out is not sent again, since T3 may
/// have delivered it.
const WAKE_SEND_TIMEOUT: Duration = Duration::from_secs(20);
/// How many agents' wake-ups a flush checks at once.
const FLUSH_CHECKS_AT_ONCE: usize = 8;

/// Renew the 45-minute coordinator lease this often while the thread is live.
pub const RENEW_EVERY: chrono::TimeDelta = chrono::TimeDelta::minutes(10);
/// A named coordinator's thread that T3 found live is not asked about again for this long.
/// Each `t3 thread show` starts a process of about 180 MB, so asking for every coordinator
/// each minute costs a home device more than the check is worth. A failed send to the
/// thread, or its agent ending, asks again on the next reconcile.
pub const THREAD_RECHECK: TimeDelta = TimeDelta::minutes(5);
/// An ended coordinator's lease is released once checks of its thread have failed this
/// long, so another home device takes over while T3 is down, and one slow check does not
/// hand the board away.
pub const CHECK_FAIL_RELEASE: TimeDelta = TimeDelta::minutes(10);
const STATE_NAME: &str = "watch";
/// Delay before retrying a coordinator that failed to start, doubled per failure up to the cap.
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
    /// Coordinators are checked this often.
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
    /// Each board's `t3 thread show` gets this long. The boards' checks run side by side,
    /// so a slow T3 for one thread delays a reconcile by this much at most.
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
    /// Coordinator threads this device started, by board.
    #[serde(default)]
    pub coordinators: BTreeMap<BoardSlug, Coordinator>,
    /// Events whose handling failed on a transient error, retried on the reconcile tick.
    #[serde(default)]
    pub retry: Vec<Event>,
    /// Boards whose coordinator start began at this time and has not recorded its thread.
    /// Saved before `t3 thread start` runs, so a start the daemon did not live to finish is
    /// found again: the next start adopts the thread it created instead of adding another.
    #[serde(default)]
    pub pending_starts: BTreeMap<BoardSlug, Timestamp>,
    /// Wake-ups not sent yet, by agent: held while the agent runs or is paused, or kept
    /// after a failed send. Saved so a restart does not lose them; an entry this version
    /// cannot read is skipped rather than resetting the rest of the state.
    #[serde(default, deserialize_with = "lenient_queues")]
    pub wakes: BTreeMap<AgentId, WakeQueue>,
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
    /// failed send, only the send waits: the agent is still checked, so its answers reach
    /// the coordinator and expired lines go.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lookup: bool,
}

/// A queued wake-up: what it is about, checked again and rendered when it is sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wake {
    pub board: BoardSlug,
    pub about: WakeAbout,
    pub queued_at: Timestamp,
    /// An answer whose asker cannot be woken now, already passed on to the coordinator.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forwarded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeAbout {
    /// For the coordinator: the task became ready.
    Ready { task: TaskId },
    /// For the coordinator: `done` cleared the last blocker of `ready`.
    Cleared { done: TaskId, ready: Vec<TaskId> },
    /// For the coordinator: another agent filed `task` as a proposal to review.
    Proposal { task: TaskId },
    /// For the owner of the blocked `task`: `done` was its last blocker not done. The line
    /// names only the task, so two blockers done close together make one line.
    Unblocked { done: TaskId, task: TaskId },
    /// The user answered `asker`'s ask: for the asker, or for the coordinator when the
    /// asker cannot be woken.
    Answered { ask: i64, asker: AgentId },
}

/// Each agent's queue read on its own, so one this version cannot read (a newer kind, a
/// damaged entry) is skipped instead of failing the whole state file.
fn lenient_queues<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<AgentId, WakeQueue>, D::Error> {
    let serde_json::Value::Object(queues) = serde_json::Value::deserialize(d)? else {
        return Ok(BTreeMap::new());
    };
    Ok(queues
        .into_iter()
        .filter_map(
            |(agent, queue)| match (agent.parse::<AgentId>(), serde_json::from_value(queue)) {
                (Ok(id), Ok(queue @ WakeQueue { .. })) => {
                    (!queue.wakes.is_empty()).then_some((id, queue))
                }
                _ => {
                    tracing::warn!(
                        "watch state: skipped the wake-ups for {agent}, which this version cannot read"
                    );
                    None
                }
            },
        )
        .collect())
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
    /// T3 has the thread and has not archived it.
    Live,
    /// T3 no longer has the thread, or archived it.
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
    /// Ended, and T3 no longer has its thread or has archived it.
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
            Self::ThreadGone => "it ended and T3 no longer has its thread, or archived it",
            Self::NoReset => "it is paused with no reset time",
        })
    }
}

impl Reach {
    fn of(seen: &Seen, device: &DeviceName, now: Timestamp) -> Self {
        let agent = &seen.agent;
        if &agent.device != device {
            return Self::Never(Unreachable::Elsewhere);
        }
        if agent.id.source() != AgentSource::T3 {
            return Self::Never(Unreachable::NotT3);
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
}

/// How a wake-up send went.
enum Sent {
    Delivered,
    /// It ran out of time: T3 may have delivered it, so it is not sent again.
    MaybeDelivered(anyhow::Error),
    /// T3 no longer has the thread, or it is archived.
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

/// When a wake-up for an agent paused until `paused_until` is still worth sending: a
/// paused agent's wake-ups live from the reset on.
fn is_fresh(wake: &Wake, paused_until: Option<Timestamp>, now: Timestamp) -> bool {
    let from = paused_until.map_or(wake.queued_at, |until| until.max(wake.queued_at));
    now - from < WAKE_TTL
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coordinator {
    pub thread: String,
    pub agent: AgentId,
    pub started_at: Timestamp,
    pub renewed_at: Timestamp,
    /// The `paused_until` the coordinator was last resumed after.
    pub resumed_after: Option<Timestamp>,
    /// Whether the thread's agent is registered; false right after the thread started.
    #[serde(default = "registered_default")]
    pub registered: bool,
    /// [`prompt_digest`] of the standing prompt the thread was last briefed with. A record
    /// without it predates the field, so its prompt counts as changed and is sent once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub briefed: Option<String>,
    /// Set after a failed re-brief: it is not sent again before `retry_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebrief_backoff: Option<Backoff>,
}

impl Coordinator {
    /// A thread taken on as coordinator at `now`, with its lease just renewed.
    fn new(
        thread: String,
        agent: AgentId,
        now: Timestamp,
        registered: bool,
        briefed: Option<String>,
    ) -> Self {
        Self {
            thread,
            agent,
            started_at: now,
            renewed_at: now,
            resumed_after: None,
            registered,
            briefed,
            rebrief_backoff: None,
        }
    }
}

/// Records written before this field existed were registered before they were saved.
fn registered_default() -> bool {
    true
}

/// A request from `board board adopt`: the next time the board needs a coordinator, this
/// device's daemon makes `thread`, an existing thread, its coordinator instead of starting
/// one. Kept in its own state file, `adopt-<board>.json`, so the CLI never writes the
/// daemon's `watch.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Adoption {
    pub thread: String,
    pub requested_at: Timestamp,
}

/// An adoption not acted on for this long is dropped: by then a new thread is what the
/// user expects. It outlasts a coordinator lease, which an adoption may wait out.
pub const ADOPTION_TTL: TimeDelta = TimeDelta::hours(1);

fn adoption_name(slug: &BoardSlug) -> String {
    format!("adopt-{slug}")
}

/// The adoption waiting for `slug` on this device, if any.
pub fn load_adoption(home: &Home, slug: &BoardSlug) -> Option<Adoption> {
    state::load_named::<Option<Adoption>>(home, &adoption_name(slug))
}

pub fn save_adoption(home: &Home, slug: &BoardSlug, adoption: &Adoption) -> Result<()> {
    state::save_named(home, &adoption_name(slug), &Some(adoption))
}

pub fn remove_adoption(home: &Home, slug: &BoardSlug) -> Result<()> {
    state::remove_named(home, &adoption_name(slug))
}

/// Removes `adoption` unless a newer request replaced it in the meantime.
fn drop_adoption(home: &Home, slug: &BoardSlug, adoption: &Adoption) -> Result<()> {
    if load_adoption(home, slug).as_ref() == Some(adoption) {
        remove_adoption(home, slug)?;
    }
    Ok(())
}

/// The coordinators this device's daemon runs, as it last saved them.
pub fn load_watch_state(home: &Home) -> WatchState {
    state::load_named(home, STATE_NAME)
}

/// The agent id of a T3 thread.
pub fn thread_agent(thread: &str) -> Result<AgentId> {
    format!("t3:{thread}")
        .parse::<AgentId>()
        .with_context(|| format!("thread id {thread:?} makes no agent id"))
}

/// The board settings that decide whether this device can start its coordinator.
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

/// A failed coordinator start, retried at `retry_at` unless the board's settings change.
#[derive(Debug, Clone)]
struct StartFailure {
    config: HostingConfig,
    delay: TimeDelta,
    retry_at: Timestamp,
}

pub struct Daemon<T: T3> {
    ctx: Ctx,
    t3: T,
    pub state: WatchState,
    boards: BTreeMap<BoardSlug, Board>,
    /// Boards whose coordinator failed to start here, by board.
    failures: BTreeMap<BoardSlug, StartFailure>,
    /// Coordinator threads T3 last found live, and when: see [`THREAD_RECHECK`].
    threads_live: BTreeMap<String, Timestamp>,
    /// Boards whose coordinator thread check has failed since, and when it first failed.
    checks_failing: BTreeMap<BoardSlug, Timestamp>,
    /// Agents with queued wake-ups, as last read or reported.
    agents: BTreeMap<AgentId, Seen>,
    /// Agents whose wake-up message is being sent. A flush cut short while sending leaves
    /// them here: their lines count as maybe delivered, never sent twice.
    sending: Vec<AgentId>,
    /// The text of `config.toml` as last read, so a reconcile re-reads it only after an edit.
    config_text: Option<String>,
    pub timing: Timing,
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
            state,
            boards: BTreeMap::new(),
            failures: BTreeMap::new(),
            threads_live: BTreeMap::new(),
            checks_failing: BTreeMap::new(),
            agents: BTreeMap::new(),
            sending: Vec::new(),
            config_text,
            timing: Timing::default(),
        }
    }

    /// Applies an edited `config.toml`: its `[projects]`, `[board_projects]` and
    /// `feedback_board` take effect at once, and boards whose start failed are tried again,
    /// since a missing project may be what failed. `server`, `device` and `t3_bin` are
    /// fixed while the daemon runs, so changing them asks for a restart. A file gone or
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
        if (&new.server, &new.device, &new.t3_bin)
            != (&current.server, &current.device, &current.t3_bin)
        {
            tracing::warn!(
                "{} changes server, device or t3_bin: restart board watch to apply them",
                path.display()
            );
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

    pub fn save(&self) -> Result<()> {
        state::save_named(&self.ctx.home, STATE_NAME, &self.state)
    }

    fn client(&self) -> &Client {
        &self.ctx.client
    }

    fn is_home(&self, board: &Board) -> bool {
        board.autopilot
            && board.status == BoardStatus::Active
            && board.home_devices.contains(&self.ctx.device)
    }

    /// Starts, renews or resumes the coordinators this device hosts.
    pub async fn reconcile(&mut self, now: Timestamp) -> Result<()> {
        self.reload_config();
        let boards = self.client().boards().await?;
        self.boards = boards.iter().map(|b| (b.slug.clone(), b.clone())).collect();
        let mut checks = self.check_threads(&boards, now).await;
        for board in &boards {
            if !self.is_home(board) {
                self.failures.remove(&board.slug);
                self.state.pending_starts.remove(&board.slug);
                if self.state.coordinators.remove(&board.slug).is_some() {
                    self.release(&board.slug).await;
                }
                continue;
            }
            let check = checks.remove(&board.slug);
            if let Err(err) = self.reconcile_board(board, now, check).await {
                tracing::warn!("coordinator of {}: {err:#}", board.slug);
            }
        }
        let boards = &self.boards;
        self.state
            .coordinators
            .retain(|slug, _| boards.contains_key(slug));
        self.failures.retain(|slug, _| boards.contains_key(slug));
        self.checks_failing
            .retain(|slug, _| boards.contains_key(slug));
        self.state
            .pending_starts
            .retain(|slug, _| boards.contains_key(slug));
        self.save()
    }

    /// Whether each coordinator thread of ours that a home board still leaves to us is live,
    /// by board: asked side by side, each within [`Timing::thread_check`]. A thread the
    /// board names as coordinator and T3 found live less than [`THREAD_RECHECK`] ago counts
    /// as live without asking.
    async fn check_threads(
        &mut self,
        boards: &[Board],
        now: Timestamp,
    ) -> BTreeMap<BoardSlug, Result<bool>> {
        let mut checks = BTreeMap::new();
        let mut ask: Vec<(BoardSlug, String)> = Vec::new();
        for board in boards.iter().filter(|board| self.is_home(board)) {
            let Some(local) = self.state.coordinators.get(&board.slug) else {
                continue;
            };
            match &board.coordinator {
                Some(named) if *named != local.agent => continue,
                Some(_)
                    if local.registered
                        && self
                            .threads_live
                            .get(&local.thread)
                            .is_some_and(|at| now - *at < THREAD_RECHECK) =>
                {
                    checks.insert(board.slug.clone(), Ok(true));
                }
                _ => ask.push((board.slug.clone(), local.thread.clone())),
            }
        }
        let answers =
            futures_util::future::join_all(ask.iter().map(|(_, thread)| self.thread_live(thread)))
                .await;
        for ((slug, thread), answer) in ask.into_iter().zip(answers) {
            if matches!(answer, Ok(true)) {
                self.threads_live.insert(thread, now);
            } else {
                self.threads_live.remove(&thread);
            }
            checks.insert(slug, answer);
        }
        let ours: Vec<&str> = self
            .state
            .coordinators
            .values()
            .map(|c| c.thread.as_str())
            .collect();
        self.threads_live
            .retain(|thread, _| ours.contains(&thread.as_str()));
        checks
    }

    /// Whether T3 has `thread` and has not archived it.
    async fn thread_live(&self, thread: &str) -> Result<bool> {
        let limit = self.timing.thread_check;
        match tokio::time::timeout(limit, self.t3.thread_info(thread)).await {
            Ok(info) => Ok(info?.is_some_and(|t| !t.archived)),
            Err(_) => Err(TimedOut(format!("t3 thread show {thread}")).into()),
        }
    }

    /// `check` is what [`Self::check_threads`] found of the board's thread, if it asked.
    async fn reconcile_board(
        &mut self,
        board: &Board,
        now: Timestamp,
        check: Option<Result<bool>>,
    ) -> Result<()> {
        let slug = &board.slug;
        // Our own coordinator whose thread is gone: replace it whatever its agent record says.
        let mut ours_gone = false;
        if let Some(local) = self.state.coordinators.get(slug).cloned() {
            // No coordinator named: the lease lapsed (the device slept) or the start did not
            // finish. The thread may still be ours to run.
            if board.coordinator.as_ref().is_none_or(|c| *c == local.agent) {
                let named = board.coordinator.is_some() && local.registered;
                let live = match check {
                    Some(check) => check,
                    None => self.thread_live(&local.thread).await,
                };
                let mut info = None;
                let live = match live {
                    Ok(live) => {
                        self.checks_failing.remove(slug);
                        live
                    }
                    // A failed check says nothing of the thread. A named coordinator at work
                    // keeps its lease, and only a check that answers replaces it. One whose
                    // session ended is not renewed while the checks fail, and once they have
                    // failed for [`CHECK_FAIL_RELEASE`] its lease goes, so another home device
                    // can take over. The record stays: this device adopts the thread again
                    // once T3 answers, unless the board names another coordinator by then.
                    Err(err) if named => {
                        let since = *self.checks_failing.entry(slug.clone()).or_insert(now);
                        let agent = self.client().agent_info(&local.agent).await?;
                        if agent.state == AgentState::Ended {
                            if now - since >= CHECK_FAIL_RELEASE {
                                tracing::warn!(
                                    "coordinator of {slug}: checking thread {} has failed since {since}: {err:#}; its session ended, so releasing the lease",
                                    local.thread
                                );
                                self.release(slug).await;
                            } else {
                                tracing::warn!(
                                    "coordinator of {slug}: checking thread {}: {err:#}; its session ended, so not renewing the lease until T3 answers",
                                    local.thread
                                );
                            }
                            return Ok(());
                        }
                        tracing::warn!(
                            "coordinator of {slug}: checking thread {}: {err:#}; renewing it as live",
                            local.thread
                        );
                        info = Some(agent);
                        true
                    }
                    Err(err) => return Err(err),
                };
                if live {
                    return if named {
                        self.keep_alive(board, local, info, now).await
                    } else {
                        self.adopt(board, local, now).await
                    };
                }
                ours_gone = board.coordinator.is_some();
            }
            // The board names someone else, or the thread is gone.
            self.state.coordinators.remove(slug);
            self.checks_failing.remove(slug);
        }
        if ours_gone {
            // A renewal keeps the coordinator named, so the gone thread would stay named
            // until its replacement claims; releasing first means no one is named meanwhile.
            self.release(slug).await;
        } else if let Some(agent) = &board.coordinator {
            match self.client().agent_info(agent).await {
                Ok(info) if info.state != AgentState::Ended => return Ok(()),
                // T3 ends a session left idle while its thread lives on, and the server keeps
                // the lease: the daemon of its device renews or releases it. Taken over once
                // it expires.
                Ok(info) if info.device != self.ctx.device => return Ok(()),
                // Ours, and unknown to this daemon's state. A pending adoption replaces it;
                // otherwise its thread runs again if T3 has it.
                Ok(_)
                    if agent.source() == AgentSource::T3
                        && load_adoption(&self.ctx.home, slug).is_none() =>
                {
                    let thread = agent.native().to_owned();
                    if self.thread_live(&thread).await? {
                        self.threads_live.insert(thread.clone(), now);
                        // What it was briefed with is unknown: briefed again, once.
                        let local = Coordinator::new(thread, agent.clone(), now, true, None);
                        return self.adopt(board, local, now).await;
                    }
                    // A renewal keeps it named: released first, as for a gone thread above.
                    self.release(slug).await;
                }
                Ok(_) => self.release(slug).await,
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {}
                Err(err) => return Err(err),
            }
        }
        let adoption = match load_adoption(&self.ctx.home, slug) {
            Some(adoption) if now - adoption.requested_at >= ADOPTION_TTL => {
                tracing::warn!(
                    "coordinator of {slug}: dropped the adoption of thread {}, requested {} and never acted on",
                    adoption.thread,
                    adoption.requested_at
                );
                drop_adoption(&self.ctx.home, slug, &adoption)?;
                None
            }
            adoption => adoption,
        };
        // An adoption requested after the last failed start is new intent: tried at once.
        if let Some(failure) = self.failures.get(slug)
            && failure.config == HostingConfig::of(board)
            && now < failure.retry_at
            && adoption
                .as_ref()
                .is_none_or(|a| a.requested_at <= failure.retry_at - failure.delay)
        {
            return Ok(());
        }
        let started = match adoption {
            Some(adoption) => self.adopt_thread(board, adoption, now).await,
            None => self.start_coordinator(board, now).await,
        };
        match started {
            Ok(()) => {
                self.failures.remove(slug);
                Ok(())
            }
            Err(err) => {
                self.note_failure(board, now);
                Err(err)
            }
        }
    }

    /// Delays the next start on `board`: doubled after each failure, reset when its hosting
    /// settings change.
    fn note_failure(&mut self, board: &Board, now: Timestamp) {
        let config = HostingConfig::of(board);
        let last = self
            .failures
            .get(&board.slug)
            .filter(|last| last.config == config)
            .map(|last| last.delay);
        let delay = next_delay(last, START_RETRY_FIRST, START_RETRY_MAX);
        self.failures.insert(
            board.slug.clone(),
            StartFailure {
                config,
                delay,
                retry_at: now + delay,
            },
        );
    }

    /// Renews the lease of a live coordinator and resumes it after a usage limit.
    /// `info` is the coordinator's agent when the caller just read it.
    async fn keep_alive(
        &mut self,
        board: &Board,
        mut local: Coordinator,
        info: Option<Agent>,
        now: Timestamp,
    ) -> Result<()> {
        if now - local.renewed_at >= RENEW_EVERY {
            match self.claim(&board.slug, Some(&local.agent)).await {
                Ok(updated) => {
                    self.boards.insert(board.slug.clone(), updated);
                    local.renewed_at = now;
                }
                // Another device took the lease over: the coordinator is no longer ours to run.
                Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => {
                    self.state.coordinators.remove(&board.slug);
                    return Ok(());
                }
                // Retried on the next reconcile; resuming below does not depend on it.
                Err(err) => tracing::warn!("renewing the coordinator of {}: {err:#}", board.slug),
            }
        }
        let info = match info {
            Some(info) => info,
            None => self.client().agent_info(&local.agent).await?,
        };
        // An ended coordinator keeps the reset its session stopped on (see `Reach::of`).
        if matches!(info.state, AgentState::Paused | AgentState::Ended)
            && let Some(until) = info.paused_until
            && until <= now
            && local.resumed_after != Some(until)
        {
            self.t3
                .send(
                    &local.thread,
                    &format!(
                        "board {}: the usage limit has reset. Run board_snapshot and continue your loop.",
                        board.slug
                    ),
                )
                .await?;
            local.resumed_after = Some(until);
        }
        self.rebrief(&board.slug, &mut local, &info, now).await;
        self.state.coordinators.insert(board.slug.clone(), local);
        Ok(())
    }

    /// Sends a live coordinator the standing prompt again when it changed since the thread
    /// was briefed (`board` was upgraded, or `feedback_board` changed), once. Held while the
    /// coordinator is paused, or runs and was seen within [`WAKE_HOLD_MAX`], so it lands
    /// between turns. Like a wake-up: a send that timed out counts as sent, a failed one
    /// backs off, and either makes the next reconcile ask T3 whether the thread is gone.
    async fn rebrief(
        &mut self,
        slug: &BoardSlug,
        local: &mut Coordinator,
        info: &Agent,
        now: Timestamp,
    ) {
        let triage = self.triages(slug);
        let digest = prompt_digest(triage);
        let held = match info.state {
            AgentState::Paused => true,
            AgentState::Running => now - info.last_seen < WAKE_HOLD_MAX,
            _ => false,
        };
        if local.briefed.as_deref() == Some(digest)
            || held
            || local
                .rebrief_backoff
                .as_ref()
                .is_some_and(|b| now < b.retry_at)
        {
            return;
        }
        let sent = self
            .send_wake(&local.agent, &rebrief_message(slug, triage))
            .await;
        if !matches!(sent, Sent::Delivered) {
            self.threads_live.remove(local.agent.native());
        }
        match sent {
            Sent::Delivered => {
                tracing::info!(
                    "re-briefed {} of {slug}: its standing prompt changed",
                    local.agent
                );
                local.briefed = Some(digest.to_owned());
                local.rebrief_backoff = None;
            }
            Sent::MaybeDelivered(err) => {
                tracing::warn!(
                    "re-briefing {} of {slug}: {err:#}; not sent again, since T3 may have delivered it",
                    local.agent
                );
                local.briefed = Some(digest.to_owned());
                local.rebrief_backoff = None;
            }
            Sent::Gone(err) | Sent::Failed(err) => {
                let last = local
                    .rebrief_backoff
                    .as_ref()
                    .map(|b| TimeDelta::seconds(b.delay_secs));
                let delay = next_delay(last, WAKE_RETRY_FIRST, WAKE_RETRY_MAX);
                tracing::warn!(
                    "re-briefing {} of {slug}: {err:#}; trying again in {}s",
                    local.agent,
                    delay.num_seconds()
                );
                local.rebrief_backoff = Some(Backoff {
                    delay_secs: delay.num_seconds(),
                    retry_at: now + delay,
                    lookup: false,
                });
            }
        }
    }

    /// Names a live thread of ours the board's coordinator: registers its agent when that
    /// has not happened yet, then claims the lease with it. On a transient failure the record
    /// stays, so the next reconcile tries again with the same thread.
    async fn adopt(&mut self, board: &Board, mut local: Coordinator, now: Timestamp) -> Result<()> {
        let slug = &board.slug;
        if !local.registered {
            self.register_coordinator(board, &local).await?;
            local.registered = true;
            self.state.coordinators.insert(slug.clone(), local.clone());
        }
        match self.claim(slug, Some(&local.agent)).await {
            Ok(updated) => {
                self.boards.insert(slug.clone(), updated);
                local.renewed_at = now;
                tracing::info!("{} coordinates {slug}", local.agent);
                self.state.coordinators.insert(slug.clone(), local);
                Ok(())
            }
            Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => {
                tracing::warn!(
                    "another device took the coordinator lease of {slug}; thread {} is no longer its coordinator",
                    local.thread
                );
                self.state.coordinators.remove(slug);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    async fn register_coordinator(&self, board: &Board, local: &Coordinator) -> Result<()> {
        let slug = &board.slug;
        let (repo, project) = self.project_for(board).with_context(|| no_project(board))?;
        let runtime = match self.t3.thread_info(&local.thread).await {
            Ok(Some(info)) => info.runtime,
            _ => Runtime::Claude,
        };
        self.client()
            .with_agent(Some(local.agent.clone()))
            .register(&RegisterAgent {
                id: local.agent.clone(),
                runtime,
                device: self.ctx.device.clone(),
                role: AgentRole::Coordinator,
                title: Some(coordinator_title(slug)),
                repo,
                branch: None,
                cwd: Some(project.display().to_string()),
                native_ids: [("t3_thread".to_owned(), local.thread.clone())].into(),
                forked_from: None,
            })
            .await?;
        Ok(())
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

    /// The local T3 project folder for one of the board's repos, with that repo; else the
    /// folder `[board_projects]` names for the board, without a repo.
    fn project_for(&self, board: &Board) -> Option<(Option<String>, PathBuf)> {
        let config = &self.ctx.config;
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

    /// Whether `slug` is this device's feedback board, whose coordinator also triages.
    fn triages(&self, slug: &BoardSlug) -> bool {
        *slug == self.ctx.config.feedback_board()
    }

    async fn start_coordinator(&mut self, board: &Board, now: Timestamp) -> Result<()> {
        let slug = &board.slug;
        // Checked before claiming: a device that cannot host the thread leaves the lease alone.
        let Some((_, project)) = self.project_for(board) else {
            anyhow::bail!(no_project(board));
        };
        match self.claim(slug, None).await {
            Ok(updated) => {
                self.boards.insert(slug.clone(), updated);
            }
            // Another device holds the lease.
            Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => return Ok(()),
            Err(err) => return Err(err),
        }
        let started = self.start_or_find_thread(slug, &project, now).await;
        let (thread, briefed) = match started {
            Ok(started) => started,
            Err(err) => {
                self.release(slug).await;
                return Err(err);
            }
        };
        let agent = match thread_agent(&thread) {
            Ok(agent) => agent,
            Err(err) => {
                self.release(slug).await;
                return Err(err.context(format!("t3 started thread {thread:?}; archive it in T3")));
            }
        };
        tracing::info!("started coordinator thread {thread} for {slug}");
        // A thread an earlier start launched may hold another prompt: re-brief it.
        let briefed = briefed.then(|| prompt_digest(self.triages(slug)).to_owned());
        let local = Coordinator::new(thread, agent, now, false, briefed);
        // Recorded before anything else can fail, so a retry reuses this thread.
        self.state.coordinators.insert(slug.clone(), local.clone());
        self.state.pending_starts.remove(slug);
        self.save()?;
        self.adopt(board, local, now).await
    }

    /// The coordinator thread for `slug`: the one an unfinished earlier start created, if
    /// any, else a new one. An earlier start is unfinished when its pending mark is still
    /// saved (the daemon was killed, or `t3` failed or timed out, after the mark was written);
    /// a thread with the coordinator title created since then is its thread. One that never
    /// ran is briefed first. Threads older than the mark are never taken. The flag says
    /// whether this binary briefed the thread; one that ran was briefed by the earlier start,
    /// maybe with another prompt.
    async fn start_or_find_thread(
        &mut self,
        slug: &BoardSlug,
        project: &Path,
        now: Timestamp,
    ) -> Result<(String, bool)> {
        let title = coordinator_title(slug);
        let message = coordinator_message(slug, self.triages(slug));
        if let Some(&since) = self.state.pending_starts.get(slug) {
            let found = self
                .t3
                .threads_titled(project, &title)
                .await
                .context("looking for the thread of an unfinished coordinator start")?
                .into_iter()
                .filter(|t| t.created_at >= since)
                .max_by_key(|t| t.created_at);
            if let Some(thread) = found {
                if thread.ran {
                    tracing::warn!(
                        "coordinator of {slug}: adopting thread {}, which a start at {since} launched but did not record",
                        thread.id
                    );
                    return Ok((thread.id, false));
                }
                tracing::warn!(
                    "coordinator of {slug}: briefing thread {}, which a start at {since} created but did not send its message",
                    thread.id
                );
                if let Err(err) = self.t3.send(&thread.id, &message).await {
                    // Not that thread again: the next start makes a new one.
                    self.state.pending_starts.remove(slug);
                    return Err(err.context(format!(
                        "briefing thread {} left by an earlier start; archive it in T3",
                        thread.id
                    )));
                }
                return Ok((thread.id, true));
            }
        }
        // On disk before T3 can create the thread.
        self.state.pending_starts.entry(slug.clone()).or_insert(now);
        self.save()?;
        let thread = self.t3.start_thread(project, &title, &message).await?;
        Ok((thread, true))
    }

    /// Makes the existing thread named by `board board adopt` the board's coordinator: like
    /// [`Self::start_coordinator`], but it briefs that thread instead of starting one. From
    /// then on the thread is this device's coordinator like any other. A thread T3 no longer
    /// has drops the adoption, and a new thread is started. An agent the thread's own hooks
    /// registered keeps its record (where it works, a usage-limit pause); only one the board
    /// has never seen is registered here.
    async fn adopt_thread(
        &mut self,
        board: &Board,
        adoption: Adoption,
        now: Timestamp,
    ) -> Result<()> {
        let slug = &board.slug;
        if self.project_for(board).is_none() {
            anyhow::bail!(no_project(board));
        }
        let agent = match thread_agent(&adoption.thread) {
            Ok(agent) => agent,
            Err(err) => {
                drop_adoption(&self.ctx.home, slug, &adoption)?;
                return Err(err.context(format!("dropped the adoption for {slug}")));
            }
        };
        // Claimed before asking T3, which is slow: meanwhile no other device starts a thread.
        match self.claim(slug, None).await {
            Ok(updated) => {
                self.boards.insert(slug.clone(), updated);
            }
            // Another device holds the lease; the adoption waits until it lets go.
            Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => return Ok(()),
            Err(err) => return Err(err),
        }
        let checked = async {
            let live = self
                .t3
                .thread_info(&adoption.thread)
                .await?
                .is_some_and(|t| !t.archived);
            if !live {
                return Ok(None);
            }
            match self.client().agent_info(&agent).await {
                Ok(_) => Ok(Some(true)),
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => Ok(Some(false)),
                Err(err) => Err(err),
            }
        }
        .await;
        let registered = match checked {
            Ok(Some(registered)) => registered,
            Ok(None) => {
                tracing::warn!(
                    "coordinator of {slug}: thread {} to adopt is gone or archived; starting a new thread",
                    adoption.thread
                );
                drop_adoption(&self.ctx.home, slug, &adoption)?;
                return self.start_coordinator(board, now).await;
            }
            Err(err) => {
                self.release(slug).await;
                return Err(err);
            }
        };
        if let Err(err) = self
            .t3
            .send(
                &adoption.thread,
                &adoption_message(slug, self.triages(slug)),
            )
            .await
        {
            self.release(slug).await;
            return Err(err);
        }
        tracing::info!(
            "adopted thread {} as coordinator of {slug}",
            adoption.thread
        );
        let briefed = Some(prompt_digest(self.triages(slug)).to_owned());
        let local = Coordinator::new(adoption.thread.clone(), agent, now, registered, briefed);
        // Recorded before the adoption is dropped, so the brief is sent only once.
        self.state.coordinators.insert(slug.clone(), local.clone());
        self.state.pending_starts.remove(slug);
        self.save()?;
        drop_adoption(&self.ctx.home, slug, &adoption)?;
        self.adopt(board, local, now).await
    }

    /// Handles one event: queues its wake-ups and advances `last_seq`. Returns true when the
    /// boards should be reconciled. On an error nothing is queued and `last_seq` stays.
    pub async fn on_event(&mut self, event: &Event, now: Timestamp) -> Result<bool> {
        if event.event_type == EventType::AgentStateChanged {
            // An ended agent's thread may be gone: the next reconcile asks T3 about it.
            if event.payload["to"] == "ended"
                && let Ok(agent) = event.actor.parse::<AgentId>()
            {
                self.threads_live.remove(agent.native());
            }
            self.note_state(event, now);
        }
        let (reconcile, wakes) = self.examine(event).await?;
        for (board, agent, about) in wakes {
            let wake = Wake {
                board,
                about,
                queued_at: now,
                forwarded: false,
            };
            self.queue(agent, wake);
        }
        self.state.last_seq = Some(self.state.last_seq.unwrap_or(0).max(event.seq));
        Ok(reconcile)
    }

    /// Adds `wake` to `agent`'s queue unless the same one is already there.
    fn queue(&mut self, agent: AgentId, wake: Wake) {
        let queue = self.state.wakes.entry(agent).or_default();
        if !queue
            .wakes
            .iter()
            .any(|w| w.board == wake.board && w.about == wake.about)
        {
            queue.wakes.push(wake);
        }
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
        match event.event_type {
            EventType::BoardCreated => return Ok((true, wakes)),
            // This device's own lease claims and releases change nothing a reconcile would act on.
            EventType::BoardUpdated => return Ok((!self.is_own_claim(event), wakes)),
            EventType::TaskCreated | EventType::TaskReleased | EventType::TaskUpdated => {
                if let Some(task) = &event.task
                    && let Some(coordinator) = self.wakeable(self.coordinator_here(slug), event)
                {
                    let detail = self.client().task(task).await?;
                    let about = if detail.task.ready && detail.task.auto {
                        Some(WakeAbout::Ready { task: task.clone() })
                    } else if event.event_type == EventType::TaskCreated
                        && is_proposal(&detail.task)
                    {
                        Some(WakeAbout::Proposal { task: task.clone() })
                    } else {
                        None
                    };
                    if let Some(about) = about {
                        wakes.push((slug.clone(), coordinator.clone(), about));
                    }
                }
            }
            // The tasks waiting on it may be on other boards: each board's coordinator hears
            // of its own tasks that became ready, and the owner of a blocked task of its
            // last blocker.
            EventType::TaskDone => {
                if let Some(done) = &event.task {
                    let detail = self.client().task(done).await?;
                    let mut ready: BTreeMap<BoardSlug, Vec<TaskId>> = BTreeMap::new();
                    for blocked in &detail.blocks {
                        // One gone since (its board deleted) does not hold up the rest.
                        let t = match self.client().task(blocked).await {
                            Ok(d) => d.task,
                            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => continue,
                            Err(err) => return Err(err),
                        };
                        if t.ready && t.auto {
                            ready.entry(t.board.clone()).or_default().push(t.id);
                        } else if let Some(owner) = self.wakeable(t.owner.as_ref(), event)
                            && t.status == TaskStatus::Blocked
                            && self.blockers_done(&t).await?
                        {
                            let about = WakeAbout::Unblocked {
                                done: done.clone(),
                                task: t.id.clone(),
                            };
                            wakes.push((t.board.clone(), owner.clone(), about));
                        }
                    }
                    for (board, ready) in ready {
                        if let Some(coordinator) =
                            self.wakeable(self.coordinator_here(&board), event)
                        {
                            let about = WakeAbout::Cleared {
                                done: done.clone(),
                                ready,
                            };
                            wakes.push((board, coordinator.clone(), about));
                        }
                    }
                }
            }
            // The asker; the coordinator only when the asker is no T3 thread. An asker that
            // turns out not to be wakeable when the flush comes is handled there.
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
                let to = match self.wakeable(Some(&asker), event) {
                    Some(asker) => Some(asker.clone()),
                    None => self
                        .wakeable(self.coordinator_here(slug), event)
                        .filter(|c| **c != asker)
                        .cloned(),
                };
                if let Some(to) = to {
                    wakes.push((slug.clone(), to, WakeAbout::Answered { ask, asker }));
                }
            }
            _ => {}
        }
        Ok((false, wakes))
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

    /// The coordinator of an autopilot board, when there is one: the one the board names,
    /// else this device's own coordinator thread while its lease has lapsed (a reconcile
    /// names it again, or replaces it when its thread is gone), so no wake-up is lost meanwhile.
    fn coordinator_here(&self, slug: &BoardSlug) -> Option<&AgentId> {
        let board = self
            .boards
            .get(slug)
            .filter(|b| b.autopilot && b.status == BoardStatus::Active)?;
        board
            .coordinator
            .as_ref()
            .or_else(|| self.state.coordinators.get(slug).map(|c| &c.agent))
    }

    /// Sends the queued wake-ups, one message per agent, after checking each again. The
    /// checks read only, a few agents at a time, within [`Timing::flush_budget`]; then the
    /// queues change at once, without waiting on anything, so a flush cut short at any
    /// point loses no wake-up. The sends run side by side, each given
    /// [`WAKE_SEND_TIMEOUT`], so a hanging T3 holds the daemon up once per flush, not once
    /// per agent; a flush cut short while sending counts those lines as maybe delivered.
    pub async fn flush(&mut self, now: Timestamp) {
        let deadline = tokio::time::Instant::now() + self.timing.flush_budget;
        let queues: Vec<(AgentId, WakeQueue)> = self
            .state
            .wakes
            .iter()
            .map(|(agent, queue)| (agent.clone(), queue.clone()))
            .collect();
        let this = &*self;
        let checked: Vec<Checked> =
            futures_util::stream::iter(queues.iter().map(|(agent, queue)| async move {
                tokio::time::timeout_at(deadline, this.check(agent, queue, now))
                    .await
                    .unwrap_or(Checked::NotReached)
            }))
            .buffered(FLUSH_CHECKS_AT_ONCE)
            .collect()
            .await;
        let late = checked
            .iter()
            .filter(|c| matches!(c, Checked::NotReached))
            .count();
        if late > 0 {
            tracing::warn!(
                "flush: {late} agents' wake-ups not checked within {}s; checked on the next flush",
                self.timing.flush_budget.as_secs()
            );
        }
        let mut outgoing = Vec::new();
        for ((agent, queue), checked) in queues.into_iter().zip(checked) {
            if let Some(out) = self.apply(agent, queue, checked, now) {
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
            seen.thread = match self.thread_live(agent.native()).await {
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
        let due =
            backing_off.is_none() && matches!(Reach::of(&seen, &self.ctx.device, now), Reach::Now);
        let rechecks = if due {
            let paused_until = paused_until(&seen);
            let mut lines = Vec::new();
            for wake in queue
                .wakes
                .iter()
                .filter(|w| is_fresh(w, paused_until, now))
            {
                lines.push(self.recheck(agent, wake).await);
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
    /// cannot be woken for to the coordinator, and returns the message to send, if any.
    /// Wake-ups queued for the agent since the check (answers forwarded to it) wait for the
    /// next flush.
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
            // Unknown to the server: its answers go to the coordinator.
            Checked::Unknown => {
                let unheard = self.forward_answers(agent, &mut queue.wakes);
                if unheard > 0 {
                    tracing::warn!(
                        "waking {agent}: unknown to the server; {unheard} answers to its asks \
                         dropped, with no coordinator here to pass them to"
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
        let paused_until = paused_until(&seen);
        queue.wakes.retain(|w| {
            let fresh = is_fresh(w, paused_until, now);
            if !fresh {
                tracing::warn!("waking {agent}: not delivered in time; dropped");
            }
            fresh
        });
        match Reach::of(&seen, &self.ctx.device, now) {
            Reach::Now => {}
            Reach::Held => {
                self.requeue(agent.clone(), queue);
                return None;
            }
            // Kept for the reset; meanwhile the coordinator hears of its answers, once one
            // is named.
            Reach::Paused => {
                self.forward_answers(agent, &mut queue.wakes);
                self.requeue(agent.clone(), queue);
                return None;
            }
            // Another device's, or not to be woken by the daemon: its answers go to the
            // coordinator, and the rest is not ours to send.
            Reach::Never(why) => {
                let dropped = queue
                    .wakes
                    .iter()
                    .filter(|w| !matches!(&w.about, WakeAbout::Answered { asker, .. } if asker == agent))
                    .count();
                let unheard = self.forward_answers(agent, &mut queue.wakes);
                if !why.expected() {
                    if dropped > 0 {
                        tracing::warn!("waking {agent}: {why}; {dropped} wake-ups dropped");
                    }
                    if unheard > 0 {
                        tracing::warn!(
                            "waking {agent}: {why}; {unheard} answers to its asks dropped, \
                             with no other coordinator here to pass them to"
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
            message: format!("{}\nRun board_snapshot and continue.", lines.join("\n")),
            wakes: sent,
            backoff,
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

    /// Passes the answers to `from`'s own asks, which `from` cannot be woken for now, to the
    /// board's coordinator, once each: an answer no one hears of would stall its task.
    /// Returns how many have no coordinator to go to (none named, or `from` is it); a queue
    /// kept for later tries those again.
    fn forward_answers(&mut self, from: &AgentId, wakes: &mut [Wake]) -> usize {
        let mut unheard = 0;
        for wake in wakes {
            let own = matches!(&wake.about, WakeAbout::Answered { asker, .. } if asker == from);
            if !own || wake.forwarded {
                continue;
            }
            let coordinator = self
                .coordinator_here(&wake.board)
                .filter(|c| *c != from && c.source() == AgentSource::T3)
                .cloned();
            let Some(coordinator) = coordinator else {
                unheard += 1;
                continue;
            };
            wake.forwarded = true;
            let forward = Wake {
                forwarded: false,
                ..wake.clone()
            };
            self.queue(coordinator, forward);
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
            Ok(Ok(Some(info))) if info.archived => Sent::Gone(err),
            _ => Sent::Failed(err),
        }
    }

    /// Records how sending `out` went.
    fn settle(&mut self, out: Outgoing, sent: Sent, now: Timestamp) {
        let agent = out.agent;
        if !matches!(sent, Sent::Delivered) {
            // A send that failed may mean the thread is gone: the next reconcile asks T3.
            self.threads_live.remove(agent.native());
        }
        match sent {
            Sent::Delivered => {
                if let Some(queue) = self.state.wakes.get_mut(&agent) {
                    queue.backoff = None;
                }
            }
            Sent::MaybeDelivered(err) => tracing::warn!(
                "waking {agent}: {err:#}; not sent again, since T3 may have delivered it"
            ),
            Sent::Gone(err) => {
                tracing::warn!(
                    "waking {agent}: {err:#}; its thread is gone, so its wake-ups are dropped"
                );
                let mut wakes = out.wakes;
                if let Some(queue) = self.state.wakes.remove(&agent) {
                    wakes.extend(queue.wakes);
                }
                let unheard = self.forward_answers(&agent, &mut wakes);
                if unheard > 0 {
                    tracing::warn!(
                        "waking {agent}: {unheard} answers to its asks dropped, with no other \
                         coordinator here to pass them to"
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

    /// The line for `wake` when what it is about still holds, else `None`.
    async fn recheck(&self, agent: &AgentId, wake: &Wake) -> Result<Option<String>> {
        let slug = &wake.board;
        let coordinator = self.coordinator_here(slug) == Some(agent);
        let line = match &wake.about {
            WakeAbout::Ready { task } if coordinator => {
                let task = self.client().task(task).await?.task;
                (task.ready && task.auto).then(|| {
                    format!(
                        "board {slug}: {} is ready ({})",
                        task.id,
                        priority(&task.priority)
                    )
                })
            }
            WakeAbout::Cleared { done, ready } if coordinator => {
                let mut still: Vec<TaskId> = Vec::new();
                for id in ready {
                    let t = self.client().task(id).await?.task;
                    if t.ready && t.auto {
                        still.push(t.id);
                    }
                }
                let ids: Vec<&str> = still.iter().map(TaskId::as_str).collect();
                (!ids.is_empty()).then(|| {
                    format!(
                        "board {slug}: {done} is done; its blocker cleared for {}",
                        ids.join(", ")
                    )
                })
            }
            WakeAbout::Proposal { task } if coordinator => {
                let task = self.client().task(task).await?.task;
                is_proposal(&task)
                    .then(|| format!("board {slug}: {} is a proposal to review", task.id))
            }
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
            WakeAbout::Answered { ask, asker } if asker == agent || coordinator => {
                let ask = self.client().ask(*ask).await?;
                let by = if asker == agent {
                    String::new()
                } else {
                    format!(" by {asker}")
                };
                let about = ask
                    .task
                    .as_ref()
                    .map(|t| format!(" about {t}"))
                    .unwrap_or_default();
                let answer = serde_json::to_string(ask.answer.as_deref().unwrap_or_default())?
                    .replace('<', "\\u003c")
                    .replace('>', "\\u003e");
                Some(format!(
                    "the user answered ask #{}{by}{about}: {answer}",
                    ask.id
                ))
            }
            // The agent is no longer this board's coordinator.
            WakeAbout::Ready { .. }
            | WakeAbout::Cleared { .. }
            | WakeAbout::Proposal { .. }
            | WakeAbout::Answered { .. } => None,
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

/// Is this a failure that may pass on its own: no answer, or a server-side error?
fn is_transient(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<ServerError>() {
        Some(server) => !server.status.is_client_error(),
        None => true,
    }
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

/// A pending proposal (prompts/coordinator.md "Proposals"). Its filing wakes the
/// coordinator to review it.
fn is_proposal(task: &board_proto::Task) -> bool {
    task.is_pending_proposal()
}

fn priority(p: &Priority) -> String {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The title of a board's coordinator thread.
pub fn coordinator_title(slug: &BoardSlug) -> String {
    format!("{slug} coordinator")
}

/// The coordinator's standing instructions: [`COORDINATOR_PROMPT`], followed by
/// [`TRIAGE_PROMPT`] when the board is the feedback board.
fn standing_prompt(triage: bool) -> String {
    if triage {
        format!("{COORDINATOR_PROMPT}\n{TRIAGE_PROMPT}")
    } else {
        COORDINATOR_PROMPT.to_owned()
    }
}

/// A digest of the standing prompt for `triage`, as hex: a coordinator briefed with another
/// one gets [`rebrief_message`]. Both prompts are compiled in, so each is hashed once.
pub fn prompt_digest(triage: bool) -> &'static str {
    use sha2::{Digest as _, Sha256};
    static DIGESTS: std::sync::LazyLock<[String; 2]> = std::sync::LazyLock::new(|| {
        [false, true].map(|triage| crate::config::hex(&Sha256::digest(standing_prompt(triage))))
    });
    &DIGESTS[usize::from(triage)]
}

/// The message that gives a running coordinator its changed standing prompt in full.
pub fn rebrief_message(slug: &BoardSlug, triage: bool) -> String {
    let prompt = standing_prompt(triage);
    format!(
        "board {slug}: your standing instructions changed (board was upgraded, or this device's feedback_board changed). The text below replaces the instructions you were briefed with; keep your task in progress and what you know about the work.\n\n{prompt}\nYou are the coordinator of board {slug}. Continue your loop: call board_snapshot."
    )
}

/// The first message of a coordinator thread; `triage` for the device's feedback board.
pub fn coordinator_message(slug: &BoardSlug, triage: bool) -> String {
    let prompt = standing_prompt(triage);
    format!("{prompt}\nYou are the coordinator of board {slug}. Start now: call board_snapshot.")
}

/// The message that briefs an existing thread adopted as a board's coordinator; `triage`
/// for the device's feedback board.
pub fn adoption_message(slug: &BoardSlug, triage: bool) -> String {
    let prompt = standing_prompt(triage);
    format!(
        "{prompt}\nThis thread is now also the coordinator of board {slug}: `board board adopt` named it, so the board's ready work comes to you from here on, and the board wakes you when work becomes ready, one of your asks is answered, or an ask is answered whose asker it cannot wake. Keep what you already know about this work. A board task you already own counts as the board's task in progress: finish it first. Start now: call board_snapshot."
    )
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
    let mut daemon = Daemon::new(ctx, t3);
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
                    if let Some(warning) = crate::client::take_skew_warning() {
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
    /// lines it was sending count as maybe delivered and are not sent again.
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
    /// they are not sent again.
    fn note_cut_sends(&mut self) {
        for agent in std::mem::take(&mut self.sending) {
            tracing::warn!(
                "waking {agent}: stopped while sending; not sent again, since T3 may have delivered it"
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
