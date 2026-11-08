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
//!   blocked task when its last blocker is done, the coordinator when an agent outside its
//!   own delegation moves a task to `review`, the owner of a task in `doing` when the
//!   server announces it has been away from it (`idle_doing`), or the coordinator when no
//!   device can wake that owner, and the asker (worker or coordinator) when its ask is
//!   answered, or the coordinator when the asker cannot be woken;
//! - adds to a coordinator's wake-up that a fresh session is due, once its thread has run
//!   [`FRESH_SESSION_RUNS`] times since the last notice and no user message is recent
//!   (see [`FreshSession`]);
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
//! off, and a gone thread drops its lines. A send that timed out may have been delivered:
//! a flush looks for its message in the thread a minute later, by the marker it ends with,
//! and queues its lines again, once, only when the thread never got it.
//!
//! Wake-up messages contain only board-generated text and the user's own answers.
//!
//! A started thread is recorded before it is registered or named coordinator, so a failed
//! step is retried with the same thread; a new thread is started only once the recorded one
//! is gone. A board whose coordinator cannot be started here is retried with a growing delay
//! until its repos, home devices, autopilot switch or status change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use board_proto::{
    Agent, AgentId, AgentRole, AgentSource, AgentState, Board, BoardSlug, BoardStatus,
    ClaimCoordinator, DeviceName, ErrorCode, Event, EventType, Priority, RegisterAgent, Runtime,
    Task, TaskId, TaskStatus, Timestamp, Usage,
};
use chrono::{TimeDelta, Utc};
use eventsource_stream::Eventsource as _;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::client::{Client, ServerError, error_code, is_unreachable};
use crate::config::{Config, Home};
use crate::ops::Ctx;
use crate::state;
use crate::t3::{T3, T3Cli, TimedOut, folder_gone};
use crate::usage::UsageSource;

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
/// How long one wake-up send may take; one that runs out may have been delivered, so it is
/// sent again only when a later check finds the thread lacks it (see [`UnsureWakes`]).
const WAKE_SEND_TIMEOUT: Duration = Duration::from_secs(20);
/// How many agents' wake-ups a flush checks at once.
const FLUSH_CHECKS_AT_ONCE: usize = 8;
/// How many delegations up [`Daemon::delegated_by`] follows, as deep as the server's
/// delegation family goes, so a cycle of `t3_parent` ids ends.
const DELEGATION_DEPTH: usize = 8;

/// A coordinator's next wake-up says a fresh session is due once its thread has run this
/// many more times than when the last one said so (T3's `runCount`). A tuned value, with a
/// revisit on the board.
pub const FRESH_SESSION_RUNS: u64 = 10;
/// No wake-up says a fresh session is due while the coordinator's thread has a user message
/// the daemon first saw less than this long ago: the user may be talking to it.
pub const USER_QUIET: TimeDelta = TimeDelta::minutes(60);

/// Renew the 45-minute coordinator lease this often while the thread is live.
pub const RENEW_EVERY: chrono::TimeDelta = chrono::TimeDelta::minutes(10);
/// The daemon reads the usage endpoint at most this often; between its own readings, each
/// reconcile takes the newest reading any device reported.
pub const USAGE_POLL_EVERY: TimeDelta = TimeDelta::minutes(5);
/// A named coordinator's thread that T3 found live is not asked about again for this long.
/// Each `t3 thread show` starts a process of about 180 MB, so asking for every coordinator
/// each minute costs a home device more than the check is worth. A failed send to the
/// thread, or its agent ending, asks again on the next reconcile. Nor is one whose check
/// timed out: it counts as failing meanwhile, until its agent ends or a send finds it gone.
pub const THREAD_RECHECK: TimeDelta = TimeDelta::minutes(5);
/// How many coordinator thread checks run at once. Each `t3` process needs about half a
/// second of CPU to start; nine at once on a busy device took 4-5 seconds each, and longer
/// than [`Timing::thread_check`] under a build, so every check timed out together.
pub const THREAD_CHECKS_AT_ONCE: usize = 2;
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
    /// Each board's `t3 thread show` gets this long. The boards' checks run
    /// [`THREAD_CHECKS_AT_ONCE`] at a time, and none starts once half of `reconcile_budget`
    /// has passed since the reconcile began, so the rest of the reconcile keeps its time.
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
    /// The wake-up messages to each agent whose send timed out, until a flush finds them
    /// in the thread or owes what they carried again (see [`UnsureWakes`]). Read leniently
    /// like `wakes`.
    #[serde(
        default,
        deserialize_with = "lenient_unsure",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub unsure_wakes: BTreeMap<AgentId, UnsureWakes>,
    /// Boards the usage held from new work at the last reading: when one drops out, its
    /// coordinator is woken to start work again.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub usage_held: BTreeSet<BoardSlug>,
}

/// The wake-up messages to one agent whose send timed out: T3 may or may not have
/// delivered them. A flush looks for each one's [`wake_marker`] in the thread
/// [`SEND_CHECK_AFTER`] after its send, and holds the agent's next lines until then, so
/// they go out with what is owed again. Found, a message is settled; missing while no run
/// of the thread may still take it, what it carried is owed again, once: its wake-ups are
/// queued again (see [`Wake::resent`]), its re-briefs are owed, and its fresh-session
/// notices are due.
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
    /// Its re-briefs not sent twice already.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rebriefs: Vec<RebriefMark>,
    /// Its fresh-session notices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fresh: Vec<FreshMark>,
    /// When the send that may have delivered it ran.
    pub since: Timestamp,
}

impl UnsureMessage {
    fn is_empty(&self) -> bool {
        self.wakes.is_empty() && self.rebriefs.is_empty() && self.fresh.is_empty()
    }
}

/// A re-brief a wake-up message carried: the coordinator's board, and the
/// [`prompt_digest`] of the standing prompt it carried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RebriefMark {
    pub slug: BoardSlug,
    pub digest: String,
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
    /// Queued again after a send that timed out never reached the thread: one more send
    /// that times out counts as delivered, so a wake-up is sent at most twice.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resent: bool,
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
    /// For the coordinator: an agent outside its own delegation moved `task` to `review`.
    Review { task: TaskId },
    /// The server noticed at `noticed` that `away`, the owner of `task` in `doing`, has been
    /// away from it since `since`: for `away`, or for the coordinator when `away` cannot be
    /// woken from any device.
    Idle {
        task: TaskId,
        away: AgentId,
        since: Timestamp,
        noticed: Timestamp,
    },
    /// For the coordinator: the board was paused and is active again (`board resume`).
    Resumed,
    /// For the coordinator: the usage no longer holds the board from new work.
    UsageEased,
}

impl WakeAbout {
    /// Its kind, as `watch.log` names it.
    fn kind(&self) -> &'static str {
        match self {
            Self::Ready { .. } => "ready",
            Self::Cleared { .. } => "cleared",
            Self::Proposal { .. } => "proposal",
            Self::Unblocked { .. } => "unblocked",
            Self::Answered { .. } => "answered",
            Self::Review { .. } => "review",
            Self::Idle { .. } => "idle",
            Self::Resumed => "resumed",
            Self::UsageEased => "usage_eased",
        }
    }
}

/// Each agent's queue read on its own, so one this version cannot read (a newer kind, a
/// damaged entry) is skipped instead of failing the whole state file.
fn lenient_queues<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<AgentId, WakeQueue>, D::Error> {
    let mut queues: BTreeMap<AgentId, WakeQueue> =
        lenient_by_agent(serde_json::Value::deserialize(d)?, "the wake-ups");
    queues.retain(|_, queue| !queue.wakes.is_empty());
    Ok(queues)
}

/// Each agent's timed-out wake-up message read on its own, like [`lenient_queues`].
fn lenient_unsure<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<AgentId, UnsureWakes>, D::Error> {
    let mut unsure: BTreeMap<AgentId, UnsureWakes> = lenient_by_agent(
        serde_json::Value::deserialize(d)?,
        "the timed-out wake-up messages",
    );
    unsure.retain(|_, u| !u.messages.is_empty());
    Ok(unsure)
}

