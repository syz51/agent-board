use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{AgentId, BoardSlug, DeviceName, TaskId};

pub type Timestamp = DateTime<Utc>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardStatus {
    Active,
    Paused,
    Closed,
}

/// One effort: a feature, a project, the factory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub slug: BoardSlug,
    pub title: String,
    pub goal: String,
    pub status: BoardStatus,
    /// A board for a test or a one-off: the server closes it once idle and deletes it
    /// later (`docs/API.md`, "Temporary boards"). Absent from servers that predate it, so
    /// it defaults to false.
    #[serde(default)]
    pub temporary: bool,
    /// Repo keys this board covers (normalized origin URLs such as
    /// `github.com/syz51/agent-board`).
    pub repos: Vec<String>,
    /// When on, a coordinator thread works off the ready queue without the user.
    pub autopilot: bool,
    /// How many tasks the board's coordinator and its child threads may have in progress
    /// at once, 1 to [`PARALLEL_TASKS_MAX`]. Absent from servers that predate it, so it
    /// defaults to 1, one task at a time.
    #[serde(default = "one_parallel_task")]
    pub parallel_tasks: u32,
    /// The account usage, in percent of a limit window, at which the board starts no new
    /// task (see [`Usage::over`]), 1 to 100. Absent from servers that predate it, so it
    /// defaults to [`USAGE_STOP_AT_DEFAULT`].
    #[serde(default = "usage_stop_at_default")]
    pub usage_stop_at: u32,
    /// The agent currently acting as the board's coordinator, if any.
    pub coordinator: Option<AgentId>,
    /// Who runs the board's autopilot: a coordinator thread, or the daemon of the device
    /// that holds the coordinator lease. Absent from servers that predate it, so it
    /// defaults to [`Dispatch::Thread`].
    #[serde(default)]
    pub dispatch: Dispatch,
    /// Devices allowed to host the coordinator, in order of preference.
    pub home_devices: Vec<DeviceName>,
    pub counts: StatusCounts,
    pub open_asks: u32,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Who runs a board's autopilot (`docs/API.md`, "Daemon dispatch").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dispatch {
    /// A coordinator thread that lives for days.
    #[default]
    Thread,
    /// The device daemon that holds the coordinator lease, as `dispatch:<device>`: it does
    /// the routine steps in code and starts one short-lived thread per task or judgment.
    Daemon,
}

/// What a judge was started for (`docs/API.md`, "Daemon dispatch").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeKind {
    /// Reviews a parked task against its acceptance checks.
    Review,
    /// Gives verdicts on proposals.
    Verdict,
    /// Reviews the board against its goal.
    Goal,
    /// Triages a feedback item.
    Triage,
}

/// A judge registered by the holder of a board's coordinator lease: a short-lived thread
/// with the coordinator's rights on that board until its agent ends or `expires_at`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Judge {
    pub board: BoardSlug,
    pub agent: AgentId,
    pub kind: JudgeKind,
    /// The tasks it was started for, on the board; empty for a goal review.
    pub tasks: Vec<TaskId>,
    /// The lease holder that registered it.
    pub registered_by: AgentId,
    pub created_at: Timestamp,
    /// At most [`JUDGE_HOURS_MAX`] hours after `created_at`.
    pub expires_at: Timestamp,
    /// Its agent is running, or has not registered yet: the judge is still at work. One
    /// whose turn ended (idle or paused) keeps its rights, but is no longer counted as a
    /// running judge.
    pub running: bool,
}

/// How long a judge keeps the coordinator's rights at most.
pub const JUDGE_HOURS_MAX: i64 = 2;

/// The most tasks one judge registration names.
pub const JUDGE_TASKS_MAX: usize = 20;

/// The most tasks a board's coordinator may run at once (see [`Board::parallel_tasks`]).
/// The usage limit is shared by every agent, so the range stays small.
pub const PARALLEL_TASKS_MAX: u32 = 4;

fn one_parallel_task() -> u32 {
    1
}

/// The default of [`Board::usage_stop_at`].
pub const USAGE_STOP_AT_DEFAULT: u32 = 90;

