//! Request and response bodies. Endpoint paths and semantics are in `docs/API.md`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    AgentId, AgentRole, AgentState, AskKind, BoardSlug, BoardStatus, DeviceName, Event,
    EvidenceKind, FeedbackActivity, FeedbackSource, NoteKind, Priority, ResolutionKind, Runtime,
    Task, TaskId, TaskStatus, Timestamp,
};

/// `POST /v1/agents`: register or refresh the calling agent (idempotent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterAgent {
    pub id: AgentId,
    pub runtime: Runtime,
    pub device: DeviceName,
    #[serde(default = "default_role")]
    pub role: AgentRole,
    pub title: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub native_ids: BTreeMap<String, String>,
    pub forked_from: Option<AgentId>,
}

fn default_role() -> AgentRole {
    AgentRole::Worker
}

/// `POST /v1/agents/{id}/state`: heartbeat and lifecycle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStateUpdate {
    pub state: AgentState,
    /// Set with `state: paused` when a usage limit stopped the agent.
    pub paused_until: Option<Timestamp>,
}

/// `POST /v1/boards`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateBoard {
    pub slug: BoardSlug,
    pub title: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub autopilot: bool,
    #[serde(default)]
    pub home_devices: Vec<DeviceName>,
    /// Close the board once idle and delete it later (see [`crate::Board::temporary`]).
    #[serde(default)]
    pub temporary: bool,
}

/// `PATCH /v1/boards/{slug}`. Absent fields are left unchanged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdateBoard {
    pub title: Option<String>,
    pub goal: Option<String>,
    pub status: Option<BoardStatus>,
    pub repos: Option<Vec<String>>,
    pub autopilot: Option<bool>,
    pub home_devices: Option<Vec<DeviceName>>,
    /// Turning it on for a board that is not temporary is for the user only, since the
    /// board is then deleted once idle.
    pub temporary: Option<bool>,
}

/// `POST /v1/boards/{slug}/join`: the calling agent joins the board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinBoard {
    pub agent: AgentId,
}

/// `POST /v1/boards/{slug}/coordinator`: take or renew the coordinator lease of an
/// autopilot board. The lease belongs to a device, so only one device starts a
/// coordinator thread; that device then names the thread's agent id once it exists.
/// The lease expires like a file lease unless renewed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimCoordinator {
    pub device: DeviceName,
    pub agent: Option<AgentId>,
}

/// Answer to `GET /v1/stats?days=`: counts for the feedback loop and the weekly report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub days: u32,
    pub tasks_created: u32,
    pub tasks_done: u32,
    pub claims_refused: u32,
    pub tasks_taken_stale: u32,
    pub lease_conflicts: u32,
    pub asks_created: u32,
    pub asks_answered: u32,
    /// Median minutes from an ask's creation to its answer.
    pub ask_answer_minutes_median: Option<f64>,
    /// Median minutes a task spent in `doing` before `done`.
    pub doing_minutes_median: Option<f64>,
    pub done_without_check_evidence: u32,
    /// Feedback reports and items. Absent from servers that predate feedback items.
    #[serde(default)]
    pub feedback: FeedbackStats,
}

/// Feedback counts in [`Stats`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackStats {
    /// Reports received: new items and additions to live ones.
    pub reported: u32,
    /// Feedback items filed.
    pub filed: u32,
    pub resolved: ResolvedCounts,
}

/// Feedback items resolved, by kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedCounts {
    pub routed: u32,
    pub duplicate: u32,
    pub forwarded: u32,
    pub declined: u32,
}

/// Longest feedback `about`.
pub const FEEDBACK_ABOUT_MAX: usize = 40;
/// Longest feedback `signature`.
pub const FEEDBACK_SIGNATURE_MAX: usize = 120;

/// `POST /v1/feedback`: report something in a setup that cost a step. Acts for the agent
/// in `X-Board-Agent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateFeedback {
    /// The setup: 1-40 characters of `a-z 0-9 : _ -`, `other` when unsure.
    pub about: String,
    /// The first line becomes the title; the whole text is the body.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<FeedbackActivity>,
    /// Defaults to `agent`; `signal` is the server's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<FeedbackSource>,
    /// Reports with equal signatures are one item. Derived from `about` and the title when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Answer to `POST /v1/feedback`: the item the report was filed on or added to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackResult {
    pub task: Task,
    /// The report was added to a live item instead of filing a new one.
    pub duplicate: bool,
    /// The item's reports, this one included.
    pub reports: u32,
}