/// The entries of `value`, an object by agent id, that this version can read; `what` names
/// an entry in the warning for one skipped.
fn lenient_by_agent<V: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    what: &str,
) -> BTreeMap<AgentId, V> {
    let serde_json::Value::Object(entries) = value else {
        return BTreeMap::new();
    };
    entries
        .into_iter()
        .filter_map(|(agent, entry)| {
            match (agent.parse::<AgentId>(), serde_json::from_value(entry)) {
                (Ok(id), Ok(entry)) => Some((id, entry)),
                _ => {
                    tracing::warn!(
                        "watch state: skipped {what} for {agent}, which this version cannot read"
                    );
                    None
                }
            }
        })
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
    /// For an ended agent: what T3 said of its thread, asked once per state change. Also
    /// gone for a coordinator whose thread a reconcile found gone, until its state changes.
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
    fn of(seen: &Seen, device: &DeviceName, now: Timestamp) -> Self {
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
    /// The re-briefs held while the coordinator's boards were idle, sent in front of the
    /// wake-up lines.
    rebriefs: Vec<HeldRebrief>,
    /// The boards whose coordinator this message tells that a fresh session is due.
    fresh: Vec<FreshMark>,
    /// The [`wake_digest`] its marker names, when its send was recorded in
    /// [`WatchState::unsure_wakes`] before it ran: it carries something not sent twice yet.
    unsure: Option<String>,
}

/// A fresh-session line that goes out with a wake-up: the coordinator's board, and its
/// [`FreshSession::since`] before, restored if the send fails or never reached the thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FreshMark {
    pub slug: BoardSlug,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_before: Option<u64>,
}

/// A re-brief that goes out with a wake-up: the coordinator's board, whether the prompt
/// carries triage, and the digest it was briefed with before, restored if the send fails.
struct HeldRebrief {
    slug: BoardSlug,
    triage: bool,
    before: Option<String>,
    /// The coordinator's [`Coordinator::rebrief_unsure`] before, restored with `before`.
    unsure_before: Option<UnsureRebrief>,
    /// Whether this prompt was found missing and is owed again already: a send of it that
    /// times out counts as delivered.
    resent: bool,
}

/// What a check found of a message whose send timed out: a re-brief, or a wake-up message.
enum Arrival {
    /// The thread has it.
    Found,
    /// Not in the thread, but a run of it is active or queued, which may still take it.
    Waiting,
    /// The thread never got it.
    Missing,
    /// The check failed; `gone` when T3 answered that it does not know the thread.
    Unknown { err: anyhow::Error, gone: bool },
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
    /// It ran out of time: T3 may have delivered it, so its wake-up lines and a re-brief it
    /// carried are sent again only when a later check finds the thread lacks it.
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

/// When a wake-up for an agent paused until `paused_until` is still worth sending: a
/// paused agent's wake-ups live from the reset on.
fn is_fresh(wake: &Wake, paused_until: Option<Timestamp>, now: Timestamp) -> bool {
    let from = paused_until.map_or(wake.queued_at, |until| until.max(wake.queued_at));
    now - from < WAKE_TTL
}

/// Whether a wake-up about `about`, for `from`, goes to the coordinator when `from` cannot
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

/// Whether `board` gives its coordinator nothing to do: no task in progress or in review
/// (T3 wakes the coordinator for those, not the daemon), no ready task it may start, and
/// no open ask. A server that predates `ready_auto` leaves the ready work unknown, so its
/// boards are never idle.
fn is_idle(board: &Board) -> bool {
    let counts = &board.counts;
    counts.doing == 0 && counts.review == 0 && counts.ready_auto == Some(0) && board.open_asks == 0
}

/// Records how sending `local` of `slug` the standing prompt with `digest` on its own went:
/// a send that timed out counts as sent until a reconcile finds it missing from the thread
/// (see [`Daemon::rebrief`]), and is sent again once at most; a failed one backs off.
fn settle_rebrief(
    local: &mut Coordinator,
    slug: &BoardSlug,
    digest: &str,
    sent: &Sent,
    now: Timestamp,
) {
    match sent {
        Sent::Delivered => {
            tracing::info!(
                "re-briefed {} of {slug}: its standing prompt changed",
                local.agent
            );
            local.briefed = Some(digest.to_owned());
            local.rebrief_backoff = None;
            local.rebrief_unsure = None;
        }
        Sent::MaybeDelivered(err) => {
            local.briefed = Some(digest.to_owned());
            local.rebrief_backoff = None;
            local.rebrief_unsure = unsure_after(local, digest, now);
            if local.rebrief_unsure.is_some() {
                tracing::warn!(
                    "re-briefing {} of {slug}: {err:#}; T3 may have delivered it, so a \
                     reconcile looks for it in the thread",
                    local.agent
                );
            } else {
                tracing::warn!(
                    "re-briefing {} of {slug}: {err:#}; sent twice already, so not sent again",
                    local.agent
                );
            }
        }
        Sent::Gone(err) | Sent::Failed(err) => {
            rebrief_back_off(local, slug, now, "re-briefing", err);
        }
    }
}

/// What `local` records once a re-brief with `digest` may have been delivered at `now`:
/// unsure, to be looked for in the thread, unless it was found missing and owed again
/// already, which leaves nothing to look for: a digest is sent at most twice.
fn unsure_after(local: &Coordinator, digest: &str, now: Timestamp) -> Option<UnsureRebrief> {
    let resent = local
        .rebrief_unsure
        .as_ref()
        .is_some_and(|u| u.digest == digest && u.resent);
    (!resent).then(|| UnsureRebrief {
        digest: digest.to_owned(),
        resent: false,
        since: now,
    })
}

/// Holds `local`'s next re-brief of `slug`, or check of one, for a back-off doubled per
/// failure, and logs `doing` and the `err` that failed it.
fn rebrief_back_off(
    local: &mut Coordinator,
    slug: &BoardSlug,
    now: Timestamp,
    doing: &str,
    err: &anyhow::Error,
) {
    let last = local
        .rebrief_backoff
        .as_ref()
        .map(|b| TimeDelta::seconds(b.delay_secs));
    let delay = next_delay(last, WAKE_RETRY_FIRST, WAKE_RETRY_MAX);
    tracing::warn!(
        "{doing} {} of {slug}: {err:#}; trying again in {}s",
        local.agent,
        delay.num_seconds()
    );
    local.rebrief_backoff = Some(Backoff {
        delay_secs: delay.num_seconds(),
        retry_at: now + delay,
        lookup: false,
    });
}

/// A re-brief whose send timed out: T3 may or may not have delivered it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnsureRebrief {
    /// The [`prompt_digest`] it carried, which its [`rebrief_marker`] names.
    pub digest: String,
    /// Whether a reconcile found it missing and it is owed again: a second send that times
    /// out counts as delivered.
    #[serde(default)]
    pub resent: bool,
    /// When the send that may have delivered it ran: it is looked for
    /// [`SEND_CHECK_AFTER`] later, once T3 has handled a request still in flight.
    pub since: Timestamp,
}

/// A re-brief or wake-up message whose send timed out is looked for in the thread this long
/// after the send, since T3 may still be handling the request the timed-out
/// `t3 thread send` made.
pub const SEND_CHECK_AFTER: TimeDelta = TimeDelta::minutes(1);

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
    /// Set after a re-brief send timed out, until a reconcile finds it in the thread or
    /// sends it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebrief_unsure: Option<UnsureRebrief>,
    /// What tells the coordinator when a fresh session is due.
    #[serde(default, skip_serializing_if = "FreshSession::is_empty")]
    pub fresh: FreshSession,
}