fn usage_stop_at_default() -> u32 {
    USAGE_STOP_AT_DEFAULT
}

/// A reading older than this is not acted on: the device that read it may be asleep.
pub const USAGE_FRESH_MINUTES: i64 = 30;

/// The Claude account's usage, as a device daemon last read it from the usage endpoint
/// (`docs/API.md`, "Usage"). Every agent shares it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// The rolling 5-hour window; absent when the endpoint gave none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<UsageWindow>,
    /// The weekly window; absent when the endpoint gave none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<UsageWindow>,
    /// When the device read it.
    pub read_at: Timestamp,
    /// The device that read it.
    pub device: DeviceName,
    /// The weekly pace the server computes while a reserve is set; absent while it is off,
    /// without a weekly window, or from a server that predates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pace: Option<UsagePace>,
}

/// Within this many points of a pace line, a board runs one task at a time and daemons
/// read the usage every minute.
pub const PACE_NEAR_POINTS: f64 = 2.0;

/// How far ahead [`UsagePace::projected`] looks, in minutes.
pub const PACE_PROJECT_MINUTES: i64 = 5;

/// How the weekly usage left is spread over the days to the weekly reset, keeping a
/// reserve for the user (`docs/DESIGN.md`, "Usage", Pace). Days run back from the reset in
/// 24-hour steps.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsagePace {
    /// Weekly percent kept for the user's own use.
    pub reserve: f64,
    /// When the current pace day began.
    pub day_start: Timestamp,
    /// Weekly percent at the day's first reading.
    pub anchor: f64,
    /// Weekly percent the day may reach: the anchor plus the day's share of what is left
    /// above the anchor and below the reserve.
    pub line: f64,
    /// Weekly percent projected [`PACE_PROJECT_MINUTES`] past the reading at the burn rate
    /// of the last 10 to 30 minutes.
    pub projected: f64,
    /// Since when the measured weekly use is at or above `100 - reserve`: agents working
    /// for a board halt, as for a pause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_since: Option<Timestamp>,
}

impl UsagePace {
    /// The percent at which agents working for a board halt.
    pub fn hard_line(&self) -> f64 {
        100.0 - self.reserve
    }

    /// The day's allowance in weekly points.
    pub fn allowance(&self) -> f64 {
        self.line - self.anchor
    }

    /// How many tasks a board with `parallel_tasks` may run at once: none once the
    /// projection reaches the day's line or the hard line is reached, one within
    /// [`PACE_NEAR_POINTS`] of the line, else all.
    pub fn limit(&self, parallel_tasks: u32) -> u32 {
        if self.hard_since.is_some() || self.projected >= self.line {
            0
        } else if self.projected >= self.line - PACE_NEAR_POINTS {
            parallel_tasks.min(1)
        } else {
            parallel_tasks
        }
    }

    /// Whether the weekly use at `percent` is still below the day's line and the hard line
    /// while it or the projection is within [`PACE_NEAR_POINTS`] of the lower one. Past the
    /// line nothing changes until the next day or the weekly reset.
    pub fn near(&self, percent: f64) -> bool {
        let line = self.line.min(self.hard_line());
        percent < line && self.projected.max(percent) >= line - PACE_NEAR_POINTS
    }

    /// When the next pace day begins.
    pub fn next_day(&self) -> Timestamp {
        self.day_start + chrono::TimeDelta::hours(24)
    }

    /// Whether `reserve` is a weekly percent a reserve may be: finite, from 0 to below 100.
    pub fn valid_reserve(reserve: f64) -> bool {
        reserve.is_finite() && (0.0..100.0).contains(&reserve)
    }
}

/// One limit window of [`Usage`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// Percent of the window's limit used: 0 to 100, above 100 once exceeded.
    pub percent: f64,
    /// When the window resets, if the endpoint said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<Timestamp>,
}

/// Which window of [`Usage`] a reading is about; not on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageWindowKind {
    FiveHour,
    SevenDay,
}

impl UsageWindowKind {
    /// How snapshots and the CLI name it.
    pub fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::SevenDay => "week",
        }
    }
}

