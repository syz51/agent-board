//! Wire types for host health telemetry, token/cache analytics, and cross-machine
//! scheduled jobs (`/v1/hosts/{host}/health`, `/v1/hosts/{host}/schedules`,
//! `/v1/health/overview`, `/v1/health/refresh`, `/v1/schedules`).
//!
//! All types are generic and additive within `/v1`: extensible enums carry `#[serde(other)]`
//! `Unknown` so older clients decode newer values safely.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    BoardSlug, DeviceEntry, DeviceName, DeviceRole, DeviceStatus, TaskId, Timestamp, UsageBucket,
};

/// Default TTL in seconds for a host health snapshot before the host counts as stale.
pub const HOST_HEALTH_TTL_SECS_DEFAULT: u64 = 300;
/// Minimum allowed TTL in seconds for a host health snapshot.
pub const HOST_HEALTH_TTL_SECS_MIN: u64 = 30;
/// Maximum allowed TTL in seconds for a host health snapshot (24 hours).
pub const HOST_HEALTH_TTL_SECS_MAX: u64 = 86_400;
/// Retention window in days for hourly `host_health_samples`.
pub const HOST_HEALTH_SAMPLES_RETENTION_DAYS: i64 = 30;
/// Maximum UTF-8 byte size for `ScheduledJobReport::last_error`.
pub const SCHEDULE_ERROR_BYTES_MAX: usize = 2_048;
/// Maximum characters for `ScheduledJobReport::command_preview`.
pub const SCHEDULE_COMMAND_CHARS_MAX: usize = 500;
/// Maximum characters for `ScheduledJobReport::deep_link`.
pub const SCHEDULE_DEEP_LINK_CHARS_MAX: usize = 500;
/// Maximum ring-buffered history entries kept per `job_key`.
pub const SCHEDULE_HISTORY_CAP: usize = 30;
/// Maximum jobs accepted in a single `POST /v1/hosts/{host}/schedules` batch.
pub const SCHEDULE_BATCH_MAX: usize = 500;

/// Overall fleet health posture computed by `GET /v1/health/overview`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthPosture {
    #[default]
    Healthy,
    Degraded,
    Incident,
    #[serde(other)]
    Unknown,
}

/// External alert severity (e.g. Alertmanager / monitoring bridge).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    #[default]
    Info,
    Warning,
    Critical,
    #[serde(other)]
    Unknown,
}

/// Long-running service / daemon state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Running,
    Stopped,
    Failed,
    Degraded,
    #[default]
    #[serde(other)]
    Unknown,
}

/// Backup freshness/outcome state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupState {
    Ok,
    Stale,
    Failed,
    #[default]
    #[serde(other)]
    Unknown,
}

/// Scheduler source for a normalized scheduled job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleSource {
    Launchd,
    Systemd,
    T3,
    Cron,
    Github,
    Board,
    Other,
    #[serde(other)]
    Unknown,
}

impl ScheduleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launchd => "launchd",
            Self::Systemd => "systemd",
            Self::T3 => "t3",
            Self::Cron => "cron",
            Self::Github => "github",
            Self::Board => "board",
            Self::Other => "other",
            Self::Unknown => "unknown",
        }
    }
}

/// Normalized category tag for a scheduled job.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleCategory {
    Backup,
    Monitoring,
    AgentOps,
    Sync,
    Security,
    Maintenance,
    Research,
    App,
    Os,
    #[default]
    Other,
    #[serde(other)]
    Unknown,
}

/// Normalized live state of a scheduled job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleState {
    Ok,
    Running,
    Failing,
    Overdue,
    NeverRun,
    Disabled,
    #[serde(other)]
    Unknown,
}

fn default_collector() -> String {
    "host".to_owned()
}

fn default_ttl_secs() -> u64 {
    HOST_HEALTH_TTL_SECS_DEFAULT
}

fn default_true() -> bool {
    true
}

/// Host CPU, memory, and root disk utilization reported by a host collector.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_1m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_15m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_used_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_gib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_days_to_full: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice_cpu_cap_pct: Option<f64>,
}

/// Tailscale and external heartbeat connectivity reported by a host collector.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostNetworkStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale_online: Option<bool>,
    /// `"direct"` or `"relay"` (DERP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale_latency_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_checked_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_label: Option<String>,
}

/// One Docker/OCI container status row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerStatus {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Container state (e.g. `"running"`, `"exited"`, `"restarting"`).
    pub state: String,
    /// Healthcheck status (e.g. `"healthy"`, `"unhealthy"`, `"starting"`, `"none"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
}

