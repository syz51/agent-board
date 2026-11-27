//! Request and response bodies. Endpoint paths and semantics are in `docs/API.md`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    AgentId, AgentRole, AgentState, Ask, AskKind, AutotuneDecision, AutotuneMode, BoardKind,
    BoardSlug, BoardStatus, ConflictKeySpec, DeviceCapacity, DeviceName, DeviceRole, DeviceStatus,
    Dispatch, DispatchHoldSummary, Event, Evidence, EvidenceKind, FeedbackActivity, FeedbackSource,
    HousekeepingAction, JudgeKind, NoteKind, Priority, QuotaBucketRef, RequestActor,
    ResolutionKind, ResourceWeight, Run, RunEventKind, RunId, RunKind, RunOrigin, RunState,
    Runtime, Said, SchedulerPolicy, SchedulerWeights, ScoreBreakdown, Taint, TaintToleration, Task,
    TaskAffinity, TaskId, TaskOrigin, TaskSize, TaskStatus, Timestamp, UsageWindow,
    default_run_cost_units,
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
    /// The last message of the turn that just ended (`Stop`). The server cleans and cuts it
    /// with [`turn_ending`] and keeps it as a `Said` only while the agent coordinates a
    /// board; it never fails the update. Servers that predate it ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub said: Option<String>,
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
    /// 1 to [`crate::PARALLEL_TASKS_MAX`]. Raising it is for the user only, since more
    /// tasks at once spend the shared usage limit faster; anyone may lower it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tasks: Option<u32>,
    /// Who runs the board's autopilot (see [`crate::Board::dispatch`]). Only
    /// [`Dispatch::Daemon`] is accepted, and it changes nothing; leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<Dispatch>,
    /// 1 to 100 (see [`crate::Board::usage_stop_at`]). Raising it is for the user only,
    /// since it lets the board spend more of the shared usage limit; anyone may lower it.
    /// A server that predates it ignores it, so a client checks the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_stop_at: Option<u32>,
    /// Whether a home device daemon starts automatic verdict judges for pending proposals
    /// when autopilot is off (see [`crate::Board::verdict_judges`]). A server that predates
    /// it ignores it, so a client checks the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_judges: Option<bool>,
    /// Optional expiry time for `parallel_tasks` and/or `usage_stop_at` changes in this
    /// request: the server stores the earlier value and restores it at `until`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
    /// Minimum interval in seconds between goal reviews
    /// (`0` disables goal reviews; otherwise [`crate::GOAL_REVIEW_EVERY_SECS_MIN`] to
    /// [`crate::GOAL_REVIEW_EVERY_SECS_MAX`]). Absent leaves it unchanged; `null` clears it.
    /// A server that predates it ignores it, so a client checks the answer.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub goal_review_every_secs: Option<Option<u64>>,
    /// Board kind (`project` or `inbox`). Absent leaves it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<BoardKind>,
    /// Optional per-board cadence in seconds for periodic backlog curation (`RunKind::Curate`),
    /// `0` to disable or [`crate::CURATE_EVERY_SECS_MIN`]..=[`crate::CURATE_EVERY_SECS_MAX`].
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub curate_every_secs: Option<Option<u64>>,
}

/// `PUT /v1/usage`: a device daemon reports the account usage it read. The server keeps
/// the newest reading by `read_at` and records the calling device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<UsageWindow>,
    pub read_at: Timestamp,
}

/// `PUT /v1/usage/reserve`: the weekly percent kept for the user's own use, from 0 to
/// below 100; `null` turns the pace off. Anyone may raise it or turn it on; only the user
/// may lower it or turn it off. The answer is the setting now.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsageReserve {
    /// Required: a missing field is refused rather than read as `null`, which turns the
    /// pace off.
    #[serde(deserialize_with = "Option::deserialize")]
    pub reserve: Option<f64>,
    /// Optional expiry time: in requests, the server stores the earlier reserve and
    /// restores it at `until`; in responses, when the current timed reserve expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
    /// In responses, the earlier reserve restored at `until` (`None` means pace was off,
    /// or no timed reserve is active when `until` is `None`). Ignored in requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<f64>,
}

/// `POST /v1/usage/approved` (user only): applies a usage reserve change the user
/// approved in their own words. The event records the calling agent and `said`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedUsageReserve {
    /// The user's words that approve this change, quoted; at most
    /// [`crate::APPROVAL_SAID_CHARS_MAX`] characters.
    pub said: String,
    /// Required: a missing field is refused rather than read as `null`, which turns the
    /// pace off.
    #[serde(deserialize_with = "Option::deserialize")]
    pub reserve: Option<f64>,
    /// Optional expiry time: the server stores the earlier reserve and restores it at
    /// `until`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
}

/// `PUT /v1/usage/{provider}/{account}`: a device daemon reports the usage windows it read
/// for one `(provider, account)` bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PutUsageBucket {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<UsageWindow>,
    pub read_at: Timestamp,
}

impl From<ReportUsage> for PutUsageBucket {
    fn from(req: ReportUsage) -> Self {
        Self {
            five_hour: req.five_hour,
            seven_day: req.seven_day,
            read_at: req.read_at,
        }
    }
}

/// `PUT /v1/usage/{provider}/{account}/reserve`: the weekly percent kept for the user's
/// own use on one bucket, from 0 to below 100; `null` turns the pace off.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PutUsageBucketReserve {
    /// Required: a missing field is refused rather than read as `null`, which turns the
    /// pace off.
    #[serde(deserialize_with = "Option::deserialize")]
    pub reserve: Option<f64>,
    /// Optional expiry time: the server stores the earlier reserve and restores it at
    /// `until`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
}

impl From<UsageReserve> for PutUsageBucketReserve {
    fn from(req: UsageReserve) -> Self {
        Self {
            reserve: req.reserve,
            until: req.until,
        }
    }
}

/// `POST /v1/boards/{slug}/judges`: the holder of the board's coordinator lease registers a
/// judge, which then has the coordinator's rights on the board until its agent ends or
/// for at most [`crate::JUDGE_HOURS_MAX`] hours. Registering the same agent again while
/// its registration is live replaces `kind` and `tasks` and keeps its expiry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterJudge {
    pub agent: AgentId,
    pub kind: JudgeKind,
    #[serde(default)]
    pub tasks: Vec<TaskId>,
}

/// `POST /v1/boards/{slug}/join`: the calling agent joins the board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinBoard {
    pub agent: AgentId,
}

/// `POST /v1/boards/{slug}/coordinator`: take or renew the coordinator lease of an
/// autopilot board. The lease belongs to a device, so only one device dispatches
/// the board; that device names its dispatch agent (`dispatch:<device>`).
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
    /// Tasks done after under a minute in `doing`, not counting takeovers or feedback
    /// items: work done off the board and claimed after the fact. Absent (`None`) from
    /// servers that predate it.
    #[serde(default)]
    pub done_under_a_minute: Option<u32>,
    /// Feedback reports and items. Absent from servers that predate feedback items.
    #[serde(default)]
    pub feedback: FeedbackStats,
    /// The oldest time the numbers cover: `days` back from now, or the oldest event the
    /// server keeps when that is later. Absent from servers that predate it.
    #[serde(default)]
    pub since: Option<Timestamp>,
    /// Per-board numbers over the same window, for each board with events in it, by slug.
    /// Empty from servers that predate them.
    #[serde(default)]
    pub boards: Vec<BoardStats>,
}

/// One board's numbers in [`Stats`]. Medians and percentiles are `None` when nothing was
/// measured; rates are per day of the window covered (from [`Stats::since`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardStats {
    pub board: BoardSlug,
    /// Claims of auto tasks that waited ready, not counting tasks created claimed.
    pub claims: u32,
    /// Minutes those tasks waited from ready to claimed.
    pub wait_minutes_median: Option<f64>,
    pub wait_minutes_p90: Option<f64>,
    pub tasks_done: u32,
    pub done_per_day: f64,
    /// Median minutes the tasks done spent in `doing`, time in `review` excluded.
    pub doing_minutes_median: Option<f64>,
    /// Lease conflicts and file-touch overlaps on the board's repos.
    pub lease_conflicts: u32,
    /// Tasks moved from `review` back to `open` or `doing`; a move to `blocked` only
    /// parks the task on another wait.
    pub reopened_after_review: u32,
    /// PR links in the evidence of the tasks done.
    pub pr_links: u32,
    /// Usage-limit pauses of the board's member agents.
    pub limit_pauses: u32,
    pub limit_pauses_per_day: f64,
    /// Hours those agents spent paused, within the window.
    pub paused_hours: f64,
    /// The most member agents running at the moment one of them paused.
    pub peak_running_at_pause: Option<u32>,
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

/// Answer to `GET /v1/errors?since=`: the API errors the server recorded since a time,
/// one row per signature, so errors under the signal threshold can be counted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorLog {
    /// Errors at or after this time are counted: the time asked for, or the oldest record
    /// kept when that is later.
    pub since: Timestamp,
    /// The server keeps error records this many days; `since` before that reads less.
    pub retention_days: u32,
    /// Most errors first.
    pub signatures: Vec<ErrorSignature>,
}

/// Most signatures one `GET /v1/errors` answer lists.
pub const ERRORS_LIMIT_MAX: u32 = 1_000;
/// Signatures `GET /v1/errors` lists when the request names no `limit`.
pub const ERRORS_LIMIT_DEFAULT: u32 = 100;
/// Longest `q` filter of `GET /v1/errors`, in characters.
pub const ERRORS_Q_CHARS_MAX: usize = 200;

/// Tasks `GET /v1/tasks` lists when the request names no `limit`.
pub const TASKS_LIMIT_DEFAULT: u32 = 200;
/// Most tasks one `GET /v1/tasks` answer lists.
pub const TASKS_LIMIT_MAX: u32 = 1_000;
/// Longest `q` search of `GET /v1/tasks`, in characters.
pub const TASKS_Q_CHARS_MAX: usize = 200;
/// Most words one `q` search of `GET /v1/tasks` takes.
pub const TASKS_Q_WORDS_MAX: usize = 8;