impl Usage {
    /// The windows the endpoint gave, the 5-hour one first.
    pub fn windows(&self) -> impl Iterator<Item = (UsageWindowKind, UsageWindow)> {
        [
            (UsageWindowKind::FiveHour, self.five_hour),
            (UsageWindowKind::SevenDay, self.seven_day),
        ]
        .into_iter()
        .filter_map(|(kind, w)| w.map(|w| (kind, w)))
    }

    /// Whether the reading is recent enough to act on at `now`.
    pub fn fresh(&self, now: Timestamp) -> bool {
        now - self.read_at <= chrono::TimeDelta::minutes(USAGE_FRESH_MINUTES)
    }

    /// The fullest window at or above `stop_at` percent, if the reading is fresh. A window
    /// whose reset time has passed no longer counts, since it has emptied since the read.
    pub fn over(&self, stop_at: u32, now: Timestamp) -> Option<(UsageWindowKind, UsageWindow)> {
        if !self.fresh(now) {
            return None;
        }
        self.windows()
            .filter(|(_, w)| w.percent >= f64::from(stop_at))
            .filter(|(_, w)| w.resets_at.is_none_or(|at| at > now))
            .max_by(|a, b| a.1.percent.total_cmp(&b.1.percent))
    }

    /// The reset the account's agents wait for, if the reading is fresh: the latest reset
    /// still to come of a window at or above 100 percent, where every turn stops on the
    /// limit. A full window without a reset time gives none, since nothing says when it ends.
    pub fn full_until(&self, now: Timestamp) -> Option<Timestamp> {
        if !self.fresh(now) {
            return None;
        }
        self.windows()
            .filter(|(_, w)| w.percent >= 100.0)
            .filter_map(|(_, w)| w.resets_at)
            .filter(|at| *at > now)
            .max()
    }

    /// The pace, if the reading is fresh and its week has not reset by `now`.
    pub fn pace_at(&self, now: Timestamp) -> Option<&UsagePace> {
        let week = self.seven_day?;
        (self.fresh(now) && week.resets_at.is_none_or(|at| at > now))
            .then_some(self.pace.as_ref())
            .flatten()
    }

    /// How many tasks a board with `parallel_tasks` may run at once at `now`
    /// ([`UsagePace::limit`]); `parallel_tasks` without a pace.
    pub fn pace_limit(&self, parallel_tasks: u32, now: Timestamp) -> u32 {
        self.pace_at(now)
            .map_or(parallel_tasks, |p| p.limit(parallel_tasks))
    }

    /// Since when the measured weekly use is at the hard line, if it is at `now`.
    pub fn hard_since(&self, now: Timestamp) -> Option<Timestamp> {
        self.pace_at(now).and_then(|p| p.hard_since)
    }

    /// Whether a daemon should read the usage every minute: the pace is near a line.
    pub fn near_line(&self, now: Timestamp) -> bool {
        match (self.pace_at(now), self.seven_day) {
            (Some(p), Some(week)) => p.near(week.percent),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusCounts {
    pub open: u32,
    pub doing: u32,
    pub blocked: u32,
    pub review: u32,
    pub done: u32,
    pub dropped: u32,
    /// Open, every blocker done, no live owner, and `not_before` (if any) passed. Computed,
    /// never stored.
    pub ready: u32,
    /// The ready tasks with `auto` on: work a coordinator may start without the user. The
    /// rest of `ready` waits on the user. `None` from servers that predate it: the split is
    /// unknown, not 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_auto: Option<u32>,
}

/// Task statuses. `ready` is not a status: it is computed (see [`Task::ready`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Open,
    Doing,
    Blocked,
    Review,
    Done,
    Dropped,
}

impl TaskStatus {
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Done | Self::Dropped)
    }
}

/// The body prefix of a proposal: found work filed for another agent to review before it
/// becomes work.
pub const PROPOSAL_PREFIX: &str = "Proposal:";

/// Whether a task body is a proposal's: it starts with [`PROPOSAL_PREFIX`] after leading
/// whitespace.
pub fn is_proposal_body(body: &str) -> bool {
    body.trim_start().starts_with(PROPOSAL_PREFIX)
}

