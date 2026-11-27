use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{AgentId, BoardSlug, DeviceName, RunId, TaskId};

pub type Timestamp = DateTime<Utc>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardStatus {
    Active,
    Paused,
    Closed,
}

/// Whether a board holds project work or acts as a triage inbox (such as `board-feedback`).
/// Open enum: unknown values from a newer server deserialize as `Unknown`, which the server
/// refuses in requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardKind {
    #[default]
    Project,
    Inbox,
    #[serde(other)]
    Unknown,
}

/// Who originated a task: the user directly, a feedback report, or an agent.
/// Open enum for `/v1` forward compatibility.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOrigin {
    User,
    Feedback,
    #[default]
    Agent,
    #[serde(other)]
    Unknown,
}

/// Coarse task effort size used for WSJF economic scheduling among unranked tasks.
/// Open enum for `/v1` forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSize {
    Xs,
    S,
    M,
    L,
    #[serde(other)]
    Unknown,
}

impl TaskSize {
    /// WSJF denominator weight (`xs=1.0, s=1.5, m=3.0, l=6.0`).
    pub fn weight(self) -> f64 {
        match self {
            Self::Xs => 1.0,
            Self::S => 1.5,
            Self::M => 3.0,
            Self::L => 6.0,
            Self::Unknown => 3.0,
        }
    }
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
    /// When on, a device daemon works off the ready queue without the user.
    pub autopilot: bool,
    /// How many tasks the board's coordinator and its child threads may have in progress
    /// at once, 1 to [`PARALLEL_TASKS_MAX`]. Absent from servers that predate it, so it
    /// defaults to 1, one task at a time.
    #[serde(default = "one_parallel_task")]
    pub parallel_tasks: u32,
    /// When a timed change to `parallel_tasks` reverts to [`Board::parallel_tasks_prev`].
    /// Absent when permanent and from servers that predate timed limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tasks_until: Option<Timestamp>,
    /// The earlier `parallel_tasks` restored at [`Board::parallel_tasks_until`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tasks_prev: Option<u32>,
    /// The account usage, in percent of a limit window, at which the board starts no new
    /// task (see [`Usage::over`]), 1 to 100. Absent from servers that predate it, so it
    /// defaults to [`USAGE_STOP_AT_DEFAULT`].
    #[serde(default = "usage_stop_at_default")]
    pub usage_stop_at: u32,
    /// When a timed change to `usage_stop_at` reverts to [`Board::usage_stop_at_prev`].
    /// Absent when permanent and from servers that predate timed limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_stop_at_until: Option<Timestamp>,
    /// The earlier `usage_stop_at` restored at [`Board::usage_stop_at_until`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_stop_at_prev: Option<u32>,
    /// Whether a home device daemon starts automatic verdict judges for pending proposals on
    /// this board when autopilot is off (`docs/DESIGN.md`, "Daemon dispatch", Boards off
    /// autopilot). Absent from servers that predate it, so it defaults to `true`.
    #[serde(default = "verdict_judges_default")]
    pub verdict_judges: bool,
    /// Minimum interval in seconds between goal reviews on this board
    /// (`0` disables goal reviews; otherwise [`GOAL_REVIEW_EVERY_SECS_MIN`] to
    /// [`GOAL_REVIEW_EVERY_SECS_MAX`]), or `None` to allow a goal review as soon as work
    /// finishes. Absent when unset or from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_review_every_secs: Option<u64>,
    /// Whether this board is a `project` board or a triage `inbox` (such as `board-feedback`).
    /// Absent from servers that predate it, defaulting to [`BoardKind::Project`].
    #[serde(default)]
    pub kind: BoardKind,
    /// Optional cadence in seconds for periodic backlog curation (`RunKind::Curate`),
    /// `0` to disable or [`CURATE_EVERY_SECS_MIN`]..=[`CURATE_EVERY_SECS_MAX`]. Absent when
    /// unset or from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curate_every_secs: Option<u64>,
    /// The agent that holds the board's coordinator lease, if any: the dispatch agent of the
    /// device whose daemon runs the board.
    pub coordinator: Option<AgentId>,
    /// Who runs the board's autopilot. Every board of a current server is on
    /// [`Dispatch::Daemon`]; a server that predates the setting leaves it out, so it
    /// defaults to [`Dispatch::Thread`]. Daemons ignore it.
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
    /// A coordinator thread that lives for days. No current server or daemon runs a board
    /// this way; the value stays so that an older server's boards still parse.
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

fn verdict_judges_default() -> bool {
    true
}

/// Minimum allowed non-zero value for [`Board::goal_review_every_secs`] (1 minute).
pub const GOAL_REVIEW_EVERY_SECS_MIN: u64 = 60;

/// Maximum allowed value for [`Board::goal_review_every_secs`] (7 days).
pub const GOAL_REVIEW_EVERY_SECS_MAX: u64 = 604_800;

/// Minimum allowed non-zero value for [`Board::curate_every_secs`] (1 hour).
pub const CURATE_EVERY_SECS_MIN: u64 = 3_600;

/// Maximum allowed value for [`Board::curate_every_secs`] (30 days).
pub const CURATE_EVERY_SECS_MAX: u64 = 2_592_000;

/// Hard upper bound on the anti-starvation `ready_at` age tie-breaker weight/contribution so
/// age can never invert priority bands (`P1 > P2 > P3`) or explicit Frontier `rank`.
pub const W_AGE_MAX: f64 = 8.0;

/// Default effective goal-review interval on the server's feedback board (6 hours; superseded
/// on v2 inbox boards where `goal_review_every_secs = 0` disables goal reviews).
pub const FEEDBACK_GOAL_REVIEW_EVERY_SECS_DEFAULT: u64 = 21_600;

/// A reading older than this is stale: the device that read it may be asleep, so rolling
/// windows and projected burn rates no longer apply (a measured weekly pace breach can still
/// hold monotonically until the next pace day or weekly reset; see [`Usage::pace_limit`]).
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
    /// When a timed reserve change reverts to [`Usage::reserve_prev`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_until: Option<Timestamp>,
    /// The earlier reserve restored at [`Usage::reserve_until`] (`None` means off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_prev: Option<f64>,
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