/// `POST /v1/errors`: an error the client answered itself, before sending the request it
/// would have made, such as text over a limit. The server records it with its own errors
/// (source `client`), for the calling agent, under the signature its own answer to that
/// request would get.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientError {
    /// Only `invalid` is recorded.
    pub code: ErrorCode,
    /// The route template the request would have gone to, such as
    /// `POST /v1/tasks/{id}/notes`.
    pub route: String,
    /// The message the server would have answered for text over a limit, such as `text is
    /// 1501 characters; the limit is 1500` ([`crate::TooLong::message`]), for one of
    /// [`crate::CLIENT_CHECKED_FIELDS`].
    pub message: String,
}

/// The routes a client may name in `POST /v1/errors`: every `/v1` route but that one, as
/// method and template (`docs/API.md`). The server's tests check that it routes each.
pub const CLIENT_ERROR_ROUTES: &[&str] = &[
    "POST /v1/agents",
    "GET /v1/agents",
    "GET /v1/agents/{id}",
    "POST /v1/agents/{id}/state",
    "GET /v1/agents/{id}/handoff",
    "PUT /v1/agents/{id}/handoff",
    "GET /v1/agents/{id}/said",
    "GET /v1/updates",
    "PUT /v1/updates/seen",
    "GET /v1/boards",
    "POST /v1/boards",
    "GET /v1/boards/{slug}",
    "PATCH /v1/boards/{slug}",
    "DELETE /v1/boards/{slug}",
    "POST /v1/boards/{slug}/join",
    "POST /v1/boards/{slug}/coordinator",
    "DELETE /v1/boards/{slug}/coordinator",
    "GET /v1/stats",
    "GET /v1/errors",
    "GET /v1/tasks",
    "POST /v1/tasks",
    "GET /v1/tasks/{id}",
    "PATCH /v1/tasks/{id}",
    "POST /v1/tasks/{id}/claim",
    "POST /v1/tasks/{id}/release",
    "POST /v1/tasks/{id}/take",
    "POST /v1/tasks/{id}/done",
    "POST /v1/tasks/{id}/notes",
    "POST /v1/leases",
    "DELETE /v1/leases/{id}",
    "POST /v1/touches",
    "POST /v1/feedback",
    "POST /v1/feedback/{id}/resolve",
    "GET /v1/asks",
    "POST /v1/asks",
    "GET /v1/asks/{id}",
    "POST /v1/asks/{id}/answer",
    "POST /v1/asks/{id}/withdraw",
    "GET /v1/snapshot",
    "GET /v1/events",
    "GET /v1/admin/devices",
    "POST /v1/admin/devices",
    "DELETE /v1/admin/devices/{name}",
];

/// Longest [`ClientError`] message, in characters.
pub const CLIENT_ERROR_MESSAGE_CHARS_MAX: usize = 300;

/// One error signature in [`ErrorLog`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorSignature {
    /// `signal:<code>:<hash>`, as on the signal feedback items.
    pub signature: String,
    /// The error code, as the wire spells it.
    pub code: String,
    /// The route template, such as `POST /v1/tasks/{id}/notes`.
    pub route: String,
    /// The message with numbers, task ids, agent ids and quoted text normalized.
    pub template: String,
    pub count: u32,
    /// Distinct agents that got it.
    pub agents: u32,
    pub first_at: Timestamp,
    pub last_at: Timestamp,
    /// The newest feedback item with this signature, when one was filed.
    pub item: Option<TaskId>,
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
    /// Required for `forwarded`, `declined` and `fixed`; at most
    /// [`crate::RESOLUTION_REASON_CHARS_MAX`] characters.
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
    /// Defaults to the board's autopilot setting. A body starting `Proposal:` is stored with
    /// false whatever is sent ([`crate::is_unaccepted_proposal`]).
    pub auto: Option<bool>,
    /// Not ready before this time; a time already passed is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<Timestamp>,
    /// Create the task in `doing`, owned by `agent`, in the same transaction, so no other
    /// agent can claim it first. Sent only when true: a server that predates it ignores
    /// the field and creates the task `open`, which the client sees in the answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub claim: bool,
    /// Target repository URL or normalized key; overrides `board.repos` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Coarse resource weight tier (`light`, `standard`, `heavy`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<ResourceWeight>,
    /// Explicit weighted capacity units overriding `weight`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_units: Option<u32>,
    /// Hard capability predicates required on the runner device.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Explicit opt-ins for tainted runner devices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<TaintToleration>,
    /// Fleet-wide concurrency keys acquired while a run executes this task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_keys: Vec<ConflictKeySpec>,
    /// Soft locality hint preferring a device or warm repository checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<TaskAffinity>,
    /// Optional preferred runner kind (`acp`, `t3`, `deterministic`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_runner: Option<String>,
    /// Explicit rank inside the task's priority level (lower first). Mutually exclusive with
    /// `rank_top`, `before`, and `after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
    /// Place this new task at rank 1 of its priority level, shifting existing ranked tasks down.
    /// Mutually exclusive with `rank`, `before`, and `after`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rank_top: bool,
    /// Place this new task immediately before the named task in the same board and priority level.
    /// Mutually exclusive with `rank`, `rank_top`, and `after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<TaskId>,
    /// Place this new task immediately after the named task in the same board and priority level.
    /// Mutually exclusive with `rank`, `rank_top`, and `before`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<TaskId>,
    /// Who originated the task (`user`, `feedback`, or `agent`). Defaults to `user` for user
    /// tokens / `user` actor and `agent` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<TaskOrigin>,
    /// Coarse effort size (`xs`, `s`, `m`, `l`) for WSJF scheduling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<TaskSize>,
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
    /// Acting agent (`<source>:<native id>`), or `"user"` / omitted for authenticated user
    /// sessions without `X-Board-Agent` (such as the browser Tailscale user). Required as an
    /// `AgentId` for device-token callers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<RequestActor>,
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
    /// The task's place inside its priority level, lower first. Absent leaves it unchanged,
    /// except that a changed priority clears it; `null` clears it. A server that predates
    /// it ignores it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub rank: Option<Option<u32>>,
    /// Hand the task over: the agent that owns it from now on. Absent leaves the owner as
    /// the status change sets it. Sent only when set; a server that predates it ignores it
    /// and answers with the owner unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<AgentId>,
    /// Why the task is dropped: only with `status: dropped`, stored as a `decision` note by
    /// the calling agent, or on the `task_updated` event's `"reason"` field when called
    /// directly as `"user"` without an agent id. Required for a drop on a board nobody runs
    /// by an agent that does not own the task and whose owner has left. Sent only when set; a
    /// server that predates it ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Absent leaves it unchanged; `null` clears it; a URL or key normalizes and sets it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub repo: Option<Option<String>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub weight: Option<Option<ResourceWeight>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub resource_units: Option<Option<u32>>,
    /// Absent leaves it unchanged; `[]` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<Vec<String>>,
    /// Absent leaves it unchanged; `[]` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerations: Option<Vec<TaintToleration>>,
    /// Absent leaves it unchanged; `[]` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_keys: Option<Vec<ConflictKeySpec>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub affinity: Option<Option<TaskAffinity>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub preferred_runner: Option<Option<String>>,
    /// Coarse effort size (`xs`, `s`, `m`, `l`). Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub size: Option<Option<TaskSize>>,
    /// Cool an open task into the Icebox (`true`) or revive it (`false`, refreshing `ready_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooled: Option<bool>,
    /// Reposition this task immediately before the named task in the same board and priority
    /// level. Mutually exclusive with `rank` and `after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<TaskId>,
    /// Reposition this task immediately after the named task in the same board and priority
    /// level. Mutually exclusive with `rank` and `before`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<TaskId>,
}

/// Deserializes a field that is present (`null` included) as `Some`; with `default`, an
/// absent field stays `None`.
pub fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `POST /v1/boards/{slug}/rank`: orders tasks inside their priority levels. The listed
/// tasks get the ranks 1, 2, 3 and so on in the order sent, counted per priority level, so
/// one list may hold several levels. A task of the board that is not listed keeps its rank.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankTasks {
    /// Acting agent (`<source>:<native id>`), or `"user"` / omitted for authenticated user
    /// sessions without `X-Board-Agent` (such as the browser Tailscale user). Required as an
    /// `AgentId` for device-token callers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<RequestActor>,
    /// At most 500 open tasks of the board, each once, first to start first.
    pub tasks: Vec<TaskId>,
    /// Clear the rank of every task of the board that is not listed.
    #[serde(default)]
    pub clear_others: bool,
}

/// The answer to [`RankTasks`]: the listed tasks as stored, in the order sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedTasks {
    pub tasks: Vec<Task>,
}

/// `POST /v1/tasks/{id}/done`: at least one evidence item is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoneTask {
    pub agent: AgentId,
    pub version: i64,
    pub evidence: Vec<NewEvidence>,
    /// For the user: what changed and what they should know, at most `SUMMARY_CHARS_MAX`
    /// characters. Shown in the web view's updates; servers that predate it ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Close a task in `review` whose owner is stale, taking it in the same step, without
    /// a move to `doing`. Servers that predate it ignore it and refuse the caller as not
    /// the owner.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub take: bool,
}

/// Longest `DoneTask.summary`, in characters.
pub const SUMMARY_CHARS_MAX: usize = 1500;

/// Longest `Said.text`, in characters.
pub const SAID_CHARS_MAX: usize = 4000;