/// `POST /v1/feedback/{task_id}/resolve`: close a feedback item, by its owner or the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolveFeedback {
    pub kind: ResolutionKind,
    /// Required for `routed` and `duplicate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    /// Required for `forwarded` and `declined`; at most
    /// [`RESOLUTION_REASON_CHARS_MAX`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `POST /v1/tasks`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateTask {
    pub board: BoardSlug,
    pub agent: AgentId,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub acceptance: String,
    pub parent: Option<TaskId>,
    #[serde(default)]
    pub blockers: Vec<TaskId>,
    #[serde(default = "default_priority")]
    pub priority: Priority,
    /// Defaults to the board's autopilot setting.
    pub auto: Option<bool>,
    /// Not ready before this time; a time already passed is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<Timestamp>,
}

fn default_priority() -> Priority {
    Priority::P2
}

/// `POST /v1/tasks/{id}/claim`, `/release` and `/take` (take a stale-owner task).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskAction {
    pub agent: AgentId,
}

/// `PATCH /v1/tasks/{id}`. Absent fields are left unchanged. `version` must match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateTask {
    pub agent: AgentId,
    pub version: i64,
    pub status: Option<TaskStatus>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub acceptance: Option<String>,
    pub priority: Option<Priority>,
    pub blockers: Option<Vec<TaskId>>,
    pub blocked_reason: Option<String>,
    pub auto: Option<bool>,
    /// Absent leaves it unchanged; `null` clears it, so the task no longer waits.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub not_before: Option<Option<Timestamp>>,
}

/// Deserializes a field that is present (`null` included) as `Some`; with `default`, an
/// absent field stays `None`.
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `POST /v1/tasks/{id}/done`: at least one evidence item is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoneTask {
    pub agent: AgentId,
    pub version: i64,
    pub evidence: Vec<NewEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewEvidence {
    pub kind: EvidenceKind,
    pub value: String,
    /// Required for `check`.
    pub exit_code: Option<i32>,
}

/// `POST /v1/tasks/{id}/notes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddNote {
    pub agent: AgentId,
    pub kind: NoteKind,
    pub text: String,
}

/// `PUT /v1/agents/{id}/handoff`: replaces the handoff. `version` is the version read,
/// or absent when creating the first one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PutHandoff {
    pub now: String,
    pub next: String,
    #[serde(default)]
    pub blockers: String,
    pub version: Option<i64>,
}

/// `POST /v1/leases`: claim globs in a repo for `ttl_secs` (default 2,700), renewing any
/// lease the agent already holds on the same glob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateLeases {
    pub agent: AgentId,
    pub repo: String,
    pub globs: Vec<String>,
    pub task: Option<TaskId>,
    pub ttl_secs: Option<u32>,
}

/// Answer to `POST /v1/leases`: the leases held, plus live leases of other agents that
/// overlap them (advisory; nothing is refused).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseResult {
    pub leases: Vec<crate::Lease>,
    pub conflicts: Vec<crate::Lease>,
}

/// Paths one `POST /v1/touches` accepts.
pub const TOUCH_PATHS_MAX: usize = 100;
/// Characters in one touched path or leased glob.
pub const PATH_CHARS_MAX: usize = 300;

/// `POST /v1/touches`: files the agent changed, relative to the repo root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Touches {
    pub agent: AgentId,
    pub repo: String,
    pub paths: Vec<String>,
    /// The paths were found by scanning the work tree after a shell call, not named by an
    /// edit tool, so another agent in the same checkout may have written them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scanned: bool,
    /// Modification times the client saw, by path (a subset of `paths`). A scanned path
    /// whose time equals a peer's edit-tool touch of it is the peer's write.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub modified: BTreeMap<String, Timestamp>,
}

/// Answer to `POST /v1/touches`: peers that leased or touched any of the paths recently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TouchResult {
    pub overlaps: Vec<Overlap>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Overlap {
    pub path: String,
    pub agent: AgentId,
    pub handle: String,
    /// `lease` or `touch`.
    pub via: String,
    pub at: Timestamp,
}

/// `POST /v1/asks`: ask the user for a decision or action. Notifies the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateAsk {
    pub agent: AgentId,
    pub board: BoardSlug,
    pub task: Option<TaskId>,
    pub kind: AskKind,
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
}

/// `POST /v1/asks/{id}/answer` (user only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnswerAsk {
    pub answer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    /// Everything the agent needs after a start, resume or compaction.
    Full,
    /// Only what changed since `since`; empty text when nothing relevant changed.
    Delta,
    /// Only alerts since `since`, for mid-turn hooks.
    Alerts,
}

/// Rendering variant, so formats can be benchmarked. `lines` is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotFormat {
    #[default]
    Lines,
    Json,
    Yaml,
}