    /// Whether a stale reading still holds by weekly monotonicity at `now`: weekly percent
    /// never decreases before `resets_at`, so once measured weekly usage has already reached
    /// the hard reserve (`100 - reserve`) or — within the same pace day (`now < next_day`) —
    /// the day's line, the hold stands without relying on stale `projected` burn rate.
    fn stale_pace_holds(&self, now: Timestamp) -> bool {
        let (Some(week), Some(pace)) = (self.seven_day, &self.pace) else {
            return false;
        };
        if !week.resets_at.is_some_and(|at| at > now) {
            return false;
        }
        week.percent >= pace.hard_line() || (now < pace.next_day() && week.percent >= pace.line)
    }

    /// The pace, if its week has not reset by `now` and either:
    /// - the reading is fresh and still within its pace day (`now < next_day`),
    /// - weekly usage already reached the hard reserve (`100 - reserve`, holds until `resets_at`
    ///   for both fresh and stale readings), or
    /// - the reading is stale within the same pace day and measured weekly usage reached the
    ///   day's line.
    pub fn pace_at(&self, now: Timestamp) -> Option<&UsagePace> {
        let week = self.seven_day?;
        let pace = self.pace.as_ref()?;
        if !week.resets_at.is_none_or(|at| at > now) {
            return None;
        }
        if self.fresh(now) {
            if now < pace.next_day()
                || pace.hard_since.is_some()
                || week.percent >= pace.hard_line()
            {
                Some(pace)
            } else {
                None
            }
        } else {
            self.stale_pace_holds(now).then_some(pace)
        }
    }

    /// How many tasks a board with `parallel_tasks` may run at once at `now`
    /// ([`UsagePace::limit`]); `parallel_tasks` without a pace. Past `pace.next_day()`, a
    /// previous day's soft line no longer holds (even while the reading is still fresh);
    /// only a hard-reserve breach keeps holding until `resets_at`. When the reading is stale,
    /// holds (`0`) only on a measured breach of the hard reserve or of the same pace day's
    /// line, never on a stale projected burn rate alone.
    pub fn pace_limit(&self, parallel_tasks: u32, now: Timestamp) -> u32 {
        if self.fresh(now) {
            self.pace_at(now).map_or(parallel_tasks, |p| {
                if now >= p.next_day() {
                    0
                } else {
                    p.limit(parallel_tasks)
                }
            })
        } else if self.stale_pace_holds(now) {
            0
        } else {
            parallel_tasks
        }
    }

    /// Since when the measured weekly use is at the hard line, if it is at `now`.
    pub fn hard_since(&self, now: Timestamp) -> Option<Timestamp> {
        let pace = self.pace_at(now)?;
        if self.fresh(now) {
            pace.hard_since
        } else {
            let week = self.seven_day?;
            (week.percent >= pace.hard_line())
                .then_some(pace.hard_since)
                .flatten()
        }
    }

    /// Whether weekly usage has reached the reserve's hard line (`100 - reserve`) at `now`.
    pub fn hard_line_reached(&self, now: Timestamp) -> bool {
        if self.hard_since(now).is_some() {
            return true;
        }
        match (self.pace_at(now), self.seven_day) {
            (Some(p), Some(week)) => week.percent >= p.hard_line(),
            _ => false,
        }
    }