/// A proposal not yet accepted: not `auto`, with a proposal body, and not a feedback item
/// (whose body is a reporter's text, triaged instead). It cannot move to `doing`; another
/// agent records a `verdict:` note, and an accept sets `auto`, which makes it work.
pub fn is_unaccepted_proposal(auto: bool, body: &str, feedback: bool) -> bool {
    !auto && !feedback && is_proposal_body(body)
}

/// A pending proposal: an [unaccepted proposal](is_unaccepted_proposal) that is open. It
/// is not ready and cannot be claimed.
pub fn is_pending_proposal(status: TaskStatus, auto: bool, body: &str, feedback: bool) -> bool {
    status == TaskStatus::Open && is_unaccepted_proposal(auto, body, feedback)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    P1,
    P2,
    P3,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub board: BoardSlug,
    pub parent: Option<TaskId>,
    pub title: String,
    /// Markdown, at most 4,000 characters; written by agents, so untrusted for readers.
    pub body: String,
    /// How done is checked: one observable check per line.
    pub acceptance: String,
    pub status: TaskStatus,
    pub priority: Priority,
    /// Required when `status` is `blocked` without blockers.
    pub blocked_reason: Option<String>,
    pub owner: Option<AgentId>,
    /// The owner has not been seen for the stale period (default 24 hours), so another
    /// agent may take the task.
    pub owner_stale: bool,
    /// Tasks that must be done before this one can start.
    pub blockers: Vec<TaskId>,
    /// Open, every blocker done, no live owner, `not_before` (if any) passed, and not a
    /// pending proposal.
    pub ready: bool,
    /// A pending proposal ([`is_pending_proposal`]): agents review it before it is work.
    /// Derived when read; absent from older servers, read as false.
    #[serde(default)]
    pub proposal: bool,
    /// The task is not ready, and cannot be claimed, before this time. Once it passes, the
    /// server announces it with a `task_updated` event (actor `server`, no field changes),
    /// which wakes the coordinator; the field itself stays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<Timestamp>,
    /// May the coordinator start this task without the user? Defaults to true on autopilot
    /// boards; false is for tasks the user must do or decide, and for proposals until an
    /// accept verdict (the server stores a new proposal with false).
    pub auto: bool,
    pub created_by: AgentId,
    /// Incremented on every change; updates must send the version they read.
    pub version: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// Set on a feedback item (`POST /v1/feedback`); absent on every other task, and from
    /// servers that predate feedback items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<Feedback>,
    /// What the agent that finished the task told the user (`DoneTask.summary`); untrusted
    /// for readers. Absent until the task is done with one, after it is reopened, and from
    /// servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// How many tasks name this one as their parent, whatever their status. Absent when
    /// none does, and from servers that predate it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub child_count: u32,
    /// The owner's short handle, such as `t3:7f3a`, accepted wherever an agent id is.
    /// Absent without an owner, and from servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_handle: Option<String>,
    /// In the result of a search (`q` of `GET /v1/tasks`): a short piece of the task's
    /// text or notes around the words found, which stand in `[` and `]`; untrusted for
    /// readers. Absent when the words are in the id or title only, outside a search, and
    /// from servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// In the result of a search: no task holds every word, so this one holds at least
    /// one of them. Absent otherwise, and from servers that predate it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub any_word: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Task {
    /// [`is_pending_proposal`] from the task's own fields. Clients use it rather than
    /// `proposal`, which servers that predate it never send.
    pub fn is_pending_proposal(&self) -> bool {
        is_pending_proposal(self.status, self.auto, &self.body, self.feedback.is_some())
    }

    /// Whether the task's own fields allow it to be a live feedback item, one that takes
    /// reports: a feedback item that is unresolved and not closed, or declined, routed or a
    /// duplicate. Whether one of the latter is live depends on the task it names, which only
    /// the server sees (`live` on `GET /v1/tasks`), so a true answer is not proof. Clients
    /// use a false one to spot a server that ignored the filter.
    pub fn may_be_live(&self) -> bool {
        let Some(feedback) = &self.feedback else {
            return false;
        };
        match feedback.resolution.as_ref().map(|r| r.kind) {
            None => !self.status.is_closed(),
            Some(ResolutionKind::Forwarded | ResolutionKind::Fixed) => false,
            // `Unknown` is a kind from a newer server, which may take reports.
            Some(
                ResolutionKind::Declined
                | ResolutionKind::Routed
                | ResolutionKind::Duplicate
                | ResolutionKind::Unknown,
            ) => true,
        }
    }
}

