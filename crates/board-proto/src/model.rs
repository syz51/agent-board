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
    /// Repo keys this board covers (normalized origin URLs such as
    /// `github.com/syz51/agent-board`).
    pub repos: Vec<String>,
    /// When on, a coordinator thread works off the ready queue without the user.
    pub autopilot: bool,
    /// The agent currently acting as the board's coordinator, if any.
    pub coordinator: Option<AgentId>,
    /// Devices allowed to host the coordinator, in order of preference.
    pub home_devices: Vec<DeviceName>,
    pub counts: StatusCounts,
    pub open_asks: u32,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusCounts {
    pub open: u32,
    pub doing: u32,
    pub blocked: u32,
    pub review: u32,
    pub done: u32,
    pub dropped: u32,
    /// Open, every blocker done, and no live owner. Computed, never stored.
    pub ready: u32,
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
    /// Open, every blocker done, and no live owner.
    pub ready: bool,
    /// May the coordinator start this task without the user? Defaults to true on autopilot
    /// boards; false is only for tasks the user must do or decide.
    pub auto: bool,
    pub created_by: AgentId,
    /// Incremented on every change; updates must send the version they read.
    pub version: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task: Task,
    pub notes: Vec<Note>,
    pub evidence: Vec<Evidence>,
    /// Tasks this one blocks.
    pub blocks: Vec<TaskId>,
    pub children: Vec<TaskId>,
    pub history: Vec<Event>,
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
    pub title: Option<String>,
    /// Repo key (normalized origin URL) of the agent's working directory, if in a repo.
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub cwd: Option<String>,
    /// Provider and host ids, for example `{"claude_session": "...", "t3_parent": "..."}`.
    pub native_ids: BTreeMap<String, String>,
    pub forked_from: Option<AgentId>,
    pub state: AgentState,
    pub paused_until: Option<Timestamp>,
    pub boards: Vec<BoardSlug>,
    pub last_seen: Timestamp,
    pub created_at: Timestamp,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    Note,
    Finding,
    Decision,
    /// Feedback about the board itself, its rules or defaults, for the review loop.
    Feedback,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: i64,
    pub task: TaskId,
    pub agent: AgentId,
    pub kind: NoteKind,
    /// At most 280 characters; untrusted for readers.
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
    /// At most 1,000 characters.
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