    /// Whether a daemon should read the usage every minute: the pace is near a line.
    pub fn near_line(&self, now: Timestamp) -> bool {
        match (self.pace_at(now), self.seven_day) {
            (Some(p), Some(week)) => p.near(week.percent),
            _ => false,
        }
    }
}

/// One per-(provider, account) quota bucket (`docs/API.md`, "Usage").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageBucket {
    pub provider: String,
    pub account: String,
    /// The rolling 5-hour window; absent when no reading or the endpoint gave none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<UsageWindow>,
    /// The weekly window; absent when no reading or the endpoint gave none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<UsageWindow>,
    /// When a device last read this bucket's usage; absent when only a reserve is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at: Option<Timestamp>,
    /// The device that reported the latest reading; absent before any reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceName>,
    /// Weekly percent kept for the user's own use; absent while pace is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve: Option<f64>,
    /// The weekly pace computed while `reserve` is set and a weekly window has `resets_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pace: Option<UsagePace>,
    /// When a timed reserve change reverts to [`UsageBucket::reserve_prev`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_until: Option<Timestamp>,
    /// The earlier reserve restored at [`UsageBucket::reserve_until`] (`None` means off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_prev: Option<f64>,
}

impl UsageBucket {
    /// Converts this bucket's latest reading into the legacy [`Usage`] shape when a reading
    /// has been reported.
    pub fn as_usage(&self) -> Option<Usage> {
        Some(Usage {
            five_hour: self.five_hour,
            seven_day: self.seven_day,
            read_at: self.read_at?,
            device: self.device.clone()?,
            pace: self.pace,
            reserve_until: self.reserve_until,
            reserve_prev: self.reserve_prev,
        })
    }
}

/// Which fleet plane a device belongs to (`docs/SCHEDULER_DESIGN.md`, "Strict 3-Plane Fleet
/// Isolation"). Open enum: unknown values from a newer server deserialize as `Unknown`,
/// which the server refuses in requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRole {
    Control,
    #[default]
    Runner,
    ServiceHost,
    #[serde(other)]
    Unknown,
}

/// Live connectivity state of a device computed from its `heartbeat_at` against the server
/// clock. Open enum for forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceStatus {
    Online,
    Stale,
    Offline,
    #[serde(other)]
    Unknown,
}

/// Coarse resource tier for a task's execution weight (`docs/SCHEDULER_DESIGN.md`). Open
/// enum: unknown values deserialize as `Unknown`, which the server refuses in requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceWeight {
    Light,
    Standard,
    Heavy,
    #[serde(other)]
    Unknown,
}

impl ResourceWeight {
    /// Default weighted capacity units consumed by a run of this weight.
    pub fn default_units(self) -> Option<u32> {
        match self {
            Self::Light => Some(1),
            Self::Standard => Some(2),
            Self::Heavy => Some(4),
            Self::Unknown => None,
        }
    }
}

/// Enforcement effect of a device taint (`docs/SCHEDULER_DESIGN.md`). Open enum: unknown
/// values deserialize as `Unknown`, which the server refuses in requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaintEffect {
    NoSchedule,
    PreferNoSchedule,
    #[serde(other)]
    Unknown,
}

/// A Kubernetes-style taint attached by the user/admin to a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Taint {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub effect: TaintEffect,
}

/// A task's explicit opt-in to run on devices carrying a matching [`Taint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaintToleration {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<TaintEffect>,
}

/// Hardware and weighted scheduling capacity reported by a device heartbeat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapacity {
    pub weight_units: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_gib: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
}

/// A fleet-wide semaphore acquired atomically when a run for a task is leased.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictKeySpec {
    pub key: String,
    #[serde(default = "default_conflict_max")]
    pub max: u32,
}

fn default_conflict_max() -> u32 {
    1
}

/// Soft locality hint for scheduling a task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAffinity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer_device: Option<DeviceName>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prefer_warm_repo: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preferred_devices: Vec<DeviceName>,
}

/// A non-revoked device in the fleet registry (`GET /v1/devices`,
/// `PUT /v1/devices/{name}/heartbeat`, `PATCH /v1/admin/devices/{name}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub name: DeviceName,
    #[serde(default)]
    pub role: DeviceRole,
    pub status: DeviceStatus,
    #[serde(default)]
    pub drain: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<DeviceCapacity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_units: Option<u32>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub runners: Vec<String>,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub taints: Vec<Taint>,
    #[serde(default)]
    pub preference: i32,
    #[serde(default)]
    pub protocol_features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<Timestamp>,
}