/// A turn's last message as kept for the user: terminal escape sequences and control
/// characters other than line breaks and tabs removed, trimmed, and cut to
/// `SAID_CHARS_MAX` characters ending with `…`. `None` when nothing is left.
pub fn turn_ending(message: &str) -> Option<String> {
    let mut clean = String::with_capacity(message.len().min(SAID_CHARS_MAX * 4));
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters and intermediates up to a final byte in @..~.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST (ESC \).
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                // Any other escape is ESC and one character.
                _ => {}
            },
            '\n' | '\t' => clean.push(c),
            c if c.is_control() => {}
            c => clean.push(c),
        }
    }
    let clean = clean.trim();
    if clean.is_empty() {
        return None;
    }
    if clean.chars().count() <= SAID_CHARS_MAX {
        return Some(clean.to_owned());
    }
    let mut cut: String = clean.chars().take(SAID_CHARS_MAX - 1).collect();
    cut.push('…');
    Some(cut)
}

/// `GET /v1/updates`: what finished, what was dropped or cut by judges, and what coordinators
/// said since the user last looked, newest first, at most `UPDATES_MAX` of each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Updates {
    /// The user's seen cursor; absent until the user first marks updates seen.
    pub seen_at: Option<Timestamp>,
    /// The time the lists start after: `since` when given, else `seen_at`.
    pub since: Option<Timestamp>,
    pub done: Vec<DoneUpdate>,
    pub said: Vec<Said>,
    /// Tasks moved to `dropped` after `since` (declined/pruned), so drops are never silent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<DroppedUpdate>,
    /// `verdict:` decision notes after `since` that cut and dropped one or more sub-parts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cuts: Vec<VerdictCutUpdate>,
    /// True when any list was cut at `UPDATES_MAX`.
    pub more: bool,
}

/// Most items of each kind in `Updates`.
pub const UPDATES_MAX: usize = 100;

/// A task that was done, with what its agent told the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoneUpdate {
    pub task: TaskId,
    pub board: BoardSlug,
    pub parent: Option<TaskId>,
    pub title: String,
    pub summary: Option<String>,
    /// The agent that completed it.
    pub owner: Option<AgentId>,
    pub done_at: Timestamp,
    pub evidence: Vec<Evidence>,
}

/// A task moved to `dropped`, surfaced in `GET /v1/updates` so judge declines/drops stay visible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DroppedUpdate {
    pub task: TaskId,
    pub board: BoardSlug,
    pub parent: Option<TaskId>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<TaskOrigin>,
    /// Agent that recorded the drop decision, or `None` for a direct user drop.
    pub dropped_by: Option<AgentId>,
    pub reason: Option<String>,
    pub dropped_at: Timestamp,
}

/// A `verdict:` decision note that dropped one or more sub-parts via `cut: ..., dropped: ...`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerdictCutUpdate {
    pub task: TaskId,
    pub board: BoardSlug,
    pub title: String,
    pub judge: AgentId,
    pub note_id: i64,
    pub at: Timestamp,
    pub cut_lines: Vec<String>,
}

/// Extracts `cut:` lines that dropped scope (rather than splitting it to another task id)
/// from a `verdict:` decision note. Returns empty for non-verdict notes or split-only cuts.
pub fn extract_dropped_verdict_cuts(note_text: &str) -> Vec<String> {
    let trimmed = note_text.trim_start();
    if !trimmed
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("verdict:"))
    {
        return Vec::new();
    }
    // Tentative verdicts have not finalized a cut-drop yet; wait for confirmation so we
    // neither report unconfirmed drops nor double-count once confirmed.
    let first_line = trimmed.lines().next().unwrap_or("").trim();
    let after_verdict = first_line[8..].trim_start();
    if after_verdict
        .get(..9)
        .is_some_and(|p| p.eq_ignore_ascii_case("tentative"))
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    for raw_line in trimmed.lines() {
        let mut line = raw_line.trim();
        for prefix in ["- ", "* ", "• "] {
            if let Some(rest) = line.strip_prefix(prefix) {
                line = rest.trim_start();
                break;
            }
        }
        if !line
            .get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("cut:"))
        {
            continue;
        }
        // Inspect the body *after* stripping the `cut:` prefix so split-only descriptions
        // starting with "dropped" (e.g. `cut: dropped tasks audit -> factory-42`) do not
        // falsely match `: dropped`.
        let body_lower = line[4..].trim().to_ascii_lowercase();
        if body_lower.contains(", dropped")
            || body_lower.contains("-> dropped")
            || body_lower.contains("→ dropped")
            || body_lower.contains("— dropped")
            || body_lower.contains("– dropped")
            || body_lower.contains(": dropped")
            || body_lower.ends_with(" dropped")
        {
            out.push(line.to_owned());
        }
    }
    out
}

/// `PUT /v1/updates/seen`: everything up to `at` has been seen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarkSeen {
    pub at: Timestamp,
}

/// The user's seen cursor after `PUT /v1/updates/seen`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeenCursor {
    pub seen_at: Timestamp,
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
    /// Conflict lease ids for which every overlapping requested glob is already covered by
    /// an earlier live overlapping lease of the caller (including live leases from earlier
    /// calls, not only `leases`). Absent when empty or from servers that predate it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub later_conflicts: Vec<i64>,
    /// Set when the lease's `task` is on a board that lists repos and not the lease's repo,
    /// so peers working the task in those repos get no warning; says what to do instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
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
    /// The task the peer's lease names, when `via == "lease"` and the lease names a task;
    /// absent for touches, untasked leases, or servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    /// When `via == "lease"` and the caller also holds a live lease covering `path`,
    /// whether the caller's earliest such lease predates the peer's (`Some(true)` = caller's
    /// lease is older, `Some(false)` = peer's lease is older). Absent when the caller holds
    /// no lease on `path`, for touches, or from servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub your_lease_earlier: Option<bool>,
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
    /// A change to the ask's board that the server applies when the user answers with the
    /// approving option, which the server puts first ([`BoardChange::approving_option`]).
    /// A server that predates it ignores it, so a client checks the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<crate::BoardChange>,
}

/// `POST /v1/boards/{slug}/approved` (user only): applies a change the user approved in
/// their own words, outside an ask. The event records the calling agent and `said`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedBoardChange {
    /// The user's words that approve this change, quoted; at most
    /// [`crate::APPROVAL_SAID_CHARS_MAX`] characters.
    pub said: String,
    pub change: crate::BoardChange,
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
    /// The paused boards this agent works for, by slug: it is their coordinator, a thread
    /// that coordinator started, or the owner of a task in `doing` on them. The hooks end
    /// a held agent's turn. Empty, and absent from servers that predate it, otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held_by: Vec<BoardSlug>,
    /// When the latest of `held_by` was paused: the hooks halt only a turn that began
    /// before it. Absent when not held, and from servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_since: Option<Timestamp>,
    /// The hold is the weekly usage at the hard line (`100 - reserve`), not only a pause:
    /// `held_by` then lists every board the agent works for, and `held_since` is when
    /// the line was reached. Absent otherwise, and from servers that predate it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held_for_usage: bool,
    /// The boards in `held_by` that hold this agent as a stood-down coordinator: it last
    /// coordinated them, and they no longer run it (docs/DESIGN.md, "Standing down a
    /// coordinator"). Empty, and absent from servers that predate it, otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stood_down: Vec<StoodDownHold>,
}

/// A board that holds a stood-down coordinator, and why it stood down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoodDownHold {
    pub board: BoardSlug,
    /// Such as "autopilot was switched off".
    pub reason: String,
}

/// What ends the alert line (`! <alert> -> <action>`) of a peer's edit that overlaps the
/// agent's lease or edit. The hooks give a subagent only the alert lines that end with this
/// or [`LEASE_OVERLAP_ACTION`].
pub const EDIT_OVERLAP_ACTION: &str = "re-read before editing";
/// What ends the alert line of a peer's lease that overlaps the agent's lease or edit.
pub const LEASE_OVERLAP_ACTION: &str = "coordinate before editing those files";

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

/// `PUT /v1/devices/{name}/heartbeat`: device-self live capability and capacity report.
/// Only the device's own bearer token (or a `--user` token) may send it. Admin fields
/// (`role`, `taints`, `preference`, `drain`) are never accepted here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHeartbeat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<DeviceCapacity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_units: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runners: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protocol_features: Vec<String>,
}

/// `PATCH /v1/admin/devices/{name}` (user only): updates a device's plane isolation and
/// scheduling governance metadata. Absent fields are left unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminUpdateDevice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<DeviceRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taints: Option<Vec<Taint>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preference: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleepable: Option<bool>,
}

/// `POST /v1/devices/{name}/rpc` (user only): invoke an in-memory RPC method on an online
/// device running `board watch`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRpcRequest {
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// One queued RPC call delivered to a device by `GET /v1/devices/{name}/rpc/poll`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRpcCall {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    pub timeout_secs: u64,
}

/// `POST /v1/devices/{name}/rpc/{id}/reply` (device-self only): device's answer to a polled
/// [`DeviceRpcCall`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRpcReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response body of `POST /v1/devices/{name}/rpc`: the correlation `id` and the device's
/// reply (or a timeout error with `ok: false`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRpcResponse {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `POST /v1/runs`: create or submit a new [`crate::Run`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateRun {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board: Option<BoardSlug>,
    pub kind: RunKind,
    #[serde(default)]
    pub origin: RunOrigin,
    pub device: DeviceName,
    pub runner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    /// Initial state (`queued`, `leased`, `starting`, or `running`). Defaults to `leased`
    /// for managed runs and `running` for interactive runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<RunState>,
    /// Requested lease duration in seconds from the server clock. Mutually exclusive with
    /// `lease_expires_at`. Defaults to [`crate::RUN_LEASE_TTL_SECS_DEFAULT`] for live
    /// managed runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl_secs: Option<u64>,
    /// Explicit lease expiry timestamp. Mutually exclusive with `lease_ttl_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_hint: Option<String>,
    #[serde(default = "default_run_cost_units")]
    pub cost_units: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<String>,
    /// Fleet-wide concurrency locks acquired atomically with the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_keys: Vec<ConflictKeySpec>,
}