/// One long-running daemon/service status row (`systemd`, `launchd`, etc.).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    /// `"systemd"`, `"launchd"`, or `"process"`.
    pub kind: String,
    pub state: ServiceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One object-storage or disk quota allocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageQuota {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub used_gib: f64,
    pub quota_gib: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One backup or replication freshness entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupStatus {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sla_secs: Option<u64>,
    pub state: BackupState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One expiring certificate, secret, token, or recovery drill deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiryItem {
    pub name: String,
    /// `"tls"`, `"token"`, `"seal"`, `"drill"`, `"task_not_before"`, etc.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_days: Option<u32>,
    #[serde(default = "default_true")]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One external alert pushed from a monitoring bridge (e.g. Alertmanager).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAlert {
    pub name: String,
    #[serde(default)]
    pub severity: AlertSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_since: Option<Timestamp>,
    /// True for dead-man's-switch heartbeat alerts (such as `Watchdog`) that indicate the
    /// alert pipeline is alive rather than an active incident.
    #[serde(default)]
    pub is_watchdog: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Secret-store / Vault health summary pushed by a host or monitoring bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultHealth {
    pub reachable: bool,
    pub sealed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_valid_until: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_running: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// CI watcher / ephemeral runner pool health summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiHealth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poller_fresh: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_poll_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_errors: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runners_online: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runners_busy: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `POST /v1/hosts/{host}/health` request body.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostHealthReport {
    /// Collector namespace for this host (default `"host"`). Separate collectors (such as
    /// `"host"` and `"prometheus"`) merge cleanly on read without overwriting each other.
    #[serde(default = "default_collector")]
    pub collector: String,
    /// Seconds after `reported_at` before this collector snapshot is considered stale.
    #[serde(default = "default_ttl_secs")]
    pub ttl_secs: u64,
    /// When the host collected this report; defaults to the server's clock when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// True when this host is a laptop or intermittent node expected to sleep offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleepable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<HostResources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<HostNetworkStatus>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub versions: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub containers: Vec<ContainerStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage: Vec<StorageQuota>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backups: Vec<BackupStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub certs: Vec<ExpiryItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<ExternalAlert>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<VaultHealth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci: Option<CiHealth>,
}

/// Answer to `POST /v1/hosts/{host}/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostHealthAck {
    pub host: DeviceName,
    pub collector: String,
    pub reported_at: Timestamp,
    pub ttl_secs: u64,
    pub recorded_sample: bool,
}

/// One normalized scheduled job reported by a host collector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledJobReport {
    /// Stable identifier within `(host, source)` (e.g. launchd label, systemd unit, T3 id).
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub category: ScheduleCategory,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub schedule_human: String,
    #[serde(default)]
    pub schedule_raw: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_interval_secs: Option<u64>,
    pub state: ScheduleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_count: Option<u64>,
    /// Secret-redacted error summary or stderr/stdout tail (at most 2,048 UTF-8 bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Secret-redacted command or prompt preview (at most 500 characters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_preview: Option<String>,
    /// Optional safe deep link (`https://`, `t3://`, or app-relative `/...`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep_link: Option<String>,
}

/// `POST /v1/hosts/{host}/schedules` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostHostSchedules {
    pub source: ScheduleSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<Timestamp>,
    #[serde(default)]
    pub jobs: Vec<ScheduledJobReport>,
}

/// Answer to `POST /v1/hosts/{host}/schedules`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSchedulesAck {
    pub host: DeviceName,
    pub source: ScheduleSource,
    pub upserted: u32,
    pub pruned: u32,
    pub history_recorded: u32,
    pub reported_at: Timestamp,
}

/// Stored and dynamically evaluated scheduled job returned by `GET /v1/schedules`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledJobSnapshot {
    pub job_key: String,
    pub host: DeviceName,
    pub source: ScheduleSource,
    pub id: String,
    pub name: String,
    pub category: ScheduleCategory,
    pub enabled: bool,
    pub schedule_human: String,
    pub schedule_raw: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_interval_secs: Option<u64>,
    /// Effective state computed against the server's clock (upgrades due `ok`/`never_run`
    /// jobs to `overdue`).
    pub state: ScheduleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep_link: Option<String>,
    /// True when the reporting host's health snapshot or job report is stale.
    #[serde(default)]
    pub host_stale: bool,
    /// True when the reporting host is marked sleepable and currently asleep/offline.
    #[serde(default)]
    pub host_asleep: bool,
    pub reported_at: Timestamp,
    pub updated_at: Timestamp,
}