/// What kind of execution a [`Run`] performs (`docs/SCHEDULER_DESIGN.md`, Section 3.3).
/// Open enum: unknown values from a newer server deserialize as `Unknown`, which the server
/// refuses in requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Worker,
    Review,
    Verdict,
    Triage,
    Goal,
    Curate,
    Merge,
    Housekeeping,
    Command,
    Interactive,
    #[serde(other)]
    Unknown,
}

impl RunKind {
    /// Whether a live managed run of this kind grants its registered `agent_id` coordinator
    /// rights on the run's board without requiring a per-board coordinator lease.
    pub fn grants_coordinator_rights(self) -> bool {
        matches!(
            self,
            Self::Review
                | Self::Verdict
                | Self::Triage
                | Self::Goal
                | Self::Curate
                | Self::Merge
                | Self::Housekeeping
        )
    }
}

/// Whether a [`Run`] was dispatched by the scheduler (`managed`) or started manually by
/// the user (`interactive`). Open enum for forward compatibility.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOrigin {
    #[default]
    Managed,
    Interactive,
    #[serde(other)]
    Unknown,
}

/// Lifecycle state of a [`Run`] (`docs/SCHEDULER_DESIGN.md`, Section 3.3). Open enum:
/// unknown values deserialize as `Unknown`, which the server refuses in requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Leased,
    Starting,
    Running,
    WaitingExternal,
    Succeeded,
    Failed,
    Preempted,
    Orphaned,
    Cancelled,
    #[serde(other)]
    Unknown,
}

impl RunState {
    /// Whether this state is terminal (no further transitions are allowed and any held
    /// conflict locks are released).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Preempted | Self::Orphaned | Self::Cancelled
        )
    }

    /// Whether this state is non-terminal (`queued`, `leased`, `starting`, `running`,
    /// `waiting_external`).
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Leased | Self::Starting | Self::Running | Self::WaitingExternal
        )
    }

    /// Whether a run in this state holds an active execution lease (`leased`, `starting`,
    /// `running`, `waiting_external`).
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Leased | Self::Starting | Self::Running | Self::WaitingExternal
        )
    }

    /// Whether a transition from `self` to `next` is valid under the Run state machine.
    pub fn can_transition_to(self, next: Self) -> bool {
        use RunState::*;
        matches!(
            (self, next),
            (Queued, Leased | Cancelled)
                | (Leased, Starting | Running | Failed | Orphaned | Cancelled)
                | (Starting, Running | Failed | Orphaned | Cancelled)
                | (
                    Running,
                    WaitingExternal | Succeeded | Failed | Preempted | Orphaned | Cancelled
                )
                | (
                    WaitingExternal,
                    Running | Succeeded | Failed | Preempted | Orphaned | Cancelled
                )
        )
    }
}

/// Kind of structured live event attached to a [`Run`] (`run_events`). Open enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEventKind {
    Phase,
    Tool,
    Note,
    Message,
    Pr,
    Error,
    Cancelled,
    Done,
    #[serde(other)]
    Unknown,
}

/// One compact structured live event stored in `run_events` (`POST/GET /v1/runs/{id}/events`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventRecord {
    pub run_id: RunId,
    pub seq: i64,
    pub at: Timestamp,
    pub kind: RunEventKind,
    /// Single-line summary capped at [`crate::RUN_EVENT_SUMMARY_CHARS_MAX`] (300) characters.
    pub summary: String,
    #[serde(default, alias = "meta_json", skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

/// One fleet-wide concurrency lock currently held by an active [`Run`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictLock {
    pub key: String,
    pub run_id: RunId,
    pub max_holders: u32,
    pub acquired_at: Timestamp,
}

/// A queued wake-up message routed to the device hosting an active session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StickyWake {
    pub id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    pub device: DeviceName,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
}

/// Target `(provider, account)` LLM quota bucket checked during `POST /v1/dispatch/claim`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaBucketRef {
    pub provider: String,
    pub account: String,
}

/// Component breakdown of the multifactor score that ranked a claimed run
/// (`docs/SCHEDULER_DESIGN.md`, Section 3 Layer 2 & Section 6.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    #[serde(default)]
    pub kind: f64,
    #[serde(default)]
    pub priority: f64,
    #[serde(default)]
    pub rank: f64,
    #[serde(default)]
    pub age: f64,
    #[serde(default)]
    pub fair_share: f64,
    #[serde(default)]
    pub locality: f64,
    #[serde(default)]
    pub device_pref: f64,
    #[serde(default)]
    pub backfill: f64,
}