/// Answer to `GET /v1/snapshot?agent=&kind=&since=&format=`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub kind: SnapshotKind,
    /// Ready to inject; empty when there is nothing to say.
    pub text: String,
    /// The event sequence this snapshot reflects; pass it as `since` next time.
    pub seq: i64,
    pub at: Timestamp,
}

/// Answer to `GET /v1/events?since=&board=&limit=`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventsPage {
    pub events: Vec<Event>,
    /// Pass as `since` to continue; equal to the request's `since` when nothing is new.
    pub next_since: i64,
}

/// `POST /v1/admin/devices` (user only): returns the token once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateDevice {
    pub name: DeviceName,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceToken {
    pub name: DeviceName,
    pub token: String,
}

/// A device as listed by `GET /v1/admin/devices`; never carries its token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: DeviceName,
    /// The token acts as the user.
    pub user: bool,
    pub created_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    Forbidden,
    NotFound,
    Invalid,
    /// The `version` sent is not the current one: re-read and retry.
    VersionConflict,
    /// Someone live owns the task.
    ClaimRefused,
    /// `done` without evidence, or `check` evidence without an exit code.
    EvidenceRequired,
    /// The status change is not allowed from the current status.
    InvalidTransition,
    Internal,
}

/// Every error response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    /// What to do next, phrased for an agent ("re-read factory-12 and retry").
    pub next: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Board;

    #[test]
    fn touches_from_older_clients_are_unscanned_without_times() {
        let old = serde_json::json!({"agent": "t3:a", "repo": "github.com/o/r", "paths": ["x"]});
        let touches: Touches = serde_json::from_value(old.clone()).unwrap();
        assert!(!touches.scanned && touches.modified.is_empty());
        assert_eq!(serde_json::to_value(&touches).unwrap(), old);
    }

    #[test]
    fn update_task_tells_an_absent_not_before_from_null() {
        let base = serde_json::json!({ "agent": "t3:a", "version": 1 });
        let absent: UpdateTask = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(absent.not_before, None);
        let mut cleared = base.clone();
        cleared["not_before"] = serde_json::Value::Null;
        let cleared: UpdateTask = serde_json::from_value(cleared).unwrap();
        assert_eq!(cleared.not_before, Some(None));
        let mut set = base;
        set["not_before"] = serde_json::json!("2026-11-21T00:00:00Z");
        let set: UpdateTask = serde_json::from_value(set).unwrap();
        assert_eq!(
            set.not_before,
            Some(Some("2026-11-21T00:00:00Z".parse().unwrap()))
        );
        // Round trip: unchanged stays absent, a clear stays null.
        let json = serde_json::to_value(&absent).unwrap();
        assert!(json.get("not_before").is_none());
        let json = serde_json::to_value(&cleared).unwrap();
        assert_eq!(json["not_before"], serde_json::Value::Null);
    }

    /// Stats and tasks from servers that predate feedback items still parse.
    #[test]
    fn feedback_fields_are_optional_on_the_wire() {
        let stats: Stats = serde_json::from_value(serde_json::json!({
            "days": 7, "tasks_created": 1, "tasks_done": 0, "claims_refused": 0,
            "tasks_taken_stale": 0, "lease_conflicts": 0, "asks_created": 0,
            "asks_answered": 0, "ask_answer_minutes_median": null,
            "doing_minutes_median": null, "done_without_check_evidence": 0,
        }))
        .unwrap();
        assert_eq!(stats.feedback, FeedbackStats::default());
        let create: CreateFeedback =
            serde_json::from_value(serde_json::json!({ "about": "board", "text": "Slow" }))
                .unwrap();
        assert_eq!((&create.source, &create.signature), (&None, &None));
        assert_eq!(
            serde_json::to_value(&create).unwrap(),
            serde_json::json!({ "about": "board", "text": "Slow" })
        );
    }

    /// Peers that predate `temporary` neither send nor expect it.
    #[test]
    fn temporary_is_optional_on_the_wire() {
        let create: CreateBoard =
            serde_json::from_value(serde_json::json!({ "slug": "factory", "title": "Factory" }))
                .unwrap();
        assert!(!create.temporary);
        let update: UpdateBoard = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(update.temporary, None);
        let board: Board = serde_json::from_value(serde_json::json!({
            "slug": "factory", "title": "Factory", "goal": "", "status": "active",
            "repos": [], "autopilot": false, "coordinator": null, "home_devices": [],
            "counts": { "open": 0, "doing": 0, "blocked": 0, "review": 0, "done": 0,
                        "dropped": 0, "ready": 0 },
            "open_asks": 0,
            "created_at": "2026-11-06T12:00:00Z", "updated_at": "2026-11-06T12:00:00Z",
        }))
        .unwrap();
        assert!(!board.temporary);
    }
}