/// When a coordinator's next wake-up says a fresh session is due: a new Claude session in
/// the same thread, which the coordinator starts itself at a safe point (see
/// `prompts/coordinator.md`, Fresh sessions). A candidate once the thread has run
/// [`FRESH_SESSION_RUNS`] times since [`Self::since`] and no user message seen there is
/// younger than [`USER_QUIET`]; the flush that would carry the notice reads the thread's
/// messages first, and a user message new to the daemon holds it. T3 gives messages no
/// time, so a user message counts from when the daemon first saw it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FreshSession {
    /// The thread's `runCount` when a wake-up last said a fresh session was due, or when
    /// the daemon first read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    /// Its `runCount` as last read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs: Option<u64>,
    /// The newest user message seen in the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message: Option<SeenMessage>,
}

/// A user message, by [`message_key`], and when the daemon first saw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeenMessage {
    pub key: String,
    pub seen_at: Timestamp,
}

impl FreshSession {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Records the thread's `runCount`, as T3 just gave it. The first one starts the count,
    /// and so does one lower than where it started.
    pub fn observe(&mut self, runs: u64) {
        self.runs = Some(runs);
        if self.since.is_none_or(|since| runs < since) {
            self.since = Some(runs);
        }
    }

    /// Whether the next wake-up may say a fresh session is due, once a read of the thread's
    /// messages finds no new user message: it has run [`FRESH_SESSION_RUNS`] times since
    /// [`Self::since`], and no user message seen holds it.
    pub fn due(&self, now: Timestamp) -> bool {
        let reached = matches!(
            (self.since, self.runs),
            (Some(since), Some(runs)) if runs.saturating_sub(since) >= FRESH_SESSION_RUNS
        );
        let held = self
            .user_message
            .as_ref()
            .is_some_and(|m| now - m.seen_at < USER_QUIET);
        reached && !held
    }

    /// Records a read of the thread's messages at `now`: `newest_user` is the
    /// [`message_key`] of the newest user message among them ([`is_user_message`]), if any.
    /// One the daemon has not seen before holds the notice for [`USER_QUIET`].
    pub fn note_read(&mut self, newest_user: Option<String>, now: Timestamp) {
        if let Some(key) = newest_user
            && self.user_message.as_ref().is_none_or(|m| m.key != key)
        {
            self.user_message = Some(SeenMessage { key, seen_at: now });
        }
    }
}

/// Names a user message without keeping its text: a digest of the run it started and its
/// text, so a message steered into a run already named by another still counts as new.
pub fn message_key(message: &crate::t3::UserMessage) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    hash.update(message.run.as_deref().unwrap_or_default());
    hash.update([0]);
    hash.update(&message.text);
    crate::config::hex(&hash.finalize())[..16].to_owned()
}

/// Whether `text`, a user message of a coordinator thread, is from the user: not a board
/// message (`board <slug>: …`), the daemon's own brief (it starts with the coordinator
/// prompt's title), a delegated child's report (`Delegated task …`), a pull request update
/// (`Update on pull request …`), or a turn T3 starts itself (`/compact` on an idle
/// thread, `Background task completed.`). Other agents' `t3_thread_send` messages look
/// like the user's: T3's CLI names no sender.
pub fn is_user_message(text: &str) -> bool {
    let from_board = text
        .strip_prefix("board ")
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(slug, _)| slug.parse::<BoardSlug>().is_ok());
    let brief_title = COORDINATOR_PROMPT.lines().next().unwrap_or_default();
    !from_board
        && !text.starts_with(brief_title)
        && !text.starts_with("Delegated task")
        && !text.starts_with("Update on pull request")
        && !text.starts_with("Background task completed")
        && text.trim() != "/compact"
}

/// The line that tells the coordinator of `slug` that a fresh session is due.
pub fn fresh_session_line(slug: &BoardSlug) -> String {
    format!(
        "board {slug}: a fresh session is due. When your loop is done, follow \"Fresh sessions\" in your instructions."
    )
}

/// The message a coordinator sends its own thread, with `freshContext`, to start a fresh
/// session.
pub fn fresh_session_message(slug: &BoardSlug) -> String {
    format!(
        "board {slug}: fresh session. Run `board brief {slug}` and follow what it prints, then call board_snapshot."
    )
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
            rebrief_unsure: None,
            fresh: FreshSession::default(),
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

/// What T3 said of a thread: `None` when it cannot run it.
type RunsIn = Option<Runnable>;

/// A thread T3 can run.
struct Runnable {
    /// The folder T3 runs it in, when known.
    folder: Option<PathBuf>,
    /// Its `runCount`, when T3 gave it.
    runs: Option<u64>,
}

/// How a coordinator thread's last check went, when it spares the next ones for
/// [`THREAD_RECHECK`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum LastCheck {
    /// Live, run in this folder when known: a folder gone since ends the spare.
    Live(Option<PathBuf>),
    TimedOut,
}