/// KPI counts returned alongside `GET /v1/schedules`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleSummary {
    pub total: u32,
    pub failing: u32,
    pub overdue: u32,
    pub running: u32,
    pub ok: u32,
    pub never_run: u32,
    pub disabled: u32,
    /// Number of `os` category jobs excluded when `include_os` is false.
    pub os_hidden: u32,
}

/// Answer to `GET /v1/schedules`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleListResponse {
    pub generated_at: Timestamp,
    pub summary: ScheduleSummary,
    pub hosts: Vec<DeviceName>,
    pub jobs: Vec<ScheduledJobSnapshot>,
}

/// One ring-buffered run/state transition entry for a scheduled job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledJobHistoryEntry {
    pub id: i64,
    pub at: Timestamp,
    pub state: ScheduleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_count: Option<u64>,
}

/// Answer to `GET /v1/schedules/{job_key}/history`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleHistoryResponse {
    pub job_key: String,
    pub entries: Vec<ScheduledJobHistoryEntry>,
}

// ---------------------------------------------------------------------------
// Aggregated System Health Overview (`GET /v1/health/overview`)
// ---------------------------------------------------------------------------

/// Top-level incident counters for the global pulse bar.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentCounts {
    /// Firing external alerts excluding `Watchdog` heartbeats.
    pub firing_alerts: u32,
    /// `Some(true)` when a `Watchdog` alert is present and active; `None` if no external
    /// alert feed is reporting yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchdog_ok: Option<bool>,
    pub failing_jobs: u32,
    pub overdue_jobs: u32,
    pub stale_hosts: u32,
    /// Sleepable hosts currently offline/asleep (informational; does not degrade posture).
    #[serde(default)]
    pub asleep_hosts: u32,
    pub version_skews: u32,
    pub open_asks: u32,
    pub stale_leases: u32,
    pub open_feedback: u32,
}

/// Control plane (`board-server`) status and SQLite footprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHealthSummary {
    pub version: String,
    pub commit: String,
    pub schema_version: i64,
    pub uptime_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wal_bytes: Option<u64>,
    pub features: Vec<String>,
}

/// Live task queue counts across active boards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueCounts {
    pub ready_auto: u32,
    pub ready_manual: u32,
    pub doing: u32,
    pub review: u32,
    pub blocked: u32,
}

/// Deploy-train status for one `(board, repo)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployTrainStatus {
    pub board: BoardSlug,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub pending_tasks: Vec<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_ready_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flush_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_lock_holder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_deploy_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_deploy_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_deploy_sha: Option<String>,
}

/// One active file lease surfaced in hygiene checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseHygieneItem {
    pub id: i64,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    pub repo: String,
    pub glob: String,
    pub expires_at: Timestamp,
    /// True when the lease belongs to an ended agent or a terminal managed run.
    pub stale_holder: bool,
}

/// Active lease hygiene summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseHygieneSummary {
    pub active_count: u32,
    pub stale_holder_count: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<LeaseHygieneItem>,
}

/// Goal-review cooldown status for one active board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCooldownStatus {
    pub board: BoardSlug,
    pub autopilot: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_goal_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_eligible_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_remaining_secs: Option<u64>,
}

/// Autonomous AI factory & board pipeline health.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FactoryHealthSummary {
    pub active_runs: u32,
    pub queue: QueueCounts,
    pub runs_total: u32,
    pub runs_succeeded: u32,
    pub runs_failed: u32,
    pub runs_orphaned: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_pass_merge_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_review_rounds: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deploy_trains: Vec<DeployTrainStatus>,
    pub leases: LeaseHygieneSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub goal_cooldowns: Vec<GoalCooldownStatus>,
    pub open_feedback: u32,
}

/// One UTC calendar day bucket for token usage and cache-hit trend charts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DailyTokenBucket {
    /// `YYYY-MM-DD` (UTC).
    pub day: String,
    pub runs: u32,
    pub tokens_in: u64,
    pub billable_in: u64,
    pub cached_tokens: u64,
    pub tokens_out: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_rate: Option<f64>,
}

/// Token consumption rollup grouped by board, run kind, device, or model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenBreakdownRow {
    pub key: String,
    pub runs: u32,
    pub tokens_in: u64,
    pub billable_in: u64,
    pub cached_tokens: u64,
    pub tokens_out: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_rate: Option<f64>,
    pub avg_billable_per_run: u64,
}

/// Aggregated token usage and cache efficiency over the requested window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenAnalyticsSummary {
    pub days: u32,
    pub runs_with_tokens: u32,
    pub tokens_in: u64,
    pub billable_in: u64,
    pub cached_tokens: u64,
    pub tokens_out: u64,
    pub reasoning_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub daily: Vec<DailyTokenBucket>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_board: Vec<TokenBreakdownRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_kind: Vec<TokenBreakdownRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_device: Vec<TokenBreakdownRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_model: Vec<TokenBreakdownRow>,
}