impl ScoreBreakdown {
    /// Total multifactor score across all eight components.
    pub fn total(self) -> f64 {
        self.kind
            + self.priority
            + self.rank
            + self.age
            + self.fair_share
            + self.locality
            + self.device_pref
            + self.backfill
    }
}

/// Deterministic housekeeping step claimed via `POST /v1/dispatch/claim` with
/// `kind = housekeeping`. Open tagged enum: future actions deserialize as `Unknown`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HousekeepingAction {
    /// An answered ask whose asking agent is gone/unreachable: record the answer as a
    /// decision note on `task_id` and release the task back to `open`.
    AnswerGoneAsker {
        ask_id: i64,
        task_id: TaskId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asker: Option<AgentId>,
    },
    /// A task in `doing` whose owner stayed away past the repeat `idle_doing` notice +
    /// grace: note its old thread/branch and release the task back to `open`.
    ReleaseAwayOwner {
        task_id: TaskId,
        owner: AgentId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        away_since: Option<Timestamp>,
    },
    #[serde(other)]
    Unknown,
}

/// Why one or more candidates were skipped during `POST /v1/dispatch/claim`.
/// Open enum for forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchHoldReason {
    Drain,
    Quota,
    ConflictKey,
    LeaseOverlap,
    Capacity,
    Taint,
    Requires,
    ParallelTasks,
    JudgeCap,
    Runner,
    Repo,
    #[serde(other)]
    Unknown,
}

/// Compact telemetry summarizing why top candidates were skipped during
/// `POST /v1/dispatch/claim`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchHoldSummary {
    pub reason: DispatchHoldReason,
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn default_w_kind() -> f64 {
    100.0
}
fn default_w_prio() -> f64 {
    30.0
}
fn default_w_rank() -> f64 {
    10.0
}
fn default_w_age() -> f64 {
    W_AGE_MAX
}
fn default_w_fair() -> f64 {
    12.0
}
fn default_w_loc() -> f64 {
    8.0
}
fn default_w_pref() -> f64 {
    1.0
}
fn default_w_backfill() -> f64 {
    6.0
}

/// Declarative multifactor scoring weights (`docs/SCHEDULER_DESIGN.md`, Section 3 & 6.4).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SchedulerWeights {
    #[serde(default = "default_w_kind")]
    pub w_kind: f64,
    #[serde(default = "default_w_prio")]
    pub w_prio: f64,
    #[serde(default = "default_w_rank")]
    pub w_rank: f64,
    #[serde(default = "default_w_age")]
    pub w_age: f64,
    #[serde(default = "default_w_fair")]
    pub w_fair: f64,
    #[serde(default = "default_w_loc")]
    pub w_loc: f64,
    #[serde(default = "default_w_pref")]
    pub w_pref: f64,
    #[serde(default = "default_w_backfill")]
    pub w_backfill: f64,
}

impl Default for SchedulerWeights {
    fn default() -> Self {
        Self {
            w_kind: default_w_kind(),
            w_prio: default_w_prio(),
            w_rank: default_w_rank(),
            w_age: default_w_age(),
            w_fair: default_w_fair(),
            w_loc: default_w_loc(),
            w_pref: default_w_pref(),
            w_backfill: default_w_backfill(),
        }
    }
}

/// A versioned declarative scheduler policy snapshot (`GET/PUT /v1/scheduler/policy`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerPolicy {
    pub version: String,
    pub weights: SchedulerWeights,
    pub lease_ttl_secs: u64,
    pub sticky_grace_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at: Timestamp,
}

/// Operating mode of the automated scheduler evaluation and self-tuning loop
/// (`docs/SCHEDULER_DESIGN.md`, Section 6.4). Open enum: unknown values from a newer
/// server deserialize as `Unknown`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutotuneMode {
    Off,
    Propose,
    #[default]
    Apply,
    #[serde(other)]
    Unknown,
}

/// Decision recorded by one automated scheduler evaluation cycle
/// (`docs/SCHEDULER_DESIGN.md`, Section 6.4). Open enum: unknown values from a newer
/// server deserialize as `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutotuneDecision {
    Keep,
    Promoted,
    Proposed,
    RolledBack,
    SkippedInsufficientData,
    #[serde(other)]
    Unknown,
}

/// Default weighted capacity units consumed by a [`Run`].
pub fn default_run_cost_units() -> u32 {
    1
}