/// What the reporter was doing when a setup cost it a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackActivity {
    Explore,
    Research,
    Design,
    Plan,
    Build,
    Review,
    Operate,
    Other,
}

/// Where a feedback report came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSource {
    /// An agent reported it (the `feedback` tool or `board feedback`).
    #[default]
    Agent,
    /// A capture job found it in transcripts.
    Capture,
    /// The server filed it from repeated API errors; clients cannot send it.
    Signal,
}

/// How a feedback item was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionKind {
    /// A task now does the work (`task`).
    Routed,
    /// The same as an earlier item (`task`), which gets later reports.
    Duplicate,
    /// Handed on outside the board (`reason` says where).
    Forwarded,
    /// Not acted on (`reason` says why); later reports still add to it.
    Declined,
    /// The resolver fixed the cause itself (`reason` says what changed); a later report
    /// files a new item that recurs this one.
    Fixed,
    /// A kind added by a newer server, read by a client that predates it. The server never
    /// stores or accepts it (`docs/API.md`, "Compatibility").
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackResolution {
    pub kind: ResolutionKind,
    /// The routed task, or the original of a duplicate; may be on another board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    /// Required for `forwarded`, `declined` and `fixed`; untrusted for readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at: Timestamp,
    /// The resolving agent id, or `user`.
    pub by: String,
}

/// A feedback item's report counts and resolution (`docs/API.md`, "Feedback").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feedback {
    /// The setup it is about: a slug such as `board`, `skills:rust` or `other`.
    pub about: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<FeedbackActivity>,
    pub source: FeedbackSource,
    /// Reports with the same signature add to this item while it is live.
    pub signature: String,
    /// Reports received, the first included, with those of the items resolved as its
    /// duplicates (and theirs). On a `signal` item, they are API errors.
    pub reports: u32,
    /// Agents that reported it or its duplicates, oldest report first; at most the 20 most
    /// recent.
    pub reporters: Vec<AgentId>,
    /// The latest report of it or its duplicates.
    pub last_reported_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<FeedbackResolution>,
    /// The earlier item with this signature, which was no longer live when this one was
    /// filed (its routed task was done, for example).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurs: Option<TaskId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task: Task,
    pub notes: Vec<Note>,
    pub evidence: Vec<Evidence>,
    /// Tasks this one blocks.
    pub blocks: Vec<TaskId>,
    /// Tasks that name this one as their parent, by id number.
    pub children: Vec<TaskId>,
    /// The same children in the same order, with their titles and statuses. Absent from
    /// servers that predate it, which send `children` alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_tasks: Vec<ChildTask>,
    pub history: Vec<Event>,
    /// Short handles of the agents this detail names (owner, authors, actors), by agent id;
    /// an id without one is shown as it is.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub handles: BTreeMap<String, String>,
}

/// A child task as its parent's detail lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildTask {
    pub id: TaskId,
    /// Written by agents, so untrusted for readers.
    pub title: String,
    pub status: TaskStatus,
    /// As [`Task::ready`].
    pub ready: bool,
    /// As [`Task::owner`] and [`Task::owner_stale`], so a parent's view shows who works on
    /// each child.
    pub owner: Option<AgentId>,
    pub owner_stale: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    Claude,
    Codex,
    Opencode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Running,
    Idle,
    /// Waiting for a usage limit to reset; see [`Agent::paused_until`].
    Paused,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Works on tasks it claims.
    Worker,
    /// Owns a board's ready queue on autopilot and delegates.
    Coordinator,
}

