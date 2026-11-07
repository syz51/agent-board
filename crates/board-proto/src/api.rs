//! Request and response bodies. Endpoint paths and semantics are in `docs/API.md`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    AgentId, AgentRole, AgentState, Ask, AskKind, BoardSlug, BoardStatus, DeviceName, Event,
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
    /// Absent from servers that predate the `fixed` kind.
    #[serde(default)]
    pub fixed: u32,
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
    /// Required for `forwarded`, `declined` and `fixed`; at most 280 characters.
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
    /// Create the task in `doing`, owned by `agent`, in the same transaction, so no other
    /// agent can claim it first. Sent only when true: a server that predates it ignores
    /// the field and creates the task `open`, which the client sees in the answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub claim: bool,
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

/// The answer to `POST /v1/asks`: the new ask, and a warning when it may repeat another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatedAsk {
    #[serde(flatten)]
    pub ask: Ask,
    /// Set when the board has other open asks younger than the server's
    /// `duplicate_ask_window_secs`: names each (id, task, asker, age) and what to do.
    /// Absent otherwise, and from servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
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

    /// Older clients never send `claim`, and a request without it is the same on the wire.
    #[test]
    fn claim_is_optional_on_the_wire() {
        let old = serde_json::json!({ "board": "factory", "agent": "t3:a", "title": "Fix" });
        let create: CreateTask = serde_json::from_value(old.clone()).unwrap();
        assert!(!create.claim);
        assert_eq!(
            serde_json::to_value(&create).unwrap()["claim"],
            serde_json::Value::Null
        );
        let claimed = CreateTask {
            claim: true,
            ..create
        };
        assert_eq!(serde_json::to_value(&claimed).unwrap()["claim"], true);
    }

    /// Resolution counts from servers that predate `fixed` still parse.
    #[test]
    fn resolved_counts_without_fixed_parse() {
        let counts: ResolvedCounts = serde_json::from_value(serde_json::json!({
            "routed": 1, "duplicate": 2, "forwarded": 3, "declined": 4,
        }))
        .unwrap();
        assert_eq!((counts.declined, counts.fixed), (4, 0));
        let counts: ResolvedCounts = serde_json::from_value(serde_json::json!({
            "routed": 0, "duplicate": 0, "forwarded": 0, "declined": 0, "fixed": 5,
        }))
        .unwrap();
        assert_eq!(counts.fixed, 5);
    }

    /// A resolution kind or event type added by a newer server reads as `Unknown`, so a
    /// running client can still read the item or the event; known values are unchanged.
    #[test]
    fn newer_resolution_kinds_and_event_types_read_as_unknown() {
        let resolution: crate::FeedbackResolution = serde_json::from_value(serde_json::json!({
            "kind": "rerouted_somewhere", "reason": "x",
            "at": "2026-11-07T12:00:00Z", "by": "t3:a",
        }))
        .unwrap();
        assert_eq!(resolution.kind, ResolutionKind::Unknown);
        let fixed: ResolutionKind = serde_json::from_value(serde_json::json!("fixed")).unwrap();
        assert_eq!(fixed, ResolutionKind::Fixed);
        assert_eq!(serde_json::to_value(fixed).unwrap(), "fixed");
        let event: crate::Event = serde_json::from_value(serde_json::json!({
            "seq": 1, "at": "2026-11-07T12:00:00Z", "board": null, "actor": "server",
            "type": "board_renamed", "task": null, "payload": {},
        }))
        .unwrap();
        assert_eq!(event.event_type, crate::EventType::Unknown);
        let done: crate::EventType =
            serde_json::from_value(serde_json::json!("task_done")).unwrap();
        assert_eq!(done, crate::EventType::TaskDone);
    }

    /// A created ask is an `Ask` on the wire; `warning` is an optional extra field.
    #[test]
    fn a_created_ask_is_an_ask_with_an_optional_warning() {
        let ask = serde_json::json!({
            "id": 3, "board": "factory", "task": "factory-2", "agent": "t3:a",
            "kind": "decision", "question": "Ship it?", "options": ["yes"],
            "status": "open", "answer": null, "answered_via": null,
            "created_at": "2026-11-06T12:00:00Z", "answered_at": null,
        });
        // From a server that predates the warning.
        let created: CreatedAsk = serde_json::from_value(ask.clone()).unwrap();
        assert_eq!((created.ask.id, &created.warning), (3, &None));
        assert_eq!(serde_json::to_value(&created).unwrap(), ask);
        // Older clients read the answer as a plain `Ask`.
        let mut warned = ask;
        warned["warning"] = serde_json::json!("board factory has another open ask");
        let plain: Ask = serde_json::from_value(warned.clone()).unwrap();
        assert_eq!(plain.id, 3);
        let created: CreatedAsk = serde_json::from_value(warned).unwrap();
        assert_eq!(
            created.warning.as_deref(),
            Some("board factory has another open ask")
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