/// One concrete execution attempt on a fleet device (`GET /v1/runs`, `POST /v1/runs`,
/// `GET /v1/runs/{id}`, `PATCH /v1/runs/{id}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
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
    pub state: RunState,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_sha256: Option<String>,
    /// Conflict locks currently held by this run; empty once terminal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_locks: Vec<ConflictLock>,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
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
    /// The task's place inside its priority level: lower first, and a task without one
    /// after every ranked task, by number. A changed priority clears it. Absent from older
    /// servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
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
    /// The task is held by blockers that are all closed (done or dropped): it is `blocked`,
    /// or `open` with a dropped blocker, which never counts as done. The board moves
    /// nothing: an agent or the user clears the blockers or drops the task. Derived when
    /// read; absent from older servers, read as false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blockers_closed: bool,
    /// A pending proposal ([`is_pending_proposal`]): agents review it before it is work.
    /// Derived when read; absent from older servers, read as false.
    #[serde(default)]
    pub proposal: bool,
    /// The task is not ready, and cannot be claimed, before this time. Once it passes, the
    /// server announces it with a `task_updated` event (actor `server`, no field changes),
    /// which has the daemon dispatch it; the field itself stays.
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
    /// Normalized repository key (`host/owner/repo`) this task targets; overrides
    /// `board.repos` when set. Absent when unset, and from servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Coarse resource weight tier (`light`, `standard`, `heavy`). Absent when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<ResourceWeight>,
    /// Explicit weighted capacity units overriding [`Task::weight`]. Absent when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_units: Option<u32>,
    /// Hard capability predicates a runner device must advertise (e.g. `os:macos`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Explicit opt-ins allowing this task to run on tainted devices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<TaintToleration>,
    /// Fleet-wide concurrency keys acquired atomically while a run executes this task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_keys: Vec<ConflictKeySpec>,
    /// Soft locality hint preferring a device or warm repository checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<TaskAffinity>,
    /// Optional preferred runner kind (e.g. `acp`, `t3`, `deterministic`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_runner: Option<String>,
    /// Who originated this task (`user`, `feedback`, or `agent`). Absent from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<TaskOrigin>,
    /// Coarse task effort size (`xs`, `s`, `m`, `l`) for WSJF economic scheduling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<TaskSize>,
    /// When this open task was cooled to the Icebox; absent when active or from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooled_at: Option<Timestamp>,
    /// When this task last became ready (used for bounded `ready_at` age tie-breaking).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<Timestamp>,
    /// Server-computed count of linked feedback reports (including duplicates and recurrences).
    /// Absent when zero or from older servers.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub report_count: u32,
    /// Server-computed effective priority with blocker Priority Inheritance
    /// (`min(self.priority, min_dependent_priority)` where `p1 < p2 < p3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_priority: Option<Priority>,
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
    Antigravity,
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
    /// At most 1,500 characters by default; untrusted for readers.
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
    /// When the lease was first taken; kept across live renewals and absent from servers
    /// that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<Timestamp>,
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
    /// The board change the ask carries, which the server applies when the user answers
    /// with the ask's first option. Absent without one, and from servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<BoardChange>,
}

/// The board settings only the user may raise or turn on, as a change the user approves:
/// attached to an ask ([`Ask::change`]) or sent with the user's words
/// (`POST /v1/boards/{slug}/approved`). At least one field is set. Fields it does not name
/// are refused, so no other board setting passes through an approval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardChange {
    /// 1 to [`PARALLEL_TASKS_MAX`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tasks: Option<u32>,
    /// 1 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_stop_at: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporary: Option<bool>,
    /// Optional expiry time for `parallel_tasks` and/or `usage_stop_at`: the server
    /// stores the earlier value and restores it at `until`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
}

impl BoardChange {
    pub fn is_empty(&self) -> bool {
        self.parallel_tasks.is_none() && self.usage_stop_at.is_none() && self.temporary.is_none()
    }

    /// The settings as `name=value`, in a fixed order, separated by `, `, with ` until <time>`
    /// when timed.
    pub fn settings(&self) -> String {
        let mut parts = Vec::new();
        if let Some(n) = self.parallel_tasks {
            parts.push(format!("parallel_tasks={n}"));
        }
        if let Some(n) = self.usage_stop_at {
            parts.push(format!("usage_stop_at={n}"));
        }
        if let Some(on) = self.temporary {
            parts.push(format!("temporary={on}"));
        }
        let mut out = parts.join(", ");
        if let Some(at) = self.until {
            out.push_str(&format!(" until {}", at.format("%Y-%m-%d %H:%MZ")));
        }
        out
    }