/// `PATCH /v1/runs/{id}`: transition state, renew lease, or update run metadata.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdateRun {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<RunState>,
    /// Renew lease by this many seconds from the server clock. Mutually exclusive with
    /// `lease_expires_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl_secs: Option<u64>,
    /// Renew lease to this explicit timestamp. Mutually exclusive with `lease_ttl_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<Timestamp>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub agent_id: Option<Option<AgentId>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub worktree_branch: Option<Option<String>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub cwd: Option<Option<String>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub title: Option<Option<String>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub attach_hint: Option<Option<String>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub policy_version: Option<Option<String>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub outcome: Option<Option<serde_json::Value>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub transcript_uri: Option<Option<String>>,
    /// Absent leaves it unchanged; `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub transcript_sha256: Option<Option<String>>,
    /// Cause or context recorded in the Trail Rule event (e.g., for cancel/fail/orphan).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Additional conflict locks acquired atomically (for example on `queued -> leased`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_keys: Vec<ConflictKeySpec>,
}

/// Query parameters for `GET /v1/runs`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board: Option<BoardSlug>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RunKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RunOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<RunState>,
    /// When `true`, returns only non-terminal states; when `false`, only terminal states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One live event submitted inside [`PushRunEvents`] (`POST /v1/runs/{id}/events`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewRunEvent {
    pub seq: i64,
    /// Optional event timestamp; defaults to the server clock when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Timestamp>,
    pub kind: RunEventKind,
    /// Single-line summary capped at [`crate::RUN_EVENT_SUMMARY_CHARS_MAX`] (300) characters.
    pub summary: String,
    #[serde(default, alias = "meta_json", skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

/// `POST /v1/runs/{id}/events`: append a batch of compact structured live events to a run's
/// bounded tail (latest [`crate::RUN_EVENTS_CAP`] events kept per run).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PushRunEvents {
    #[serde(default)]
    pub events: Vec<NewRunEvent>,
}

/// `POST /v1/runs/{id}/steer`: enqueue a steering message on the owning device's sticky wake
/// queue for an active run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerRunRequest {
    pub message: String,
}

/// Answer to `POST /v1/runs/{id}/steer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerRunResponse {
    pub wake_id: i64,
    pub run_id: RunId,
    pub device: DeviceName,
}

/// `POST /v1/runs/{id}/cancel`: cancel a queued or active run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRunRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Answer to `POST /v1/runs/{id}/cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRunResponse {
    pub run_id: RunId,
    pub device: DeviceName,
    pub state: RunState,
    /// Sticky wake id when cancellation was routed to the owning device; `None` when a
    /// `queued` run transitioned directly to `cancelled` on the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_id: Option<i64>,
}

/// `POST /v1/dispatch/claim`: atomic Admission -> Filter -> Score -> Lease grant
/// (`docs/SCHEDULER_DESIGN.md`, Section 3 Layer 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchClaimRequest {
    pub device: DeviceName,
    pub free_units: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runners: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    /// Managed run kinds this device is willing to claim. Empty defaults to all managed
    /// kinds (`worker`, `review`, `verdict`, `triage`, `goal`, `merge`, `housekeeping`,
    /// `command`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_kinds: Vec<RunKind>,
    /// Target `(provider, account)` usage bucket for LLM-backed runs. Defaults to
    /// `("anthropic", "default")` (`LEGACY_USAGE_PROVIDER`, `LEGACY_USAGE_ACCOUNT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_bucket: Option<QuotaBucketRef>,
    /// Maximum runs to claim in one call (default 1, clamped 1..=4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_claims: Option<u32>,
}

/// One run atomically leased to a device by `POST /v1/dispatch/claim`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimedRun {
    pub run: Run,
    pub board: BoardSlug,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Tasks covered by this run (up to 5 for batched `verdict`/`triage` runs, or the
    /// single task for task-scoped runs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TaskId>,
    pub score: f64,
    pub score_breakdown: ScoreBreakdown,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_keys: Vec<ConflictKeySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub housekeeping_action: Option<HousekeepingAction>,
}

/// Answer to `POST /v1/dispatch/claim`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DispatchClaimResponse {
    #[serde(default)]
    pub claimed: Vec<ClaimedRun>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holds_summary: Vec<DispatchHoldSummary>,
}

/// Answer to `GET /v1/scheduler/policy`. Flattens the active [`SchedulerPolicy`] and
/// optionally includes recent policy versions when `?history=true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerPolicyResponse {
    #[serde(flatten)]
    pub policy: SchedulerPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<SchedulerPolicy>,
}

/// `PUT /v1/scheduler/policy`: creates and activates a new immutable versioned policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PutSchedulerPolicy {
    pub version: String,
    #[serde(default)]
    pub weights: SchedulerWeights,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky_grace_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Sample count and nearest-rank percentiles (`p50`, `p90`) for a latency metric in seconds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LatencyPercentiles {
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90: Option<f64>,
}

/// Per-device utilization and reliability breakdown inside [`SchedulerStats`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceSchedulerStats {
    pub device: DeviceName,
    pub status: DeviceStatus,
    pub runs: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub orphaned: u32,
    pub weight_unit_minutes: f64,
}

/// Per-board scheduling wait, fair-share, and hold breakdown inside [`SchedulerStats`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardSchedulerStats {
    pub board: BoardSlug,
    pub runs: u32,
    pub succeeded: u32,
    pub orphaned: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_to_leased_p50_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_to_leased_p90_secs: Option<f64>,
    pub weight_unit_minutes: f64,
    /// Board's percentage of total `weight_unit_minutes` across boards in this window.
    pub fair_share_percent: f64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub holds_by_reason: BTreeMap<String, u32>,
}

/// Per-`policy_version` cohort metrics inside [`SchedulerStats`] for before/after evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyVersionStats {
    pub policy_version: String,
    pub runs: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub orphaned: u32,
    pub orphan_rate: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_to_leased_p50_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_to_leased_p90_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_p50_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_pass_merge_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_cache_hit_rate: Option<f64>,
    pub total_weight_unit_minutes: f64,
}

/// Per-`RunKind` token usage rollup inside [`SchedulerStats`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KindTokenStats {
    pub runs: u32,
    pub tokens_in: u64,
    pub cached_tokens: u64,
    pub tokens_out: u64,
    pub reasoning_tokens: u64,
    /// Weighted prompt-cache hit ratio across runs of this kind reporting `tokens_in > 0`
    /// and `cached_tokens`. Absent when no run of this kind reported cached tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_rate: Option<f64>,
}

/// Answer to `GET /v1/stats/scheduler?days=7&board=&policy_version=`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerStats {
    pub days: u32,
    pub since: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board: Option<BoardSlug>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<String>,
    pub runs_total: u32,
    #[serde(default)]
    pub runs_by_kind: BTreeMap<String, u32>,
    #[serde(default)]
    pub runs_by_terminal_state: BTreeMap<String, u32>,
    pub orphan_rate: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_pass_merge_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_cache_hit_rate: Option<f64>,
    pub ready_to_leased_secs: LatencyPercentiles,
    pub leased_to_running_secs: LatencyPercentiles,
    pub running_secs: LatencyPercentiles,
    pub conflict_wait_secs: LatencyPercentiles,
    pub total_weight_unit_minutes: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_ci_runs_before_merge: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_review_rounds: Option<f64>,
    /// Number of terminal runs in the window that recorded token usage.
    #[serde(default)]
    pub runs_with_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens_total: Option<u64>,
    /// Weighted prompt-cache hit ratio (`cached / tokens_in`) across runs with `tokens_in > 0`
    /// and `cached_tokens` reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_cache_hit_rate: Option<f64>,
    /// Per-`RunKind` token usage rollups across runs with recorded token usage.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens_by_kind: BTreeMap<String, KindTokenStats>,
    #[serde(default)]
    pub per_device: Vec<DeviceSchedulerStats>,
    #[serde(default)]
    pub per_board: Vec<BoardSchedulerStats>,
    #[serde(default)]
    pub per_policy_version: Vec<PolicyVersionStats>,
    #[serde(default)]
    pub holds_by_reason: BTreeMap<String, u32>,
    /// Most recent automated scheduler evaluation run, when at least one has been recorded.
    /// Absent from servers that predate `scheduler-autotune-v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_evaluation: Option<SchedulerEvaluation>,
}

/// One recorded run of the automated scheduler evaluation and guardrailed self-tuning loop
/// (`GET /v1/scheduler/evaluations`, `POST /v1/scheduler/evaluations/run`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerEvaluation {
    pub id: i64,
    pub evaluated_at: Timestamp,
    pub mode: AutotuneMode,
    pub decision: AutotuneDecision,
    pub baseline_policy_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_policy_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_policy_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_policy_version: Option<String>,
    pub window_days: u32,
    pub sample_runs: u32,
    pub sample_tasks: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_metrics: Option<ReplayMetrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_metrics: Option<ReplayMetrics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_weights: Option<SchedulerWeights>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_delta_pct: Option<f64>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_task_id: Option<TaskId>,
}

/// Answer to `GET /v1/scheduler/evaluations?limit=20`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerEvaluationsResponse {
    pub mode: AutotuneMode,
    pub enabled: bool,
    pub interval_secs: u64,
    #[serde(default)]
    pub evaluations: Vec<SchedulerEvaluation>,
}

fn default_candidate_version() -> String {
    "candidate".to_owned()
}

/// Candidate policy file format for `board admin replay-schedule --policy <file>` (TOML or JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayPolicyConfig {
    #[serde(default = "default_candidate_version")]
    pub version: String,
    #[serde(default)]
    pub weights: SchedulerWeights,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky_grace_secs: Option<u64>,
}

