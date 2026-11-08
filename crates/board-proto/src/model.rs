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
    /// When the agent last changed the board or touched a file, on the server's clock: its
    /// newest event other than a handoff, registration or state change. A handoff older
    /// than this is stale. Absent when the agent has changed nothing, and from servers that
    /// predate the field.
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
}