pub struct Daemon<T: T3> {
    ctx: Ctx,
    t3: T,
    pub state: WatchState,
    boards: BTreeMap<BoardSlug, Board>,
    /// Boards whose coordinator failed to start here, by board.
    failures: BTreeMap<BoardSlug, StartFailure>,
    /// Coordinator threads whose last check T3 answered live or that timed out, and when:
    /// see [`THREAD_RECHECK`].
    last_checks: BTreeMap<String, (LastCheck, Timestamp)>,
    /// Boards whose coordinator thread check has failed since, and when it first failed.
    checks_failing: BTreeMap<BoardSlug, Timestamp>,
    /// Agents with queued wake-ups, as last read or reported.
    agents: BTreeMap<AgentId, Seen>,
    /// Coordinators with queued wake-ups whose thread a reconcile found gone, until their
    /// state changes: not woken, as for an ended agent whose thread T3 says is gone.
    gone: BTreeSet<AgentId>,
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
            last_checks: BTreeMap::new(),
            checks_failing: BTreeMap::new(),
            agents: BTreeMap::new(),
            gone: BTreeSet::new(),
            sending: Vec::new(),
            config_text,
            timing: Timing::default(),
            usage_source: UsageSource::Server,
            usage: None,
            usage_polled: None,
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
        self.hosts(board) && board.status == BoardStatus::Active
    }

    /// An autopilot board with this device among its homes, whatever its status.
    fn hosts(&self, board: &Board) -> bool {
        board.autopilot && board.home_devices.contains(&self.ctx.device)
    }

    /// Starts, renews or resumes the coordinators this device hosts.
    pub async fn reconcile(&mut self, now: Timestamp) -> Result<()> {
        let checks_until = tokio::time::Instant::now() + self.timing.reconcile_budget / 2;
        self.reload_config();
        let boards = self.client().boards().await?;
        self.boards = boards.iter().map(|b| (b.slug.clone(), b.clone())).collect();
        self.refresh_usage(now).await;
        let mut checks = self.check_threads(&boards, now, checks_until).await;
        for board in &boards {
            if !self.is_home(board) {
                self.failures.remove(&board.slug);
                self.state.pending_starts.remove(&board.slug);
                // A paused board keeps its coordinator thread and names it still, so the
                // server holds that thread while paused (`Snapshot::held_by`), and the same
                // thread is woken when the board resumes instead of a new one starting.
                if self.hosts(board) && board.status == BoardStatus::Paused {
                    continue;
                }
                if self.state.coordinators.remove(&board.slug).is_some() {
                    self.release(&board.slug).await;
                }
                continue;
            }
            let check = match checks.remove(&board.slug) {
                Some(Some(check)) => Some(check),
                // Says nothing of the thread, so nothing is changed or counted as failing.
                Some(None) => {
                    tracing::warn!(
                        "coordinator of {}: its thread check did not start within the reconcile's first {}s; checked on the next reconcile",
                        board.slug,
                        (self.timing.reconcile_budget / 2).as_secs()
                    );
                    continue;
                }
                None => None,
            };
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
    /// by board: asked [`THREAD_CHECKS_AT_ONCE`] at a time, each within
    /// [`Timing::thread_check`]; `None` for one not started by `until`. A thread the board
    /// names as its registered coordinator counts as live without asking when T3 found it
    /// live less than [`THREAD_RECHECK`] ago, and as failing when its check timed out that
    /// recently.
    async fn check_threads(
        &mut self,
        boards: &[Board],
        now: Timestamp,
        until: tokio::time::Instant,
    ) -> BTreeMap<BoardSlug, Option<Result<bool>>> {
        let mut checks = BTreeMap::new();
        let mut ask: Vec<(BoardSlug, String, Option<PathBuf>)> = Vec::new();
        for board in boards.iter().filter(|board| self.is_home(board)) {
            let Some(local) = self.state.coordinators.get(&board.slug) else {
                continue;
            };
            // A clock set back makes no check recent.
            let last = self
                .last_checks
                .get(&local.thread)
                .filter(|(_, at)| (TimeDelta::zero()..THREAD_RECHECK).contains(&(now - *at)));
            let project = self.project_for(board).map(|(_, project)| project);
            match (&board.coordinator, last) {
                (Some(named), _) if *named != local.agent => continue,
                // A folder found gone since is not left for the recheck.
                (Some(_), Some((LastCheck::Live(folder), _)))
                    if local.registered && !folder.as_deref().is_some_and(folder_gone) =>
                {
                    checks.insert(board.slug.clone(), Some(Ok(true)));
                }
                (Some(_), Some((LastCheck::TimedOut, at))) if local.registered => {
                    let err = anyhow::anyhow!(
                        "t3 thread show {} timed out at {at}; not asked again before {}",
                        local.thread,
                        *at + THREAD_RECHECK
                    );
                    checks.insert(board.slug.clone(), Some(Err(err)));
                }
                _ => ask.push((board.slug.clone(), local.thread.clone(), project)),
            }
        }
        let this = &*self;
        // Unordered, so a hung check holds only its own slot.
        let mut answers: Vec<(usize, Option<Result<RunsIn>>)> =
            futures_util::stream::iter(ask.iter().enumerate().map(
                |(i, (_, thread, project))| async move {
                    if tokio::time::Instant::now() >= until {
                        return (i, None);
                    }
                    (
                        i,
                        Some(this.thread_runs_in(thread, project.as_deref()).await),
                    )
                },
            ))
            .buffer_unordered(THREAD_CHECKS_AT_ONCE)
            .collect()
            .await;
        answers.sort_by_key(|(i, _)| *i);
        for ((slug, thread, _), (_, answer)) in ask.into_iter().zip(answers) {
            let answer = answer.map(|answer| match answer {
                Ok(Some(live)) => {
                    if let Some(runs) = live.runs
                        && let Some(local) = self.state.coordinators.get_mut(&slug)
                        && local.thread == thread
                    {
                        local.fresh.observe(runs);
                    }
                    self.last_checks
                        .insert(thread, (LastCheck::Live(live.folder), now));
                    Ok(true)
                }
                Ok(None) => {
                    self.last_checks.remove(&thread);
                    Ok(false)
                }
                Err(err) => {
                    if err.downcast_ref::<TimedOut>().is_some() {
                        self.last_checks.insert(thread, (LastCheck::TimedOut, now));
                    } else {
                        self.last_checks.remove(&thread);
                    }
                    Err(err)
                }
            });
            checks.insert(slug, answer);
        }
        let ours: Vec<&str> = self
            .state
            .coordinators
            .values()
            .map(|c| c.thread.as_str())
            .collect();
        self.last_checks
            .retain(|thread, _| ours.contains(&thread.as_str()));
        checks
    }

    /// Whether T3 has `thread` and can run it: not archived, and its folder (its worktree,
    /// else `project` when given) still exists, since T3 fails every run of a thread whose
    /// folder is gone.
    async fn thread_live(&self, thread: &str, project: Option<&Path>) -> Result<bool> {
        Ok(self.thread_runs_in(thread, project).await?.is_some())
    }

    /// [`Self::thread_live`], with the folder T3 runs a live thread in.
    async fn thread_runs_in(&self, thread: &str, project: Option<&Path>) -> Result<RunsIn> {
        let limit = self.timing.thread_check;
        match tokio::time::timeout(limit, self.t3.thread_info(thread)).await {
            Ok(info) => Ok(info?.filter(|t| t.runnable(project)).map(|t| Runnable {
                folder: t.folder(project).map(Path::to_path_buf),
                runs: t.runs,
            })),
            Err(_) => Err(TimedOut(format!("t3 thread show {thread}")).into()),
        }
    }

    /// Wake-ups queued for `agent`, whose thread a reconcile found gone, are not sent to it:
    /// a flush passes its answers and idle notices to the board's coordinator, as for any
    /// gone thread.
    fn thread_gone(&mut self, agent: &AgentId) {
        if self.state.wakes.contains_key(agent) {
            self.gone.insert(agent.clone());
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
        let project = self.project_for(board).map(|(_, project)| project);
        // Our own coordinator whose thread is gone: replace it whatever its agent record says.
        let mut ours_gone = false;
        if let Some(local) = self.state.coordinators.get(slug).cloned() {
            // No coordinator named: the lease lapsed (the device slept) or the start did not
            // finish. The thread may still be ours to run.
            if board.coordinator.as_ref().is_none_or(|c| *c == local.agent) {
                let named = board.coordinator.is_some() && local.registered;
                let live = match check {
                    Some(check) => check,
                    None => self.thread_live(&local.thread, project.as_deref()).await,
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
                tracing::warn!(
                    "coordinator of {slug}: thread {} is gone, archived or its folder is missing; replacing it",
                    local.thread
                );
                self.thread_gone(&local.agent);
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
                    if let Some(live) = self.thread_runs_in(&thread, project.as_deref()).await? {
                        self.last_checks
                            .insert(thread.clone(), (LastCheck::Live(live.folder), now));
                        // What it was briefed with is unknown: briefed again, once.
                        let local = Coordinator::new(thread, agent.clone(), now, true, None);
                        return self.adopt(board, local, now).await;
                    }
                    self.thread_gone(agent);
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
            // A re-brief held while the board was idle goes in front.
            let triage = self.triages(&board.slug);
            let digest = prompt_digest(triage);
            let owed = local.briefed.as_deref() != Some(digest);
            let resume = format!(
                "board {}: the usage limit has reset. Run board_snapshot and continue your loop.",
                board.slug
            );
            let message = if owed {
                format!("{}\n\n{resume}", rebrief_message(&board.slug, triage))
            } else {
                resume
            };
            if let Err(err) = self.t3.send(&local.thread, &message).await {
                // The resume is tried again on the next reconcile; a re-brief that went with
                // a send that timed out is looked for in the thread instead of resent.
                if owed && err.downcast_ref::<TimedOut>().is_some() {
                    let sent = Sent::MaybeDelivered(anyhow::anyhow!("{err:#}"));
                    settle_rebrief(&mut local, &board.slug, digest, &sent, now);
                    self.state.coordinators.insert(board.slug.clone(), local);
                }
                return Err(err);
            }
            local.resumed_after = Some(until);
            if owed {
                settle_rebrief(&mut local, &board.slug, digest, &Sent::Delivered, now);
            }
        }
        self.rebrief(board, &mut local, &info, now).await;
        self.state.coordinators.insert(board.slug.clone(), local);
        Ok(())
    }

    /// `thread`'s [`crate::t3::RECENT_MESSAGES`] latest messages, read within
    /// [`Timing::thread_check`]; `None` when T3 does not know the thread.
    async fn read_messages(&self, thread: &str) -> Result<Option<crate::t3::RecentMessages>> {
        tokio::time::timeout(self.timing.thread_check, self.t3.recent_messages(thread))
            .await
            .unwrap_or_else(|_| Err(TimedOut(format!("t3 thread show {thread}")).into()))
    }

    /// The [`message_key`] of the newest user message in `agent`'s thread
    /// ([`is_user_message`]), when a coordinator record of `agent` here has a fresh session
    /// [`FreshSession::due`]: `None` when none has, so nothing is read. `read`, the
    /// thread's latest messages when this flush read them already, spares a second read.
    async fn fresh_read(
        &self,
        agent: &AgentId,
        now: Timestamp,
        read: Option<&crate::t3::RecentMessages>,
    ) -> Option<Result<Option<String>>> {
        let due = self
            .state
            .coordinators
            .values()
            .any(|local| local.agent == *agent && local.fresh.due(now));
        if !due {
            return None;
        }
        let read = match read {
            Some(recent) => Ok(Some(recent.clone())),
            None => self.read_messages(agent.native()).await,
        };
        let read = match read {
            Ok(Some(recent)) => Ok(recent
                .user
                .iter()
                .rev()
                .find(|m| is_user_message(&m.text))
                .map(message_key)),
            Ok(None) => Err(anyhow::anyhow!(
                "T3 does not know thread {}",
                agent.native()
            )),
            Err(err) => Err(err),
        };
        Some(read)
    }

    /// Records `read`, a [`Self::fresh_read`] of `agent`'s thread, on each coordinator
    /// record of `agent` whose fresh session is due, and returns whether the read worked:
    /// one that failed says nothing of the user, so no notice goes with this wake-up.
    fn note_fresh_read(
        &mut self,
        agent: &AgentId,
        read: Option<Result<Option<String>>>,
        now: Timestamp,
    ) -> bool {
        match read {
            None => false,
            Some(Ok(newest)) => {
                for local in self.state.coordinators.values_mut() {
                    if local.agent == *agent && local.fresh.due(now) {
                        local.fresh.note_read(newest.clone(), now);
                    }
                }
                true
            }
            Some(Err(err)) => {
                tracing::warn!(
                    "waking {agent}: reading its thread for a fresh session: {err:#}; no notice this time"
                );
                false
            }
        }
    }

    /// Sends a live coordinator the standing prompt again when it changed since the thread
    /// was briefed (`board` was upgraded, or `feedback_board` changed), once. Held while the
    /// coordinator is paused, or runs and was seen within [`WAKE_HOLD_MAX`], so it lands
    /// between turns. Held too while its board is idle ([`is_idle`]), or wake-ups wait for
    /// it: the next wake-up carries it (see [`Self::flush`]), so a deploy costs an idle
    /// coordinator no turn. A failed send backs off. One that timed out counts as sent until
    /// a reconcile [`SEND_CHECK_AFTER`] later finds it missing ([`Self::rebrief_arrival`]),
    /// which makes it owed again, once. Either makes the next reconcile ask T3 whether the
    /// thread is gone.
    async fn rebrief(
        &mut self,
        board: &Board,
        local: &mut Coordinator,
        info: &Agent,
        now: Timestamp,
    ) {
        let slug = &board.slug;
        let triage = self.triages(slug);
        let digest = prompt_digest(triage);
        let held = match info.state {
            AgentState::Paused => true,
            AgentState::Running => now - info.last_seen < WAKE_HOLD_MAX,
            _ => false,
        };
        let backing_off = local
            .rebrief_backoff
            .as_ref()
            .is_some_and(|b| now < b.retry_at);
        // A re-brief with another prompt is owed in its place.
        if local
            .rebrief_unsure
            .as_ref()
            .is_some_and(|unsure| unsure.digest != digest)
        {
            local.rebrief_unsure = None;
        }
        // Looked for while the coordinator works too, so its turns do not push the message
        // out of the messages read first.
        let unsure_since = local
            .rebrief_unsure
            .as_ref()
            .filter(|unsure| !unsure.resent && local.briefed.as_deref() == Some(digest))
            .map(|unsure| unsure.since);
        if let Some(since) = unsure_since {
            if backing_off || now - since < SEND_CHECK_AFTER {
                return;
            }
            match self.rebrief_arrival(&local.thread, slug, triage).await {
                Arrival::Found => {
                    tracing::info!(
                        "re-briefed {} of {slug}: the send timed out, but the thread has the message",
                        local.agent
                    );
                    local.rebrief_unsure = None;
                    local.rebrief_backoff = None;
                    return;
                }
                Arrival::Waiting => {
                    local.rebrief_backoff = None;
                    return;
                }
                Arrival::Missing => {
                    tracing::warn!(
                        "re-briefing {} of {slug}: the send timed out and the thread never got \
                         the message; it is owed again",
                        local.agent
                    );
                    local.rebrief_backoff = None;
                    local.briefed = None;
                    local.rebrief_unsure = Some(UnsureRebrief {
                        digest: digest.to_owned(),
                        resent: true,
                        since,
                    });
                }
                Arrival::Unknown { err, gone } => {
                    // T3 answered that it has no such thread: the next reconcile asks again.
                    if gone {
                        self.last_checks.remove(&local.thread);
                    }
                    let doing = "looking for the timed-out re-brief of";
                    rebrief_back_off(local, slug, now, doing, &err);
                    return;
                }
            }
        }
        if local.briefed.as_deref() == Some(digest)
            || held
            || backing_off
            || is_idle(board)
            || self.state.wakes.contains_key(&local.agent)
            || self
                .state
                .unsure_wakes
                .get(&local.agent)
                .is_some_and(|u| u.holds(now))
        {
            return;
        }
        let sent = self
            .send_wake(&local.agent, &rebrief_message(slug, triage))
            .await;
        self.forget_thread_check(local.agent.native(), &sent);
        settle_rebrief(local, slug, digest, &sent, now);
    }

    /// Whether `thread` got the re-brief of `slug` whose send timed out, from its
    /// [`RECENT_MESSAGES`] latest messages: the newest user message with a re-brief marker
    /// of `slug` must carry this prompt's [`rebrief_marker`]. Without one, it counts as
    /// missing only when T3 has no run of the thread active or queued, which might still
    /// take the message.
    async fn rebrief_arrival(&self, thread: &str, slug: &BoardSlug, triage: bool) -> Arrival {
        let marker = rebrief_marker(slug, triage);
        let any_marker = format!(" of board {slug})");
        self.arrival(thread, |user| {
            user.iter()
                .rev()
                .map(|m| &m.text)
                .find(|text| text.contains("(re-brief ") && text.contains(&any_marker))
                .is_some_and(|text| text.contains(&marker))
        })
        .await
    }

    /// Whether `thread` got a message whose send timed out, from its [`RECENT_MESSAGES`]
    /// latest messages (see [`arrival_in`]).
    async fn arrival(
        &self,
        thread: &str,
        has: impl FnOnce(&[crate::t3::UserMessage]) -> bool,
    ) -> Arrival {
        match self.read_messages(thread).await {
            Ok(Some(recent)) => arrival_in(&recent, has),
            Ok(None) => Arrival::Unknown {
                err: anyhow::anyhow!("T3 does not know thread {thread}"),
                gone: true,
            },
            Err(err) => Arrival::Unknown { err, gone: false },
        }
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
    /// those found are settled; for those missing, their re-briefs are owed and their
    /// fresh-session notices due again, while their wake-ups are already in the queue the
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
        for message in sorted.missing {
            tracing::warn!(
                "waking {agent}: a send timed out and the thread never got the message; what \
                 it carried is sent again"
            );
            for mark in message.rebriefs {
                let Some(local) = self
                    .state
                    .coordinators
                    .get_mut(&mark.slug)
                    .filter(|c| c.agent == *agent && c.briefed.as_ref() == Some(&mark.digest))
                else {
                    continue;
                };
                local.briefed = None;
                local.rebrief_unsure = Some(UnsureRebrief {
                    digest: mark.digest,
                    resent: true,
                    since: message.since,
                });
            }
            for mark in message.fresh {
                if let Some(local) = self
                    .state
                    .coordinators
                    .get_mut(&mark.slug)
                    .filter(|c| c.agent == *agent)
                {
                    local.fresh.since = mark.since_before;
                }
            }
        }
    }

    /// The re-briefs `agent` is owed, for each board this device hosts it as coordinator
    /// of where it was briefed with another standing prompt; marked briefed at once. The
    /// message that carries them tracks them ([`UnsureMessage::rebriefs`]), so a flush cut
    /// short while sending leaves them to be looked for in the thread.
    fn take_owed_rebriefs(&mut self, agent: &AgentId, now: Timestamp) -> Vec<HeldRebrief> {
        let feedback = self.ctx.config.feedback_board();
        self.state
            .coordinators
            .iter_mut()
            .filter(|(_, local)| local.agent == *agent)
            .filter_map(|(slug, local)| {
                let triage = *slug == feedback;
                let digest = prompt_digest(triage);
                (local.briefed.as_deref() != Some(digest)).then(|| {
                    let resent = unsure_after(local, digest, now).is_none();
                    HeldRebrief {
                        slug: slug.clone(),
                        triage,
                        before: local.briefed.replace(digest.to_owned()),
                        unsure_before: local.rebrief_unsure.take(),
                        resent,
                    }
                })
            })
            .collect()
    }

    /// The boards this device hosts `agent` as coordinator of whose [`FreshSession`] is due,
    /// each restarted from its last `runCount` at once, so a flush cut short while sending
    /// does not say it twice.
    fn take_fresh_marks(&mut self, agent: &AgentId, now: Timestamp) -> Vec<FreshMark> {
        self.state
            .coordinators
            .iter_mut()
            .filter(|(_, local)| local.agent == *agent && local.fresh.due(now))
            .map(|(slug, local)| FreshMark {
                slug: slug.clone(),
                since_before: std::mem::replace(&mut local.fresh.since, local.fresh.runs),
            })
            .collect()
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

    /// [`project_for`] on this device.
    fn project_for(&self, board: &Board) -> Option<(Option<String>, PathBuf)> {
        project_for(&self.ctx.config, board)
    }

    /// [`host_folder`] on this device.
    fn host_folder(&self, board: &Board) -> Result<PathBuf> {
        host_folder(&self.ctx.config, board)
    }

    /// Whether `slug` is this device's feedback board, whose coordinator also triages.
    fn triages(&self, slug: &BoardSlug) -> bool {
        *slug == self.ctx.config.feedback_board()
    }

    async fn start_coordinator(&mut self, board: &Board, now: Timestamp) -> Result<()> {
        let slug = &board.slug;
        let project = self.host_folder(board)?;
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
        // Never in the project root, which other threads share: see `worktree`.
        let worktree = crate::worktree::coordinator_worktree(project, slug, now)
            .await
            .context("preparing the coordinator's worktree")?;
        // On disk before T3 can create the thread.
        self.state.pending_starts.entry(slug.clone()).or_insert(now);
        self.save()?;
        let thread = self
            .t3
            .start_thread(project, &title, &message, worktree.as_ref())
            .await?;
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
        let agent = match thread_agent(&adoption.thread) {
            Ok(agent) => agent,
            Err(err) => {
                drop_adoption(&self.ctx.home, slug, &adoption)?;
                return Err(err.context(format!("dropped the adoption for {slug}")));
            }
        };
        let project = self.host_folder(board)?;
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
                .is_some_and(|t| t.runnable(Some(&project)));
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
                    "coordinator of {slug}: thread {} to adopt is gone, archived or its folder is missing; starting a new thread",
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
            if let Ok(agent) = event.actor.parse::<AgentId>() {
                // An ended agent's thread may be gone: the next reconcile asks T3 about it.
                if event.payload["to"] == "ended" {
                    self.last_checks.remove(agent.native());
                }
                // Its thread ran again, or is asked about again once ended.
                self.gone.remove(&agent);
            }
            self.note_state(event, now);
            // Back at work: a notice that it was away no longer holds, for it or for the
            // coordinator it was passed to.
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
        let (reconcile, wakes) = self.examine(event).await?;
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
        match event.event_type {
            EventType::BoardCreated => return Ok((true, wakes)),
            // A board paused and active again: its coordinator, which stopped when it was
            // paused. The cached board is still paused until the reconcile this event asks
            // for, so its coordinator is read from it whatever its status; the wake-up is
            // checked again against the reconciled board when it is sent.
            EventType::BoardUpdated if event.payload["status"] == "active" => {
                let board = self.boards.get(slug).filter(|b| b.autopilot);
                let coordinator = board
                    .and_then(|b| b.coordinator.as_ref())
                    .or_else(|| self.state.coordinators.get(slug).map(|c| &c.agent));
                if let Some(to) = self.wakeable(coordinator, event) {
                    wakes.push((slug.clone(), to.clone(), WakeAbout::Resumed));
                }
                return Ok((true, wakes));
            }
            // This device's own lease claims and releases change nothing a reconcile would act on.
            EventType::BoardUpdated => return Ok((!self.is_own_claim(event), wakes)),
            // The server's notice that a task's owner has been away from it: the owner; the
            // coordinator when the owner is no T3 thread, which no daemon can wake. Whether
            // the owner is on this device and still away is checked when the wake-up is sent.
            EventType::TaskUpdated if event.payload.get("idle_doing").is_some() => {
                let away = event.payload["away"]
                    .as_str()
                    .and_then(|a| a.parse::<AgentId>().ok());
                let since =
                    serde_json::from_value::<Timestamp>(event.payload["idle_doing"].clone());
                if let (Some(task), Some(away), Ok(since)) = (&event.task, away, since) {
                    let to = match self.wakeable(Some(&away), event) {
                        Some(owner) => Some(owner.clone()),
                        None => self
                            .wakeable(self.coordinator_here(slug), event)
                            .filter(|c| **c != away)
                            .cloned(),
                    };
                    if let Some(to) = to {
                        let about = WakeAbout::Idle {
                            task: task.clone(),
                            away,
                            since,
                            noticed: event.at,
                        };
                        wakes.push((slug.clone(), to, about));
                    }
                }
            }
            EventType::TaskCreated | EventType::TaskReleased | EventType::TaskUpdated => {
                if let Some(task) = &event.task
                    && let Some(coordinator) = self.wakeable(self.coordinator_here(slug), event)
                {
                    // A move to review by the coordinator's own delegated threads is its own
                    // work in progress; any other is checked again when the line is sent.
                    if event.event_type == EventType::TaskUpdated && event.payload["to"] == "review"
                    {
                        if !self.delegated_by(&event.actor, coordinator).await? {
                            let about = WakeAbout::Review { task: task.clone() };
                            wakes.push((slug.clone(), coordinator.clone(), about));
                        }
                        return Ok((false, wakes));
                    }
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

    /// Whether `actor` is `coordinator` or a T3 thread it delegated, at any depth: the chain
    /// of `t3_parent` native ids from `actor` reaches `coordinator` within
    /// [`DELEGATION_DEPTH`] steps, as the server's delegation family follows it.
    async fn delegated_by(&self, actor: &str, coordinator: &AgentId) -> Result<bool> {
        let Ok(mut agent) = actor.parse::<AgentId>() else {
            return Ok(false);
        };
        for _ in 0..DELEGATION_DEPTH {
            if agent == *coordinator {
                return Ok(true);
            }
            if agent.source() != AgentSource::T3 {
                return Ok(false);
            }
            let info = match self.client().agent_info(&agent).await {
                Ok(info) => info,
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => return Ok(false),
                Err(err) => return Err(err),
            };
            let parent = info
                .native_ids
                .get("t3_parent")
                .and_then(|p| format!("t3:{p}").parse::<AgentId>().ok());
            let Some(parent) = parent else {
                return Ok(false);
            };
            agent = parent;
        }
        Ok(agent == *coordinator)
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
    /// A coordinator owed a re-brief, held while its board was idle, gets the standing
    /// prompt in front of its lines. Each message ends with a [`wake_marker`]; one whose
    /// send timed out is looked for in the thread [`SEND_CHECK_AFTER`] later, while the
    /// agent's next lines wait, and its wake-ups are queued again, once, when the thread
    /// never got it (see [`UnsureWakes`]).
    pub async fn flush(&mut self, now: Timestamp) {
        let deadline = tokio::time::Instant::now() + self.timing.flush_budget;
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
        type Read = (
            Checked,
            Option<Result<Option<String>>>,
            Option<Result<Sorted>>,
            Option<WakeQueue>,
        );
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
                        return (Checked::Waiting, None, look.read, requeued);
                    }
                    let checked = this.check(agent, queue, now).await;
                    // Read only for a message about to go out.
                    let fresh = match &checked {
                        Checked::Known {
                            rechecks: Some(lines),
                            ..
                        } if lines.iter().any(|l| matches!(l, Ok(Some(_)))) => {
                            this.fresh_read(agent, now, look.recent.as_ref()).await
                        }
                        _ => None,
                    };
                    (checked, fresh, look.read, requeued)
                })
                .await
                .unwrap_or((Checked::NotReached, None, None, None))
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
        for ((agent, queue, _), (checked, fresh, read, requeued)) in queues.into_iter().zip(checked)
        {
            self.settle_look(&agent, read, now);
            let fresh_read = self.note_fresh_read(&agent, fresh, now);
            let queue = requeued.unwrap_or(queue);
            if let Some(mut out) = self.apply(agent, queue, checked, now) {
                out.rebriefs = self.take_owed_rebriefs(&out.agent, now);
                for held in out.rebriefs.iter().rev() {
                    out.message = format!(
                        "{}\n\n{}",
                        rebrief_message(&held.slug, held.triage),
                        out.message
                    );
                }
                if fresh_read {
                    out.fresh = self.take_fresh_marks(&out.agent, now);
                }
                for mark in &out.fresh {
                    out.message = format!("{}\n{}", out.message, fresh_session_line(&mark.slug));
                }
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
        self.gone.retain(|agent| wakes.contains_key(agent));
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
        if self.gone.contains(agent) && seen.thread != ThreadCheck::Gone {
            seen.thread = ThreadCheck::Gone;
            read = true;
        }
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
            // Unknown to the server: its answers and idle notices go to the coordinator.
            Checked::Unknown => {
                let unheard = self.forward(agent, &mut queue.wakes, true);
                if unheard > 0 {
                    tracing::warn!(
                        "waking {agent}: unknown to the server; {unheard} answers to its asks \
                         or idle notices dropped, with no coordinator here to pass them to"
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
                // Waiting for a usage-limit reset is not being away: its idle notices no
                // longer hold, so none is sent at the reset.
                queue
                    .wakes
                    .retain(|w| !matches!(&w.about, WakeAbout::Idle { away, .. } if away == agent));
                self.forward(agent, &mut queue.wakes, false);
                self.requeue(agent.clone(), queue);
                return None;
            }
            // Another device's, or not to be woken by the daemon: its answers go to the
            // coordinator, and so do its idle notices when no device can wake it; the rest
            // is not ours to send.
            Reach::Never(why) => {
                let idle = matches!(why, Unreachable::NotT3 | Unreachable::ThreadGone);
                let dropped = queue
                    .wakes
                    .iter()
                    .filter(|w| !passed_on(agent, &w.about, idle))
                    .count();
                let unheard = self.forward(agent, &mut queue.wakes, idle);
                if !why.expected() {
                    if dropped > 0 {
                        tracing::warn!("waking {agent}: {why}; {dropped} wake-ups dropped");
                    }
                    if unheard > 0 {
                        tracing::warn!(
                            "waking {agent}: {why}; {unheard} answers to its asks or idle \
                             notices dropped, with no other coordinator here to pass them to"
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
            rebriefs: Vec::new(),
            fresh: Vec::new(),
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

    /// Passes the wake-ups [`passed_on`] names, which `from` cannot be woken for now, to the
    /// board's coordinator, once each: an answer or an idle notice no one hears of would
    /// stall its task. `idle` when no device can wake `from`. Returns how many have no
    /// coordinator to go to (none named, or `from` is it); a queue kept for later tries
    /// those again.
    fn forward(&mut self, from: &AgentId, wakes: &mut [Wake], idle: bool) -> usize {
        let mut unheard = 0;
        for wake in wakes {
            if !passed_on(from, &wake.about, idle) || wake.forwarded {
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
                resent: false,
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
            Ok(Ok(Some(info))) if !info.runnable(None) => Sent::Gone(err),
            _ => Sent::Failed(err),
        }
    }

    /// A send that failed may mean the thread is gone: the next reconcile asks T3. One that
    /// T3 answered as gone also ends the wait after a timed-out check, since T3 answers.
    fn forget_thread_check(&mut self, thread: &str, sent: &Sent) {
        let forget = match sent {
            Sent::Delivered => false,
            Sent::Gone(_) => true,
            Sent::MaybeDelivered(_) | Sent::Failed(_) => {
                matches!(self.last_checks.get(thread), Some((LastCheck::Live(_), _)))
            }
        };
        if forget {
            self.last_checks.remove(thread);
        }
    }

    /// Ends `out`'s message with its [`wake_marker`]. When it carries something not sent
    /// twice yet (wake-ups, re-briefs, fresh-session notices), records it in
    /// [`WatchState::unsure_wakes`] before the send, so a flush cut short while sending
    /// leaves it to be looked for; [`Self::settle`] drops it unless the send timed out.
    fn mark_unsure(&mut self, out: &mut Outgoing, now: Timestamp) {
        let digest = wake_digest(&out.message, now);
        out.message = format!("{}\n{}", out.message, wake_marker(&digest));
        let message = UnsureMessage {
            digest: digest.clone(),
            wakes: out.wakes.iter().filter(|w| !w.resent).cloned().collect(),
            rebriefs: out
                .rebriefs
                .iter()
                .filter(|held| !held.resent)
                .map(|held| RebriefMark {
                    slug: held.slug.clone(),
                    digest: prompt_digest(held.triage).to_owned(),
                })
                .collect(),
            fresh: out.fresh.clone(),
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
                    message
                        .rebriefs
                        .retain(|r| !out.rebriefs.iter().any(|d| d.slug == r.slug));
                    message
                        .fresh
                        .retain(|f| !out.fresh.iter().any(|d| d.slug == f.slug));
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
        self.forget_thread_check(agent.native(), &sent);
        let unsure_wakes = self.settle_unsure(&agent, &out, &sent);
        let recorded = out.unsure.is_some();
        // A failed send leaves the re-briefs owed; the wake-ups' back-off times the retry.
        for held in out.rebriefs {
            let Some(local) = self
                .state
                .coordinators
                .get_mut(&held.slug)
                .filter(|c| c.agent == agent)
            else {
                continue;
            };
            let digest = prompt_digest(held.triage);
            match &sent {
                Sent::Delivered => {
                    settle_rebrief(local, &held.slug, digest, &sent, now);
                }
                // Looked for by the flush, as part of the message.
                Sent::MaybeDelivered(err) => {
                    local.briefed = Some(digest.to_owned());
                    local.rebrief_backoff = None;
                    if held.resent {
                        tracing::warn!(
                            "re-briefing {} of {}: {err:#}; sent twice already, so not sent \
                             again",
                            local.agent,
                            held.slug
                        );
                    }
                }
                Sent::Gone(_) | Sent::Failed(_) => {
                    local.briefed = held.before;
                    local.rebrief_unsure = held.unsure_before;
                }
            }
        }
        // A failed send leaves the fresh session due, for the retry.
        if matches!(sent, Sent::Gone(_) | Sent::Failed(_)) {
            for mark in out.fresh {
                if let Some(local) = self
                    .state
                    .coordinators
                    .get_mut(&mark.slug)
                    .filter(|c| c.agent == agent)
                {
                    local.fresh.since = mark.since_before;
                }
            }
        }
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
                let unheard = self.forward(&agent, &mut wakes, true);
                if unheard > 0 {
                    tracing::warn!(
                        "waking {agent}: {unheard} answers to its asks or idle notices dropped, \
                         with no other coordinator here to pass them to"
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
    /// Whether the usage holds `slug` from new work at `now`: a window of a fresh reading
    /// is at or above the board's `usage_stop_at`.
    fn usage_holds(&self, slug: &BoardSlug, now: Timestamp) -> bool {
        let (Some(board), Some(usage)) = (self.boards.get(slug), &self.usage) else {
            return false;
        };
        usage.over(board.usage_stop_at, now).is_some()
    }

    /// Takes a new usage reading: from the endpoint, when [`USAGE_POLL_EVERY`] has passed
    /// since this daemon last read it, reported to the server; else the newest any device
    /// reported. Then wakes the coordinators of boards the usage no longer holds. A
    /// failure keeps the last reading, which goes stale and then holds nothing.
    async fn refresh_usage(&mut self, now: Timestamp) {
        let mut reported = None;
        if let UsageSource::ClaudeCode { url, credentials } = &self.usage_source
            && self
                .usage_polled
                .is_none_or(|at| now - at >= USAGE_POLL_EVERY)
        {
            self.usage_polled = Some(now);
            match crate::usage::read(url, credentials, now).await {
                Ok(Some(report)) => match self.client().report_usage(&report).await {
                    Ok(usage) => reported = Some(usage),
                    Err(err) => tracing::warn!("reporting the usage: {err:#}"),
                },
                Ok(None) => tracing::debug!(
                    "usage: Claude Code is not signed in here or its token has expired"
                ),
                Err(err) => tracing::warn!("reading the usage: {err:#}"),
            }
        }
        match reported {
            Some(usage) => self.usage = Some(usage),
            None => match self.client().usage().await {
                Ok(Some(usage)) => self.usage = Some(usage),
                Ok(None) => {}
                Err(err) => tracing::debug!("reading the usage from the server: {err:#}"),
            },
        }
        let held: BTreeSet<BoardSlug> = self
            .boards
            .keys()
            .filter(|slug| self.usage_holds(slug, now))
            .cloned()
            .collect();
        let eased: Vec<BoardSlug> = self.state.usage_held.difference(&held).cloned().collect();
        for slug in eased {
            if let Some(coordinator) = self
                .coordinator_here(&slug)
                .filter(|c| c.source() == AgentSource::T3)
                .cloned()
            {
                tracing::info!("usage: {slug} may start new work again; waking {coordinator}");
                self.queue(
                    coordinator,
                    Wake {
                        board: slug,
                        about: WakeAbout::UsageEased,
                        queued_at: now,
                        forwarded: false,
                        resent: false,
                    },
                );
            }
        }
        self.state.usage_held = held;
    }

    async fn recheck(
        &self,
        agent: &AgentId,
        wake: &Wake,
        now: Timestamp,
    ) -> Result<Option<String>> {
        let slug = &wake.board;
        // A paused board starts no turn; the coordinator is told what to check on resume.
        if self
            .boards
            .get(slug)
            .is_some_and(|b| b.status == BoardStatus::Paused)
        {
            return Ok(None);
        }
        let coordinator = self.coordinator_here(slug) == Some(agent);
        // High usage starts no new work: the coordinator is woken when it eases.
        if matches!(
            wake.about,
            WakeAbout::Ready { .. } | WakeAbout::Cleared { .. }
        ) && self.usage_holds(slug, now)
        {
            return Ok(None);
        }
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
            WakeAbout::Review { task } if coordinator => {
                let task = self.client().task(task).await?.task;
                (task.status == TaskStatus::Review).then(|| {
                    format!(
                        "board {slug}: {} moved to review; check it against its acceptance \
                         checks, or note what it still waits on",
                        task.id
                    )
                })
            }
            // For the owner, or the coordinator it was passed to: still the owner's, still in
            // doing, and the owner not back since the notice.
            WakeAbout::Idle {
                task,
                away,
                since,
                noticed,
            } if away == agent || coordinator => {
                let task = self.client().task(task).await?.task;
                let due = task.owner.as_ref() == Some(away)
                    && task.status == TaskStatus::Doing
                    && !self.back_since(away, *noticed).await?;
                let since = since.format("%Y-%m-%d %H:%MZ");
                due.then(|| {
                    if away == agent {
                        format!(
                            "board {slug}: your {} is in doing, and you have been away from it \
                             since {since}. Continue it, or park it by what it waits on: review \
                             with a note naming the PR, blocked with the reason, or open with \
                             your handoff",
                            task.id
                        )
                    } else {
                        format!(
                            "board {slug}: {} is in doing, and its owner {away}, which cannot \
                             be woken, has been away from it since {since}. Check it, then \
                             continue it, hand it over, or park it by what it waits on",
                            task.id
                        )
                    }
                })
            }
            WakeAbout::Resumed if coordinator => Some(format!(
                "board {slug}: the user resumed this board after a pause. Read your board \
                 snapshot and handoff, then continue. The pause stopped the threads on its \
                 tasks in doing: send each owner thread a message to continue its task, and \
                 restart a task only when its owner thread is gone. Check blocked tasks and \
                 answered asks too: their wake-ups were dropped while paused"
            )),
            WakeAbout::UsageEased if coordinator && !self.usage_holds(slug, now) => {
                let stop_at = self
                    .boards
                    .get(slug)
                    .map_or(board_proto::USAGE_STOP_AT_DEFAULT, |b| b.usage_stop_at);
                Some(format!(
                    "board {slug}: the account usage no longer stops new work on this board \
                     (stop at {stop_at}%). Read your board snapshot, then start ready tasks \
                     as usual"
                ))
            }
            // The agent is no longer this board's coordinator, or the usage is high again.
            WakeAbout::Resumed
            | WakeAbout::UsageEased
            | WakeAbout::Ready { .. }
            | WakeAbout::Cleared { .. }
            | WakeAbout::Proposal { .. }
            | WakeAbout::Review { .. }
            | WakeAbout::Idle { .. }
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

/// The folder a device runs the board's coordinator in: [`project_for`], which must not be
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

/// The message that gives a running coordinator its changed standing prompt in full,
/// ending with its [`rebrief_marker`].
pub fn rebrief_message(slug: &BoardSlug, triage: bool) -> String {
    let brief = brief(slug, triage);
    let marker = rebrief_marker(slug, triage);
    format!(
        "board {slug}: your standing instructions changed (board was upgraded, or this device's feedback_board changed). The text below replaces the instructions you were briefed with; keep your task in progress and what you know about the work.\n\n{brief} Continue your loop: call board_snapshot.\n{marker}"
    )
}

/// The last line of [`rebrief_message`]: it names the board and the prompt's digest, so a
/// reconcile finds the message among a thread's messages after its send timed out.
pub fn rebrief_marker(slug: &BoardSlug, triage: bool) -> String {
    format!(
        "(re-brief {} of board {slug})",
        &prompt_digest(triage)[..16]
    )
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

/// The standing prompt the coordinator of `slug` is briefed with, naming the board;
/// `triage` for the device's feedback board. `board brief <slug>` prints it, for a fresh
/// session in the same thread.
pub fn brief(slug: &BoardSlug, triage: bool) -> String {
    let prompt = standing_prompt(triage);
    format!("{prompt}\nYou are the coordinator of board {slug}.")
}

/// The first message of a coordinator thread; `triage` for the device's feedback board.
pub fn coordinator_message(slug: &BoardSlug, triage: bool) -> String {
    format!("{} Start now: call board_snapshot.", brief(slug, triage))
}

/// The message that briefs an existing thread adopted as a board's coordinator; `triage`
/// for the device's feedback board.
pub fn adoption_message(slug: &BoardSlug, triage: bool) -> String {
    let prompt = standing_prompt(triage);
    format!(
        "{prompt}\nThis thread is now also the coordinator of board {slug}: `board board adopt` named it, so the board's ready work comes to you from here on, and the board wakes you when work becomes ready, a task moves to review, one of your asks is answered, or an ask is answered whose asker it cannot wake. Keep what you already know about this work. A board task you already own counts as the board's task in progress: finish it first. Start now: call board_snapshot."
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
    daemon.usage_source = UsageSource::claude_code();
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