/// One thread: a T3 Code thread, or a standalone Claude Code or Codex session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    /// Short stable handle for display, such as `t3:7f3a`. Unique among agents.
    pub handle: String,
    pub runtime: Runtime,
    pub device: DeviceName,
    pub role: AgentRole,
    /// A T3 thread's current title (from the hooks); a board's coordinator keeps
    /// `<slug> coordinator` from its daemon.
    pub title: Option<String>,
    /// Repo key (normalized origin URL) of the agent's working directory, if in a repo.
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub cwd: Option<String>,
    /// Provider and host ids, for example `{"claude_session": "...", "t3_thread": "...",
    /// "t3_parent": "...", "t3_link": "https://..."}`; `t3_link` is the T3 thread's link in
    /// the T3 app, always `https://` (the server refuses any other).
    pub native_ids: BTreeMap<String, String>,
    pub forked_from: Option<AgentId>,
    pub state: AgentState,
    pub paused_until: Option<Timestamp>,
    pub boards: Vec<BoardSlug>,
    pub last_seen: Timestamp,
    pub created_at: Timestamp,
}

/// The last message of one turn of a board's coordinator, kept for the user
/// (`AgentStateUpdate.said`): what it said may be followed at once by a wake-up that
/// starts the next turn, so the user reads it here instead of in the thread. Written by the
/// agent, so untrusted for readers; never shown to agents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Said {
    pub id: i64,
    pub agent: AgentId,
    /// The board the agent was coordinating when it said it.
    pub board: BoardSlug,
    /// At most `SAID_CHARS_MAX` characters.
    pub text: String,
    pub at: Timestamp,
}

/// An agent's working memory: replaced as a whole, read first after a restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handoff {
    pub agent: AgentId,
    pub now: String,
    pub next: String,
    pub blockers: String,
    pub version: i64,
    pub updated_at: Timestamp,
    /// When the agent last made a change this handoff must cover, on the server's clock:
    /// after the handoff, its newest claim, release, take or completion of a task, its
    /// newest update of a task that is or was its own, or its newest file touch when that
    /// came more than 30 minutes after the handoff. A handoff older than this is stale. It
    /// is `updated_at` when there is no such change. Absent from servers that predate the
    /// field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_change_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    Note,
    Finding,
    Decision,
    /// Feedback about the board itself. Still accepted; agents report through
    /// `POST /v1/feedback` instead, which files a feedback item.
    Feedback,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: i64,
    pub task: TaskId,
    pub agent: AgentId,
    pub kind: NoteKind,
    /// At most 800 characters by default; untrusted for readers.
    pub text: String,
    pub at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A command that was run; `exit_code` is required.
    Check,
    Commit,
    Pr,
    Artifact,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub value: String,
    pub exit_code: Option<i32>,
    pub agent: AgentId,
    pub at: Timestamp,
}

/// An advisory claim on files or an area of a repo. Expires unless renewed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub id: i64,
    pub agent: AgentId,
    pub task: Option<TaskId>,
    pub repo: String,
    /// A glob relative to the repo root, such as `crates/board-server/**`.
    pub glob: String,
    pub expires_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// The user chooses among options or answers a question.
    Decision,
    /// The user must do something (log in, approve a payment, provide a key).
    Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskStatus {
    Open,
    Answered,
    Withdrawn,
}

/// Something only the user can decide or do. Creating one notifies the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ask {
    pub id: i64,
    pub board: BoardSlug,
    pub task: Option<TaskId>,
    pub agent: AgentId,
    pub kind: AskKind,
    /// At most 1,500 characters by default.
    pub question: String,
    /// Suggested answers, recommended first; the user may also answer freely.
    pub options: Vec<String>,
    pub status: AskStatus,
    pub answer: Option<String>,
    /// Where the answer came from: `web`, `telegram` or `cli`.
    pub answered_via: Option<String>,
    pub created_at: Timestamp,
    pub answered_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    BoardCreated,
    BoardUpdated,
    BoardJoined,
    TaskCreated,
    TaskClaimed,
    TaskReleased,
    TaskTaken,
    TaskUpdated,
    TaskDone,
    NoteAdded,
    EvidenceAdded,
    HandoffUpdated,
    LeaseCreated,
    LeaseReleased,
    LeaseExpired,
    FilesTouched,
    AgentRegistered,
    AgentStateChanged,
    AskCreated,
    AskAnswered,
    AskWithdrawn,
    /// A type added by a newer server, read by a client that predates it. The server never
    /// records it (`docs/API.md`, "Compatibility").
    #[serde(other)]
    Unknown,
}