/// One hourly host resource sample for mini trend sparklines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostResourceSample {
    pub hour_bucket: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_used_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_gib: Option<f64>,
}

/// Merged per-host health summary in [`HealthOverview`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostHealthSummary {
    pub host: DeviceName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_role: Option<DeviceRole>,
    pub status: DeviceStatus,
    /// True when this host is configured as sleepable/intermittent.
    #[serde(default)]
    pub sleepable: bool,
    /// True when this host is sleepable and currently offline/stale (asleep).
    #[serde(default)]
    pub asleep: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collectors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<HostResources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<HostNetworkStatus>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub versions: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub containers: Vec<ContainerStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<HostResourceSample>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostStorageQuota {
    pub host: DeviceName,
    #[serde(flatten)]
    pub quota: StorageQuota,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostBackupStatus {
    pub host: DeviceName,
    #[serde(flatten)]
    pub backup: BackupStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostExpiryItem {
    pub host: DeviceName,
    #[serde(flatten)]
    pub item: ExpiryItem,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostExternalAlert {
    pub host_reporter: DeviceName,
    #[serde(flatten)]
    pub alert: ExternalAlert,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostVaultHealth {
    pub host: DeviceName,
    #[serde(flatten)]
    pub vault: VaultHealth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCiHealth {
    pub host: DeviceName,
    #[serde(flatten)]
    pub ci: CiHealth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostVersionEntry {
    pub host: DeviceName,
    pub version: String,
}

/// Detected version skew for one component across hosts (or relative to the server commit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionSkew {
    pub component: String,
    pub expected: String,
    pub mismatched: Vec<HostVersionEntry>,
}

/// Answer to `GET /v1/health/overview?days=7`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthOverview {
    pub generated_at: Timestamp,
    pub days: u32,
    pub posture: HealthPosture,
    /// Human-readable reasons explaining why `posture` is `incident` or `degraded`
    /// (empty when `healthy`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub posture_reasons: Vec<String>,
    pub incidents: IncidentCounts,
    pub server: ServerHealthSummary,
    pub factory: FactoryHealthSummary,
    pub tokens: TokenAnalyticsSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<HostHealthSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage: Vec<HostStorageQuota>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backups: Vec<HostBackupStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub certs: Vec<HostExpiryItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<HostExternalAlert>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vault: Vec<HostVaultHealth>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ci: Vec<HostCiHealth>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub version_skews: Vec<VersionSkew>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage_buckets: Vec<UsageBucket>,
}

/// Per-device failure reported by `POST /v1/health/refresh`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthRefreshFailure {
    pub device: DeviceName,
    pub error: String,
}

/// Answer to `POST /v1/health/refresh`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthRefreshResponse {
    pub triggered: Vec<DeviceName>,
    pub failed: Vec<HealthRefreshFailure>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_and_schedule_open_enums_decode_unknown_safely() {
        assert_eq!(
            serde_json::from_str::<HealthPosture>("\"future\"").unwrap(),
            HealthPosture::Unknown
        );
        assert_eq!(
            serde_json::from_str::<AlertSeverity>("\"page\"").unwrap(),
            AlertSeverity::Unknown
        );
        assert_eq!(
            serde_json::from_str::<ServiceState>("\"reloading\"").unwrap(),
            ServiceState::Unknown
        );
        assert_eq!(
            serde_json::from_str::<BackupState>("\"verifying\"").unwrap(),
            BackupState::Unknown
        );
        assert_eq!(
            serde_json::from_str::<ScheduleSource>("\"nomad\"").unwrap(),
            ScheduleSource::Unknown
        );
        assert_eq!(
            serde_json::from_str::<ScheduleCategory>("\"ml\"").unwrap(),
            ScheduleCategory::Unknown
        );
        assert_eq!(
            serde_json::from_str::<ScheduleState>("\"paused\"").unwrap(),
            ScheduleState::Unknown
        );
    }

    #[test]
    fn host_health_report_defaults_and_round_trips() {
        let minimal: HostHealthReport = serde_json::from_str("{}").unwrap();
        assert_eq!(minimal.collector, "host");
        assert_eq!(minimal.ttl_secs, HOST_HEALTH_TTL_SECS_DEFAULT);
        assert!(minimal.containers.is_empty());
        assert!(minimal.alerts.is_empty());

        let json = serde_json::to_value(&minimal).unwrap();
        let back: HostHealthReport = serde_json::from_value(json).unwrap();
        assert_eq!(back, minimal);
    }
}