    /// The option the server puts first on an ask that carries this change. Answering
    /// with exactly this text applies the change.
    pub fn approving_option(&self) -> String {
        format!("Apply: {}", self.settings())
    }
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
    /// Admin changed a device's plane metadata (`role`, `taints`, `preference`, `drain`).
    DeviceUpdated,
    /// A new run was created or submitted (`POST /v1/runs`).
    RunCreated,
    /// A run's state or metadata changed (`PATCH /v1/runs/{id}`).
    RunUpdated,
    /// A run's lease expired or it was marked `orphaned` (Trail Rule record).
    RunOrphaned,
    /// A run was cancelled (`PATCH /v1/runs/{id}` to `cancelled` or `POST /v1/runs/{id}/cancel`).
    RunCancelled,
    /// A steering message was enqueued for an active run (`POST /v1/runs/{id}/steer`).
    RunSteered,
    /// A new versioned scheduler policy was activated (`PUT /v1/scheduler/policy`).
    SchedulerPolicyUpdated,
    /// The account or bucket weekly usage reserve changed or expired back to its earlier value.
    UsageReserveUpdated,
    /// A host's reported health posture, alert count, or service/version state changed.
    HostHealthUpdated,
    /// A host's scheduled jobs changed state, recorded a new run, or added/removed jobs.
    SchedulesUpdated,
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
        // An older server sends no `blockers_closed`; false is left out when written.
        assert!(!read.blockers_closed);
        assert!(
            serde_json::to_value(&read)
                .unwrap()
                .get("blockers_closed")
                .is_none()
        );
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

    #[test]
    fn stale_weekly_pace_holds_only_on_measured_breach_until_next_day_or_reset() {
        let read_at: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let day_start = read_at - chrono::TimeDelta::hours(2);
        let next_day = day_start + chrono::TimeDelta::hours(24);
        let resets_at = day_start + chrono::TimeDelta::days(6);
        let stale_same_day = read_at + chrono::TimeDelta::minutes(USAGE_FRESH_MINUTES + 15);
        let stale_next_day = next_day;
        let stale_after_next_day = next_day + chrono::TimeDelta::hours(2);

        let make_usage =
            |measured: f64, projected: f64, line: f64, reserve: f64, hard: bool| Usage {
                five_hour: Some(UsageWindow {
                    percent: 10.0,
                    resets_at: Some(read_at + chrono::TimeDelta::hours(4)),
                }),
                seven_day: Some(UsageWindow {
                    percent: measured,
                    resets_at: Some(resets_at),
                }),
                read_at,
                device: "air".parse().unwrap(),
                pace: Some(UsagePace {
                    reserve,
                    day_start,
                    anchor: 0.0,
                    line,
                    projected,
                    hard_since: hard.then_some(read_at),
                }),
                reserve_until: None,
                reserve_prev: None,
            };

        // 1. Stale (>30m) measured seven_day.percent >= pace.line within the same pace day holds (0).
        let breached_day = make_usage(32.6, 32.6, 16.0, 4.0, false);
        assert!(!breached_day.fresh(stale_same_day));
        assert_eq!(breached_day.pace_limit(3, stale_same_day), 0);
        assert!(breached_day.pace_at(stale_same_day).is_some());
        assert_eq!(breached_day.hard_since(stale_same_day), None);
        assert!(!breached_day.near_line(stale_same_day));

        // 2. Next pace day starts (now >= day_start + 24h): same-day line hold releases.
        assert_eq!(breached_day.pace_limit(3, stale_next_day), 3);
        assert!(breached_day.pace_at(stale_next_day).is_none());

        // 3. Stale (>30m) measured < pace.line but stale projected >= pace.line must NOT hold.
        let projected_only = make_usage(14.0, 18.0, 16.0, 4.0, false);
        assert!(!projected_only.fresh(stale_same_day));
        assert_eq!(projected_only.pace_limit(3, stale_same_day), 3);
        assert!(projected_only.pace_at(stale_same_day).is_none());

        // 4. Stale (>30m) measured >= 100 - reserve holds across pace days until resets_at, then releases.
        let breached_hard = make_usage(96.0, 96.0, 80.0, 4.0, true);
        assert_eq!(breached_hard.pace_limit(3, stale_same_day), 0);
        assert_eq!(breached_hard.hard_since(stale_same_day), Some(read_at));
        assert_eq!(breached_hard.pace_limit(3, stale_after_next_day), 0);
        assert_eq!(
            breached_hard.hard_since(stale_after_next_day),
            Some(read_at)
        );
        assert_eq!(breached_hard.pace_limit(3, resets_at), 3);
        assert_eq!(breached_hard.hard_since(resets_at), None);
        assert!(breached_hard.pace_at(resets_at).is_none());

        // 5. Stale reading with no resets_at never holds indefinitely.
        let mut no_reset = breached_hard;
        no_reset.seven_day.as_mut().unwrap().resets_at = None;
        assert_eq!(no_reset.pace_limit(3, stale_same_day), 3);
        assert_eq!(no_reset.hard_since(stale_same_day), None);
    }