/// One entry of the append-only event log, which is also the change feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: i64,
    pub at: Timestamp,
    pub board: Option<BoardSlug>,
    /// An agent id, `user`, or `server`.
    pub actor: String,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub task: Option<TaskId>,
    /// Type-specific details, such as `{"from": "open", "to": "doing"}` or `{"paths": [...]}`.
    pub payload: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_proposal_is_open_not_auto_with_a_proposal_body() {
        use TaskStatus::{Blocked, Open};
        assert!(is_proposal_body("Proposal: cache builds"));
        assert!(is_proposal_body("\n  Proposal: cache builds"));
        assert!(!is_proposal_body("Fix it. Proposal: later"));
        assert!(!is_proposal_body("proposal: lower case"));
        assert!(is_pending_proposal(Open, false, " Proposal: x", false));
        assert!(!is_pending_proposal(Open, true, "Proposal: x", false));
        assert!(!is_pending_proposal(Blocked, false, "Proposal: x", false));
        assert!(!is_pending_proposal(Open, false, "Ship it", false));
        // A feedback item is triaged, whatever its reporter wrote.
        assert!(!is_pending_proposal(Open, false, "Proposal: x", true));
        // A blocked proposal is not pending, but still not accepted.
        assert!(is_unaccepted_proposal(false, "Proposal: x", false));
        assert!(!is_unaccepted_proposal(true, "Proposal: x", false));
    }

    /// Servers that predate `proposal` send none: it reads as false, and old clients
    /// ignore it.
    #[test]
    fn task_proposal_is_false_when_absent() {
        let task = serde_json::json!({
            "id": "factory-1", "board": "factory", "parent": null, "title": "T", "body": "",
            "acceptance": "", "status": "open", "priority": "p2", "blocked_reason": null,
            "owner": null, "owner_stale": false, "blockers": [], "ready": true, "auto": true,
            "created_by": "t3:a", "version": 1,
            "created_at": "2026-11-06T12:00:00Z", "updated_at": "2026-11-06T12:00:00Z",
        });
        let read: Task = serde_json::from_value(task).unwrap();
        assert!(!read.proposal);
        assert_eq!(serde_json::to_value(&read).unwrap()["proposal"], false);
    }

    #[test]
    fn may_be_live_reads_a_feedback_items_own_fields() {
        let task = |status: &str, resolution: Option<&str>| -> Task {
            let mut feedback = serde_json::json!({
                "about": "board", "source": "agent", "signature": "board:x", "reports": 1,
                "reporters": ["t3:a"], "last_reported_at": "2026-11-06T12:00:00Z",
            });
            if let Some(kind) = resolution {
                feedback["resolution"] = serde_json::json!({
                    "kind": kind, "at": "2026-11-06T12:00:00Z", "by": "user",
                });
            }
            serde_json::from_value(serde_json::json!({
                "id": "board-feedback-1", "board": "board-feedback", "parent": null,
                "title": "T", "body": "", "acceptance": "", "status": status,
                "priority": "p2", "blocked_reason": null, "owner": null, "owner_stale": false,
                "blockers": [], "ready": false, "auto": false, "created_by": "t3:a",
                "version": 1, "created_at": "2026-11-06T12:00:00Z",
                "updated_at": "2026-11-06T12:00:00Z", "feedback": feedback,
            }))
            .unwrap()
        };
        assert!(task("open", None).may_be_live());
        assert!(task("doing", None).may_be_live());
        assert!(!task("done", None).may_be_live());
        assert!(!task("dropped", None).may_be_live());
        for kind in ["declined", "routed", "duplicate", "from-a-newer-server"] {
            assert!(task("done", Some(kind)).may_be_live(), "{kind}");
        }
        for kind in ["forwarded", "fixed"] {
            assert!(!task("done", Some(kind)).may_be_live(), "{kind}");
        }
        let mut plain = task("open", None);
        plain.feedback = None;
        assert!(!plain.may_be_live(), "a plain task takes no reports");
    }
}