/// Metrics produced by one deterministic policy pass of `board admin replay-schedule`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayMetrics {
    pub policy_version: String,
    pub tasks_scheduled: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_wait_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90_wait_secs: Option<f64>,
    /// Gini coefficient of per-board scheduled `weight_unit_minutes` (`0.0` = equal, `1.0` = max skew).
    pub fair_share_gini: f64,
    pub conflict_blocks: u32,
    pub backfill_count: u32,
    pub locality_hits: u32,
}

/// Side-by-side output of `board admin replay-schedule`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayComparison {
    pub since: Timestamp,
    pub until: Timestamp,
    pub days: u32,
    pub historical_tasks: u32,
    pub historical_runs: u32,
    pub devices_count: u32,
    pub boards_count: u32,
    pub baseline: ReplayMetrics,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<ReplayMetrics>,
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
    fn turn_endings_lose_escapes_and_control_characters_and_fit() {
        assert_eq!(
            turn_ending("  \u{1b}[1;32mPR open\u{1b}[0m:\r\n\t- review it\u{7}  ").as_deref(),
            Some("PR open:\n\t- review it")
        );
        assert_eq!(
            turn_ending("\u{1b}]8;;https://x.test\u{1b}\\link\u{1b}]8;;\u{7} done").as_deref(),
            Some("link done")
        );
        assert_eq!(turn_ending("a\u{1b}(Bb").as_deref(), Some("aBb"));
        assert_eq!(turn_ending(" \u{7}\r\n "), None);
        let long = turn_ending(&"y".repeat(SAID_CHARS_MAX + 10)).unwrap();
        assert_eq!(long.chars().count(), SAID_CHARS_MAX);
        assert!(long.ends_with("y…"));
        let exact = "z".repeat(SAID_CHARS_MAX);
        assert_eq!(turn_ending(&exact).as_deref(), Some(exact.as_str()));
    }

    #[test]
    fn touches_from_older_clients_are_unscanned_without_times() {
        let old = serde_json::json!({"agent": "t3:a", "repo": "github.com/o/r", "paths": ["x"]});
        let touches: Touches = serde_json::from_value(old.clone()).unwrap();
        assert!(!touches.scanned && touches.modified.is_empty());
        assert_eq!(serde_json::to_value(&touches).unwrap(), old);
    }

    #[test]
    fn snapshots_from_older_servers_hold_no_one() {
        let old = serde_json::json!({
            "kind": "delta", "text": "", "seq": 3, "at": "2026-11-09T00:00:00Z"
        });
        let snap: Snapshot = serde_json::from_value(old.clone()).unwrap();
        assert!(snap.held_by.is_empty() && snap.held_since.is_none());
        assert_eq!(serde_json::to_value(&snap).unwrap(), old);
        let mut held = old;
        held["held_by"] = serde_json::json!(["factory"]);
        let snap: Snapshot = serde_json::from_value(held.clone()).unwrap();
        assert_eq!(snap.held_by, ["factory".parse::<BoardSlug>().unwrap()]);
        assert_eq!(snap.held_since, None, "a server without the pause time");
        held["held_since"] = serde_json::json!("2026-11-08T16:00:00Z");
        let snap: Snapshot = serde_json::from_value(held).unwrap();
        assert_eq!(
            snap.held_since,
            Some("2026-11-08T16:00:00Z".parse().unwrap())
        );
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

    #[test]
    fn update_task_tells_an_absent_rank_from_null() {
        let base = serde_json::json!({ "agent": "t3:a", "version": 1 });
        let absent: UpdateTask = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(absent.rank, None);
        assert!(serde_json::to_value(&absent).unwrap().get("rank").is_none());
        let mut cleared = base.clone();
        cleared["rank"] = serde_json::Value::Null;
        let cleared: UpdateTask = serde_json::from_value(cleared).unwrap();
        assert_eq!(cleared.rank, Some(None));
        assert_eq!(
            serde_json::to_value(&cleared).unwrap()["rank"],
            serde_json::Value::Null
        );
        let mut set = base;
        set["rank"] = serde_json::json!(3);
        let set: UpdateTask = serde_json::from_value(set).unwrap();
        assert_eq!(set.rank, Some(Some(3)));
    }

    /// `owner` is optional and sent only when set, so older peers see the same requests.
    #[test]
    fn update_task_owner_is_sent_only_when_set() {
        let base = serde_json::json!({ "agent": "t3:a", "version": 1 });
        let absent: UpdateTask = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(absent.owner, None);
        assert!(
            serde_json::to_value(&absent)
                .unwrap()
                .get("owner")
                .is_none()
        );
        let mut set = base;
        set["owner"] = serde_json::json!("t3:b");
        let set: UpdateTask = serde_json::from_value(set.clone()).unwrap();
        assert_eq!(set.owner.as_ref().map(AgentId::as_str), Some("t3:b"));
        assert_eq!(serde_json::to_value(&set).unwrap()["owner"], "t3:b");
        let mut bad = serde_json::json!({ "agent": "t3:a", "version": 1 });
        bad["owner"] = serde_json::json!("");
        assert!(serde_json::from_value::<UpdateTask>(bad).is_err());
    }

    /// A drop's `reason` is optional and sent only when set, so older peers see the same
    /// requests.
    #[test]
    fn update_task_reason_is_sent_only_when_set() {
        let base = serde_json::json!({ "agent": "t3:a", "version": 1 });
        let absent: UpdateTask = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(absent.reason, None);
        assert!(
            serde_json::to_value(&absent)
                .unwrap()
                .get("reason")
                .is_none()
        );
        let mut set = base;
        set["status"] = serde_json::json!("dropped");
        set["reason"] = serde_json::json!("the runtime was removed");
        let set: UpdateTask = serde_json::from_value(set.clone()).unwrap();
        assert_eq!(set.reason.as_deref(), Some("the runtime was removed"));
        assert_eq!(
            serde_json::to_value(&set).unwrap()["reason"],
            "the runtime was removed"
        );
    }

    #[test]
    fn update_task_and_rank_tasks_accept_agent_user_or_omitted() {
        let patch_agent: UpdateTask =
            serde_json::from_value(serde_json::json!({ "agent": "t3:a", "version": 1 })).unwrap();
        assert_eq!(
            patch_agent.agent,
            Some(RequestActor::Agent("t3:a".parse().unwrap()))
        );
        assert_eq!(serde_json::to_value(&patch_agent).unwrap()["agent"], "t3:a");

        let patch_user: UpdateTask =
            serde_json::from_value(serde_json::json!({ "agent": "user", "version": 1 })).unwrap();
        assert_eq!(patch_user.agent, Some(RequestActor::User));
        assert_eq!(serde_json::to_value(&patch_user).unwrap()["agent"], "user");

        let patch_omitted: UpdateTask =
            serde_json::from_value(serde_json::json!({ "version": 1 })).unwrap();
        assert_eq!(patch_omitted.agent, None);
        assert!(
            serde_json::to_value(&patch_omitted)
                .unwrap()
                .get("agent")
                .is_none()
        );

        assert!(
            serde_json::from_value::<UpdateTask>(
                serde_json::json!({ "agent": "invalid", "version": 1 })
            )
            .is_err()
        );

        let rank_agent: RankTasks = serde_json::from_value(serde_json::json!({
            "agent": "t3:a",
            "tasks": ["factory-1"],
            "clear_others": true
        }))
        .unwrap();
        assert_eq!(
            rank_agent.agent,
            Some(RequestActor::Agent("t3:a".parse().unwrap()))
        );
        assert_eq!(serde_json::to_value(&rank_agent).unwrap()["agent"], "t3:a");

        let rank_user: RankTasks = serde_json::from_value(serde_json::json!({
            "agent": "user",
            "tasks": ["factory-1"],
            "clear_others": false
        }))
        .unwrap();
        assert_eq!(rank_user.agent, Some(RequestActor::User));

        let rank_omitted: RankTasks = serde_json::from_value(serde_json::json!({
            "tasks": ["factory-1"]
        }))
        .unwrap();
        assert_eq!(rank_omitted.agent, None);
        assert!(
            serde_json::to_value(&rank_omitted)
                .unwrap()
                .get("agent")
                .is_none()
        );

        assert!(
            serde_json::from_value::<RankTasks>(serde_json::json!({
                "agent": "gpt:1",
                "tasks": ["factory-1"]
            }))
            .is_err()
        );
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
        assert_eq!(stats.done_under_a_minute, None);
        assert_eq!((stats.since, stats.boards.len()), (None, 0));
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

    /// `ready_auto` is part of the counts on the wire, next to the `ready` it splits.
    #[test]
    fn counts_carry_ready_auto() {
        let counts = crate::StatusCounts {
            ready: 5,
            ready_auto: Some(3),
            ..Default::default()
        };
        let value = serde_json::to_value(counts).unwrap();
        assert_eq!(
            (&value["ready"], &value["ready_auto"]),
            (&5.into(), &3.into())
        );
        let back: crate::StatusCounts = serde_json::from_value(value).unwrap();
        assert_eq!(back, counts);
        // Unknown stays unknown on the way back out.
        let unknown = crate::StatusCounts::default();
        let value = serde_json::to_value(unknown).unwrap();
        assert!(value.get("ready_auto").is_none(), "{value}");
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
        assert_eq!(
            board.counts.ready_auto, None,
            "older servers do not split ready work"
        );
        assert_eq!(
            board.parallel_tasks, 1,
            "older servers run one task at a time"
        );
        assert_eq!(update.parallel_tasks, None);
        assert_eq!(
            board.dispatch,
            Dispatch::Thread,
            "older servers run a coordinator thread"
        );
        assert_eq!(update.dispatch, None);
        assert_eq!(
            board.usage_stop_at,
            crate::USAGE_STOP_AT_DEFAULT,
            "older servers hold no work on usage"
        );
        assert_eq!(update.usage_stop_at, None);
        assert!(
            board.verdict_judges,
            "older servers run automatic verdict judges off autopilot"
        );
        assert_eq!(update.verdict_judges, None);
        // Older servers reject no field they do not know, and an unset one is not sent.
        let unset = serde_json::to_value(UpdateBoard::default()).unwrap();
        assert_eq!(unset["parallel_tasks"], serde_json::Value::Null);
        assert_eq!(unset["dispatch"], serde_json::Value::Null);
        assert_eq!(unset["usage_stop_at"], serde_json::Value::Null);
        assert!(unset.get("verdict_judges").is_none());
        let set: UpdateBoard = serde_json::from_value(serde_json::json!({
            "dispatch": "daemon",
            "verdict_judges": false,
        }))
        .unwrap();
        assert_eq!(set.dispatch, Some(Dispatch::Daemon));
        assert_eq!(set.verdict_judges, Some(false));
    }

    #[test]
    fn done_task_take_is_false_when_absent_and_left_out_when_false() {
        let req: DoneTask = serde_json::from_value(serde_json::json!({
            "agent": "t3:a", "version": 3,
            "evidence": [{ "kind": "pr", "value": "https://example.com/pull/1" }],
        }))
        .unwrap();
        assert!(!req.take);
        // Clients that do not take send what servers before the field accept.
        assert!(serde_json::to_value(&req).unwrap().get("take").is_none());
        let taking = DoneTask { take: true, ..req };
        assert_eq!(serde_json::to_value(&taking).unwrap()["take"], true);
    }

    #[test]
    fn judge_registration_tasks_default_to_none() {
        let req: RegisterJudge = serde_json::from_value(serde_json::json!({
            "agent": "t3:judge", "kind": "goal",
        }))
        .unwrap();
        assert_eq!(req.kind, JudgeKind::Goal);
        assert!(req.tasks.is_empty());
        assert!(
            serde_json::from_value::<RegisterJudge>(serde_json::json!({
                "agent": "t3:judge", "kind": "merge",
            }))
            .is_err()
        );
    }

    fn usage(five: Option<f64>, week: Option<f64>, read_min_ago: i64) -> crate::Usage {
        let now: Timestamp = "2026-11-08T12:00:00Z".parse().unwrap();
        let window = |percent| UsageWindow {
            percent,
            resets_at: Some(now + chrono::TimeDelta::hours(2)),
        };
        crate::Usage {
            five_hour: five.map(window),
            seven_day: week.map(window),
            read_at: now - chrono::TimeDelta::minutes(read_min_ago),
            device: "air".parse().unwrap(),
            pace: None,
            reserve_until: None,
            reserve_prev: None,
        }
    }

    #[test]
    fn the_pace_limit_falls_to_one_near_the_line_and_to_none_at_it() {
        let now: Timestamp = "2026-11-08T12:00:00Z".parse().unwrap();
        let mut u = usage(None, Some(78.5), 1);
        let pace = |projected| crate::UsagePace {
            reserve: 5.5,
            day_start: now - chrono::TimeDelta::hours(3),
            anchor: 78.0,
            line: 81.0,
            projected,
            hard_since: None,
        };
        assert_eq!(u.pace_limit(3, now), 3, "no pace without a reserve");
        u.pace = Some(pace(78.9));
        assert_eq!(u.pace_limit(3, now), 3);
        assert!(!u.near_line(now));
        u.pace = Some(pace(79.0));
        assert_eq!(u.pace_limit(3, now), 1, "within 2 points");
        assert!(u.near_line(now));
        u.pace = Some(pace(81.0));
        assert_eq!(u.pace_limit(3, now), 0, "at the line");
        u.pace = Some(crate::UsagePace {
            hard_since: Some(now),
            ..pace(70.0)
        });
        assert_eq!(u.pace_limit(3, now), 0, "at the hard line");
        assert_eq!(u.hard_since(now), Some(now));
        assert!(
            !u.near_line(now),
            "past the line nothing changes until the reset"
        );
        assert_eq!(
            u.hard_since(now + chrono::TimeDelta::minutes(30)),
            None,
            "stale"
        );
        assert_eq!(
            u.pace_limit(3, now + chrono::TimeDelta::hours(2)),
            3,
            "the week reset"
        );
        let old: crate::Usage = serde_json::from_value(serde_json::json!({
            "read_at": "2026-11-08T12:00:00Z", "device": "air",
        }))
        .unwrap();
        assert_eq!(old.pace, None, "a server that predates the pace");
    }

    #[test]
    fn usage_is_over_at_the_fullest_window_of_a_fresh_reading() {
        use crate::UsageWindowKind::{FiveHour, SevenDay};
        let now: Timestamp = "2026-11-08T12:00:00Z".parse().unwrap();
        let over = |u: &crate::Usage, at| u.over(at, now).map(|(k, w)| (k, w.percent));
        let u = usage(Some(95.0), Some(92.0), 1);
        assert_eq!(over(&u, 90), Some((FiveHour, 95.0)));
        assert_eq!(over(&u, 93), Some((FiveHour, 95.0)));
        assert_eq!(over(&u, 96), None);
        assert_eq!(
            over(&usage(None, Some(90.0), 1), 90),
            Some((SevenDay, 90.0))
        );
        assert_eq!(over(&usage(Some(99.0), None, 31), 90), None, "stale");
        assert_eq!(
            over(&usage(Some(99.0), None, 30), 90),
            Some((FiveHour, 99.0))
        );
        let mut reset = usage(Some(99.0), Some(91.0), 1);
        reset.five_hour.as_mut().unwrap().resets_at = Some(now);
        assert_eq!(
            over(&reset, 90),
            Some((SevenDay, 91.0)),
            "a reset window is empty"
        );
        reset.seven_day.as_mut().unwrap().resets_at = None;
        assert_eq!(
            over(&reset, 90),
            Some((SevenDay, 91.0)),
            "no reset time still counts"
        );
    }

    #[test]
    fn usage_is_full_until_the_latest_reset_of_a_window_at_100_percent() {
        let now: Timestamp = "2026-11-08T12:00:00Z".parse().unwrap();
        let hours = chrono::TimeDelta::hours;
        let with_resets = |five: f64, week: f64, minutes_ago| {
            let mut u = usage(Some(five), Some(week), minutes_ago);
            u.five_hour.as_mut().unwrap().resets_at = Some(now + hours(2));
            u.seven_day.as_mut().unwrap().resets_at = Some(now + hours(50));
            u
        };
        assert_eq!(with_resets(99.9, 95.0, 1).full_until(now), None);
        assert_eq!(
            with_resets(100.0, 95.0, 1).full_until(now),
            Some(now + hours(2))
        );
        assert_eq!(
            with_resets(100.0, 104.0, 1).full_until(now),
            Some(now + hours(50)),
            "the later reset"
        );
        assert_eq!(with_resets(100.0, 100.0, 31).full_until(now), None, "stale");
        let full = with_resets(100.0, 95.0, 1);
        assert_eq!(full.full_until(now + hours(2)), None, "the window reset");
        let mut unknown = with_resets(100.0, 95.0, 1);
        unknown.five_hour.as_mut().unwrap().resets_at = None;
        assert_eq!(unknown.full_until(now), None, "no reset time to wait for");
    }

    #[test]
    fn a_usage_report_needs_only_the_read_time() {
        let req: ReportUsage =
            serde_json::from_value(serde_json::json!({ "read_at": "2026-11-08T12:00:00Z" }))
                .unwrap();
        assert_eq!((req.five_hour, req.seven_day), (None, None));
    }

    #[test]
    fn scheduler_open_enums_read_future_values_as_unknown() {
        use crate::{DeviceRole, DeviceStatus, ResourceWeight, TaintEffect};
        assert_eq!(
            serde_json::from_value::<DeviceRole>(serde_json::json!("edge_gpu")).unwrap(),
            DeviceRole::Unknown
        );
        assert_eq!(
            serde_json::from_value::<DeviceRole>(serde_json::json!("service_host")).unwrap(),
            DeviceRole::ServiceHost
        );
        assert_eq!(
            serde_json::from_value::<DeviceStatus>(serde_json::json!("draining_future")).unwrap(),
            DeviceStatus::Unknown
        );
        assert_eq!(
            serde_json::from_value::<ResourceWeight>(serde_json::json!("xlarge")).unwrap(),
            ResourceWeight::Unknown
        );
        assert_eq!(
            serde_json::from_value::<TaintEffect>(serde_json::json!("no_execute")).unwrap(),
            TaintEffect::Unknown
        );
        assert_eq!(
            serde_json::to_value(crate::EventType::DeviceUpdated).unwrap(),
            "device_updated"
        );
    }

    #[test]
    fn task_routing_fields_are_optional_and_distinguish_absent_from_null() {
        let base = serde_json::json!({ "agent": "t3:a", "version": 1 });
        let absent: UpdateTask = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(absent.repo, None);
        assert_eq!(absent.weight, None);
        assert_eq!(absent.resource_units, None);
        assert_eq!(absent.requires, None);
        assert_eq!(absent.tolerations, None);
        assert_eq!(absent.conflict_keys, None);
        assert_eq!(absent.affinity, None);
        assert_eq!(absent.preferred_runner, None);
        let serialized = serde_json::to_value(&absent).unwrap();
        for key in [
            "repo",
            "weight",
            "resource_units",
            "requires",
            "tolerations",
            "conflict_keys",
            "affinity",
            "preferred_runner",
        ] {
            assert!(serialized.get(key).is_none(), "{key} leaked: {serialized}");
        }

        let mut cleared = base;
        cleared["repo"] = serde_json::Value::Null;
        cleared["weight"] = serde_json::Value::Null;
        cleared["resource_units"] = serde_json::Value::Null;
        cleared["affinity"] = serde_json::Value::Null;
        cleared["preferred_runner"] = serde_json::Value::Null;
        cleared["requires"] = serde_json::json!([]);
        let cleared: UpdateTask = serde_json::from_value(cleared).unwrap();
        assert_eq!(cleared.repo, Some(None));
        assert_eq!(cleared.weight, Some(None));
        assert_eq!(cleared.resource_units, Some(None));
        assert_eq!(cleared.affinity, Some(None));
        assert_eq!(cleared.preferred_runner, Some(None));
        assert_eq!(cleared.requires, Some(Vec::new()));
    }

    #[test]
    fn run_open_enums_state_machine_and_update_fields() {
        use crate::{EventType, RunEventKind, RunKind, RunOrigin, RunState};
        assert_eq!(
            serde_json::from_value::<RunKind>(serde_json::json!("quantum_audit")).unwrap(),
            RunKind::Unknown
        );
        assert_eq!(
            serde_json::from_value::<RunOrigin>(serde_json::json!("scheduled_cron")).unwrap(),
            RunOrigin::Unknown
        );
        assert_eq!(
            serde_json::from_value::<RunState>(serde_json::json!("migrating")).unwrap(),
            RunState::Unknown
        );
        assert_eq!(
            serde_json::from_value::<RunEventKind>(serde_json::json!("metric")).unwrap(),
            RunEventKind::Unknown
        );
        for (ev, name) in [
            (EventType::RunCreated, "run_created"),
            (EventType::RunUpdated, "run_updated"),
            (EventType::RunOrphaned, "run_orphaned"),
            (EventType::RunCancelled, "run_cancelled"),
        ] {
            assert_eq!(serde_json::to_value(ev).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<EventType>(serde_json::json!(name)).unwrap(),
                ev
            );
        }

        // State machine transition checks.
        assert!(RunState::Queued.can_transition_to(RunState::Leased));
        assert!(RunState::Queued.can_transition_to(RunState::Cancelled));
        assert!(!RunState::Queued.can_transition_to(RunState::Running));
        assert!(RunState::Leased.can_transition_to(RunState::Starting));
        assert!(RunState::Leased.can_transition_to(RunState::Running));
        assert!(RunState::Leased.can_transition_to(RunState::Orphaned));
        assert!(!RunState::Leased.can_transition_to(RunState::Succeeded));
        assert!(RunState::Starting.can_transition_to(RunState::Running));
        assert!(!RunState::Starting.can_transition_to(RunState::Succeeded));
        assert!(RunState::Running.can_transition_to(RunState::WaitingExternal));
        assert!(RunState::WaitingExternal.can_transition_to(RunState::Running));
        assert!(RunState::Running.can_transition_to(RunState::Succeeded));
        assert!(RunState::WaitingExternal.can_transition_to(RunState::Preempted));
        for terminal in [
            RunState::Succeeded,
            RunState::Failed,
            RunState::Preempted,
            RunState::Orphaned,
            RunState::Cancelled,
        ] {
            assert!(terminal.is_terminal());
            assert!(!terminal.is_active());
            assert!(!terminal.can_transition_to(RunState::Running));
            assert!(!terminal.can_transition_to(RunState::Cancelled));
        }

        // UpdateRun distinguishes absent from null.
        let absent: UpdateRun = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(absent.agent_id, None);
        assert_eq!(absent.worktree_branch, None);
        assert_eq!(absent.transcript_uri, None);
        assert_eq!(
            serde_json::to_value(&absent).unwrap(),
            serde_json::json!({})
        );

        let cleared: UpdateRun = serde_json::from_value(serde_json::json!({
            "agent_id": null,
            "worktree_branch": null,
            "transcript_uri": null,
            "outcome": null,
        }))
        .unwrap();
        assert_eq!(cleared.agent_id, Some(None));
        assert_eq!(cleared.worktree_branch, Some(None));
        assert_eq!(cleared.transcript_uri, Some(None));
        assert_eq!(cleared.outcome, Some(None));
    }

    #[test]
    fn dispatch_claim_and_open_enums_preserve_backward_compatibility() {
        use crate::{DispatchHoldReason, HousekeepingAction, ScoreBreakdown};

        // Minimal request defaults cleanly.
        let req: DispatchClaimRequest = serde_json::from_value(serde_json::json!({
            "device": "mac-pro",
            "free_units": 4
        }))
        .unwrap();
        assert_eq!(req.device.as_str(), "mac-pro");
        assert_eq!(req.free_units, 4);
        assert!(req.runners.is_empty());
        assert!(req.capabilities.is_empty());
        assert!(req.repos.is_empty());
        assert!(req.supported_kinds.is_empty());
        assert_eq!(req.quota_bucket, None);
        assert_eq!(req.max_claims, None);

        // Open enums deserialize future values as Unknown.
        assert_eq!(
            serde_json::from_value::<DispatchHoldReason>(serde_json::json!("future_reason"))
                .unwrap(),
            DispatchHoldReason::Unknown
        );
        assert_eq!(
            serde_json::from_value::<HousekeepingAction>(serde_json::json!({
                "action": "future_cleanup",
                "extra": 123
            }))
            .unwrap(),
            HousekeepingAction::Unknown
        );
        let gone: HousekeepingAction = serde_json::from_value(serde_json::json!({
            "action": "answer_gone_asker",
            "ask_id": 7,
            "task_id": "factory-1"
        }))
        .unwrap();
        assert_eq!(
            gone,
            HousekeepingAction::AnswerGoneAsker {
                ask_id: 7,
                task_id: "factory-1".parse().unwrap(),
                asker: None,
            }
        );

        let sb = ScoreBreakdown {
            kind: 100.0,
            priority: 30.0,
            rank: 9.0,
            age: 4.0,
            fair_share: -2.0,
            locality: 8.0,
            device_pref: 3.0,
            backfill: 6.0,
        };
        assert!((sb.total() - 158.0).abs() < 1e-9);
    }

    #[test]
    fn scheduler_policy_telemetry_and_replay_types_preserve_compatibility() {
        use crate::{
            EventType, PutSchedulerPolicy, ReplayPolicyConfig, SchedulerPolicy,
            SchedulerPolicyResponse, SchedulerWeights,
        };

        assert_eq!(
            serde_json::to_value(EventType::SchedulerPolicyUpdated).unwrap(),
            "scheduler_policy_updated"
        );
        let default_weights: SchedulerWeights =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(default_weights, SchedulerWeights::default());
        assert_eq!(default_weights.w_kind, 100.0);
        assert_eq!(default_weights.w_backfill, 6.0);

        let put: PutSchedulerPolicy =
            serde_json::from_value(serde_json::json!({ "version": "v2-fair" })).unwrap();
        assert_eq!(put.weights, SchedulerWeights::default());
        assert_eq!(put.lease_ttl_secs, None);
        assert_eq!(put.sticky_grace_secs, None);

        // SchedulerPolicyResponse flattens SchedulerPolicy cleanly so older clients reading
        // SchedulerPolicy work whether or not `history` is included.
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let resp = SchedulerPolicyResponse {
            policy: SchedulerPolicy {
                version: "v1-default".into(),
                weights: SchedulerWeights::default(),
                lease_ttl_secs: 180,
                sticky_grace_secs: 600,
                notes: Some("default".into()),
                active: true,
                created_by: Some("server".into()),
                created_at: now,
            },
            history: Vec::new(),
        };
        let val = serde_json::to_value(&resp).unwrap();
        assert!(val.get("history").is_none(), "{val}");
        let plain: SchedulerPolicy = serde_json::from_value(val).unwrap();
        assert_eq!(plain, resp.policy);

        let candidate: ReplayPolicyConfig =
            serde_json::from_value(serde_json::json!({ "weights": { "w_fair": 40.0 } })).unwrap();
        assert_eq!(candidate.version, "candidate");
        assert_eq!(candidate.weights.w_fair, 40.0);
        assert_eq!(candidate.weights.w_kind, 100.0);
    }

    #[test]
    fn live_run_event_and_control_wire_types_preserve_compatibility() {
        use crate::{EventType, RunEventKind};

        assert_eq!(
            serde_json::to_value(EventType::RunSteered).unwrap(),
            "run_steered"
        );
        assert_eq!(
            serde_json::from_value::<RunEventKind>(serde_json::json!("message")).unwrap(),
            RunEventKind::Message
        );
        assert_eq!(
            serde_json::from_value::<RunEventKind>(serde_json::json!("cancelled")).unwrap(),
            RunEventKind::Cancelled
        );
        assert_eq!(
            serde_json::from_value::<RunEventKind>(serde_json::json!("future_kind")).unwrap(),
            RunEventKind::Unknown
        );

        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let rec: crate::RunEventRecord = serde_json::from_value(serde_json::json!({
            "run_id": "run-abc",
            "seq": 3,
            "at": now,
            "kind": "tool",
            "summary": "cargo test",
            "meta_json": { "exit_code": 0 }
        }))
        .unwrap();
        assert_eq!(rec.seq, 3);
        assert_eq!(rec.kind, RunEventKind::Tool);
        assert_eq!(rec.meta, Some(serde_json::json!({ "exit_code": 0 })));
        let serialized = serde_json::to_value(&rec).unwrap();
        assert_eq!(serialized["meta"], serde_json::json!({ "exit_code": 0 }));

        let push: PushRunEvents = serde_json::from_value(serde_json::json!({
            "events": [{ "seq": 1, "kind": "phase", "summary": "starting" }]
        }))
        .unwrap();
        assert_eq!(push.events.len(), 1);
        assert_eq!(push.events[0].at, None);

        let cancel_empty: CancelRunRequest = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cancel_empty.reason, None);
        let cancel_resp = CancelRunResponse {
            run_id: "run-abc".parse().unwrap(),
            device: "mac".parse().unwrap(),
            state: RunState::Cancelled,
            wake_id: None,
        };
        assert!(
            serde_json::to_value(&cancel_resp)
                .unwrap()
                .get("wake_id")
                .is_none()
        );
        let steer_req = SteerRunRequest {
            message: "focus on tests".into(),
        };
        let steer_resp = SteerRunResponse {
            wake_id: 7,
            run_id: "run-abc".parse().unwrap(),
            device: "mac".parse().unwrap(),
        };
        assert_eq!(steer_req.message, "focus on tests");
        assert_eq!(steer_resp.wake_id, 7);
    }

    #[test]
    fn scheduler_autotune_wire_types_and_open_enums_preserve_compatibility() {
        assert_eq!(
            serde_json::from_value::<AutotuneMode>(serde_json::json!("shadow_future")).unwrap(),
            AutotuneMode::Unknown
        );
        assert_eq!(
            serde_json::from_value::<AutotuneMode>(serde_json::json!("apply")).unwrap(),
            AutotuneMode::Apply
        );
        assert_eq!(
            serde_json::from_value::<AutotuneDecision>(serde_json::json!("canary_paused")).unwrap(),
            AutotuneDecision::Unknown
        );
        assert_eq!(
            serde_json::from_value::<AutotuneDecision>(serde_json::json!(
                "skipped_insufficient_data"
            ))
            .unwrap(),
            AutotuneDecision::SkippedInsufficientData
        );

        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        // Older servers omit `latest_evaluation` on SchedulerStats; it reads as None and stays omitted.
        let old_stats: SchedulerStats = serde_json::from_value(serde_json::json!({
            "days": 7,
            "since": now,
            "runs_total": 0,
            "orphan_rate": 0.0,
            "ready_to_leased_secs": { "count": 0 },
            "leased_to_running_secs": { "count": 0 },
            "running_secs": { "count": 0 },
            "conflict_wait_secs": { "count": 0 },
            "total_weight_unit_minutes": 0.0
        }))
        .unwrap();
        assert_eq!(old_stats.latest_evaluation, None);
        assert!(
            serde_json::to_value(&old_stats)
                .unwrap()
                .get("latest_evaluation")
                .is_none()
        );

        let eval = SchedulerEvaluation {
            id: 1,
            evaluated_at: now,
            mode: AutotuneMode::Apply,
            decision: AutotuneDecision::Promoted,
            baseline_policy_version: "v1-default".into(),
            candidate_policy_version: Some("v2-auto-20261124".into()),
            applied_policy_version: Some("v2-auto-20261124".into()),
            previous_policy_version: Some("v1-default".into()),
            window_days: 7,
            sample_runs: 30,
            sample_tasks: 25,
            baseline_metrics: None,
            candidate_metrics: None,
            candidate_weights: Some(SchedulerWeights::default()),
            score_delta_pct: Some(12.5),
            reason: "promoted".into(),
            feedback_task_id: None,
        };
        let resp = SchedulerEvaluationsResponse {
            mode: AutotuneMode::Apply,
            enabled: true,
            interval_secs: 21_600,
            evaluations: vec![eval.clone()],
        };
        let round: SchedulerEvaluationsResponse =
            serde_json::from_value(serde_json::to_value(&resp).unwrap()).unwrap();
        assert_eq!(round, resp);
        assert_eq!(round.evaluations[0], eval);
    }

    #[test]
    fn lease_result_and_overlap_optional_fields_stay_v1_compatible() {
        // Legacy LeaseResult without `later_conflicts` and Lease without `created_at`.
        let legacy_lease_result: LeaseResult = serde_json::from_value(serde_json::json!({
            "leases": [{
                "id": 1,
                "agent": "t3:a",
                "task": null,
                "repo": "github.com/acme/app",
                "glob": "src/a.rs",
                "expires_at": "2026-11-24T13:00:00Z"
            }],
            "conflicts": []
        }))
        .unwrap();
        assert!(legacy_lease_result.later_conflicts.is_empty());
        assert_eq!(legacy_lease_result.leases[0].created_at, None);
        let serialized_lr = serde_json::to_value(&legacy_lease_result).unwrap();
        assert!(serialized_lr.get("later_conflicts").is_none());
        assert!(serialized_lr["leases"][0].get("created_at").is_none());

        // Legacy Overlap without `task` or `your_lease_earlier`.
        let legacy_overlap: Overlap = serde_json::from_value(serde_json::json!({
            "path": "src/a.rs",
            "agent": "t3:b",
            "handle": "t3:b",
            "via": "lease",
            "at": "2026-11-24T12:00:00Z"
        }))
        .unwrap();
        assert_eq!(legacy_overlap.task, None);
        assert_eq!(legacy_overlap.your_lease_earlier, None);
        let serialized_ov = serde_json::to_value(&legacy_overlap).unwrap();
        assert!(serialized_ov.get("task").is_none());
        assert!(serialized_ov.get("your_lease_earlier").is_none());

        // Populated fields round-trip cleanly.
        let full_overlap = Overlap {
            task: Some("acme-7".parse().unwrap()),
            your_lease_earlier: Some(true),
            ..legacy_overlap
        };
        let round: Overlap =
            serde_json::from_value(serde_json::to_value(&full_overlap).unwrap()).unwrap();
        assert_eq!(round, full_overlap);
    }

    #[test]
    fn goal_review_every_and_scheduler_token_stats_preserve_wire_compatibility() {
        // UpdateBoard: absent vs null vs value.
        let absent: UpdateBoard = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(absent.goal_review_every_secs, None);
        assert!(
            serde_json::to_value(&absent)
                .unwrap()
                .get("goal_review_every_secs")
                .is_none()
        );

        let cleared: UpdateBoard =
            serde_json::from_value(serde_json::json!({ "goal_review_every_secs": null })).unwrap();
        assert_eq!(cleared.goal_review_every_secs, Some(None));
        assert_eq!(
            serde_json::to_value(&cleared).unwrap()["goal_review_every_secs"],
            serde_json::Value::Null
        );

        let set: UpdateBoard =
            serde_json::from_value(serde_json::json!({ "goal_review_every_secs": 21_600 }))
                .unwrap();
        assert_eq!(set.goal_review_every_secs, Some(Some(21_600)));
        assert_eq!(
            serde_json::to_value(&set).unwrap()["goal_review_every_secs"],
            serde_json::json!(21_600)
        );

        // SchedulerStats: absent token fields from older servers default cleanly; populated ones round-trip.
        let now: Timestamp = "2026-11-25T12:00:00Z".parse().unwrap();
        let old: SchedulerStats = serde_json::from_value(serde_json::json!({
            "days": 7,
            "since": now,
            "runs_total": 1,
            "orphan_rate": 0.0,
            "ready_to_leased_secs": { "count": 0 },
            "leased_to_running_secs": { "count": 0 },
            "running_secs": { "count": 0 },
            "conflict_wait_secs": { "count": 0 },
            "total_weight_unit_minutes": 1.0
        }))
        .unwrap();
        assert_eq!(old.runs_with_tokens, 0);
        assert_eq!(old.tokens_in_total, None);
        assert_eq!(old.cached_tokens_total, None);
        assert_eq!(old.tokens_out_total, None);
        assert_eq!(old.reasoning_tokens_total, None);
        assert_eq!(old.token_cache_hit_rate, None);
        assert!(old.tokens_by_kind.is_empty());

        let mut with_tokens = old;
        with_tokens.runs_with_tokens = 2;
        with_tokens.tokens_in_total = Some(20_000);
        with_tokens.cached_tokens_total = Some(15_000);
        with_tokens.tokens_out_total = Some(1_200);
        with_tokens.reasoning_tokens_total = Some(300);
        with_tokens.token_cache_hit_rate = Some(0.75);
        with_tokens.tokens_by_kind.insert(
            "worker".into(),
            KindTokenStats {
                runs: 2,
                tokens_in: 20_000,
                cached_tokens: 15_000,
                tokens_out: 1_200,
                reasoning_tokens: 300,
                cache_hit_rate: Some(0.75),
            },
        );
        let round: SchedulerStats =
            serde_json::from_value(serde_json::to_value(&with_tokens).unwrap()).unwrap();
        assert_eq!(round, with_tokens);
    }

    #[test]
    fn extract_dropped_verdict_cuts_and_updates_backward_compat() {
        let note = "verdict: accept (p2, size s, auto=true).\n\
                    cut: split sub-part -> factory-42\n\
                    cut: dropped tasks audit -> factory-43\n\
                    cut: prefix cache key order, dropped: not checked\n\
                    - cut: secondary fan-out -> dropped (low volume)\n\
                    * cut: unicode arrow → dropped: stale\n\
                    • cut: em dash — dropped: out of scope";
        let cuts = extract_dropped_verdict_cuts(note);
        assert_eq!(
            cuts,
            vec![
                "cut: prefix cache key order, dropped: not checked".to_owned(),
                "cut: secondary fan-out -> dropped (low volume)".to_owned(),
                "cut: unicode arrow → dropped: stale".to_owned(),
                "cut: em dash — dropped: out of scope".to_owned(),
            ]
        );
        // Tentative verdicts do not report finalized dropped cuts.
        assert!(
            extract_dropped_verdict_cuts(
                "verdict: tentative cut\ncut: p1 finding, dropped: needs 2nd judge"
            )
            .is_empty()
        );
        // Split-only verdict notes and non-verdict notes yield nothing.
        assert!(extract_dropped_verdict_cuts("verdict: accept\ncut: a -> factory-9").is_empty());
        assert!(extract_dropped_verdict_cuts("note: cut: a, dropped: nope").is_empty());

        // Older servers omitting `dropped` and `cuts` still deserialize into `Updates`.
        let legacy: Updates = serde_json::from_value(serde_json::json!({
            "seen_at": null,
            "since": null,
            "done": [],
            "said": [],
            "more": false
        }))
        .unwrap();
        assert!(legacy.dropped.is_empty());
        assert!(legacy.cuts.is_empty());
    }
}