    #[test]
    fn fresh_weekly_pace_lifts_at_next_day_unless_hard_line_reached() {
        let day_start: Timestamp = "2026-11-26T07:04:00Z".parse().unwrap();
        let next_day = day_start + chrono::TimeDelta::hours(24);
        let resets_at = day_start + chrono::TimeDelta::days(7);
        // Reading taken 2 minutes before next_day, evaluated 1 minute after next_day (still fresh!).
        let read_at = next_day - chrono::TimeDelta::minutes(2);
        let after_boundary = next_day + chrono::TimeDelta::minutes(1);

        let make = |measured: f64, projected: f64, hard: bool| Usage {
            five_hour: Some(UsageWindow {
                percent: 2.0,
                resets_at: Some(read_at + chrono::TimeDelta::hours(3)),
            }),
            seven_day: Some(UsageWindow {
                percent: measured,
                resets_at: Some(resets_at),
            }),
            read_at,
            device: "pro".parse().unwrap(),
            pace: Some(UsagePace {
                reserve: 5.0,
                day_start,
                anchor: 0.4,
                line: 13.9,
                projected,
                hard_since: hard.then_some(read_at),
            }),
            reserve_until: None,
            reserve_prev: None,
        };

        // 1. Soft daily line breach holds before next_day, and lifts immediately once next_day
        //    arrives even though the reading is still fresh (3m old).
        let soft = make(23.4, 23.4, false);
        assert!(soft.fresh(after_boundary));
        assert_eq!(soft.pace_limit(2, read_at), 0);
        assert_eq!(soft.pace_limit(2, next_day), 2);
        assert_eq!(soft.pace_limit(2, after_boundary), 2);
        assert!(soft.pace_at(after_boundary).is_none());
        assert!(!soft.hard_line_reached(after_boundary));

        // 2. Hard reserve breach (>= 95.0%) keeps holding across next_day even on a fresh reading,
        //    and releases once resets_at passes.
        let hard = make(95.5, 95.5, true);
        assert!(hard.fresh(after_boundary));
        assert_eq!(hard.pace_limit(2, after_boundary), 0);
        assert!(hard.pace_at(after_boundary).is_some());
        assert_eq!(hard.hard_since(after_boundary), Some(read_at));
        assert!(hard.hard_line_reached(after_boundary));
        assert_eq!(hard.pace_limit(2, resets_at), 2);
        assert!(hard.pace_at(resets_at).is_none());
        assert!(!hard.hard_line_reached(resets_at));
    }

    #[test]
    fn timed_board_change_and_wire_defaults_stay_backward_compatible() {
        let until: Timestamp = "2026-11-22T01:02:00Z".parse().unwrap();
        let empty_until = BoardChange {
            until: Some(until),
            ..Default::default()
        };
        assert!(empty_until.is_empty());

        let timed = BoardChange {
            parallel_tasks: Some(3),
            usage_stop_at: Some(99),
            temporary: None,
            until: Some(until),
        };
        assert!(!timed.is_empty());
        assert_eq!(
            timed.settings(),
            "parallel_tasks=3, usage_stop_at=99 until 2026-11-22 01:02Z"
        );
        assert_eq!(
            timed.approving_option(),
            "Apply: parallel_tasks=3, usage_stop_at=99 until 2026-11-22 01:02Z"
        );

        // Older server payloads without timed fields deserialize cleanly and omit them on write.
        let legacy_reserve: crate::UsageReserve =
            serde_json::from_value(serde_json::json!({ "reserve": 5.5 })).unwrap();
        assert_eq!(legacy_reserve.until, None);
        assert_eq!(legacy_reserve.previous, None);
        assert_eq!(
            serde_json::to_value(legacy_reserve).unwrap(),
            serde_json::json!({ "reserve": 5.5 })
        );
    }

    #[test]
    fn lease_created_at_is_optional_and_omitted_when_none() {
        let legacy: Lease = serde_json::from_value(serde_json::json!({
            "id": 42,
            "agent": "t3:alpha",
            "task": "acme-1",
            "repo": "github.com/acme/app",
            "glob": "src/lib.rs",
            "expires_at": "2026-11-24T13:00:00Z"
        }))
        .unwrap();
        assert_eq!(legacy.created_at, None);
        assert!(
            serde_json::to_value(&legacy)
                .unwrap()
                .get("created_at")
                .is_none()
        );

        let dated = Lease {
            created_at: Some("2026-11-24T12:15:00Z".parse().unwrap()),
            ..legacy
        };
        let round: Lease = serde_json::from_value(serde_json::to_value(&dated).unwrap()).unwrap();
        assert_eq!(round, dated);
    }
}
