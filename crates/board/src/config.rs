//! Client configuration in `$BOARD_HOME` and the device token.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use board_proto::{
    BoardSlug, DeviceName, JudgeKind, LEGACY_USAGE_ACCOUNT, LEGACY_USAGE_PROVIDER, QuotaBucketRef,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Keychain service name for the device token.
const KEYCHAIN_SERVICE: &str = "agent-board";
/// Keychain account of the default `$BOARD_HOME`; other homes add a suffix
/// ([`keychain_account`]).
const KEYCHAIN_ACCOUNT: &str = "device-token";
/// Board that `board feedback` files tasks on unless configured otherwise.
pub const DEFAULT_FEEDBACK_BOARD: &str = "board-feedback";

/// The client's data directory: `config.toml`, `token`, `cache/` and `state/`.
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// `$BOARD_HOME`, else `~/.local/share/agent-board`.
    pub fn from_env() -> Result<Self> {
        if let Some(root) = std::env::var_os("BOARD_HOME").filter(|v| !v.is_empty()) {
            return Ok(Self::at(PathBuf::from(root)));
        }
        let root = Self::default_root().context("no home directory; set BOARD_HOME")?;
        Ok(Self::at(root))
    }

    /// `~/.local/share/agent-board`, the home used when `$BOARD_HOME` is unset.
    pub fn default_root() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".local/share/agent-board"))
    }

    /// The home that holds the user's login on this device, which `board approved` acts
    /// with: `$BOARD_USER_HOME`, else the default home's path with `-user` added
    /// (`~/.local/share/agent-board-user`). It is a home of its own, so the device's token
    /// stays the one every other command uses.
    pub fn user_from_env() -> Result<Self> {
        if let Some(root) = std::env::var_os("BOARD_USER_HOME").filter(|v| !v.is_empty()) {
            return Ok(Self::at(PathBuf::from(root)));
        }
        let root = Self::default_root().context("no home directory; set BOARD_USER_HOME")?;
        Ok(Self::at(Self::user_root(&root)))
    }

    /// The user home beside the default home at `default_root`.
    fn user_root(default_root: &Path) -> PathBuf {
        let mut root = default_root.as_os_str().to_owned();
        root.push("-user");
        PathBuf::from(root)
    }

    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn token_path(&self) -> PathBuf {
        self.root.join("token")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
}

/// `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Base URL of the board server, such as `https://board.example.ts.net`.
    pub server: Option<String>,
    /// This device's name, as registered on the server.
    pub device: Option<DeviceName>,
    /// Board that `board feedback` files tasks on.
    pub feedback_board: Option<BoardSlug>,
    /// Path of the `t3` binary used by `board watch`.
    pub t3_bin: Option<PathBuf>,
    /// How many runs, workers and judges together, `board watch` has at once on this
    /// device across its daemon-dispatch boards; without it, one per 4 GiB of host memory
    /// and at least 2.
    pub runs_at_once: Option<std::num::NonZeroU32>,
    /// Device turns `board watch` leaves free for threads it did not start (such as owner
    /// threads calling `delegate_task` or `t3_thread_launch`); without it, 1. Set 0 to let
    /// the daemon use every slot of `runs_at_once`.
    #[serde(alias = "reserve")]
    pub reserve_runs: Option<u32>,
    /// Total weighted capacity units of this device (`light=1`, `standard=2`, `heavy=4`);
    /// without it, derived from `runs_at_once * 2` or host memory (`docs/SCHEDULER_DESIGN.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight_units: Option<u32>,
    /// Extra capability tags advertised in device heartbeats alongside auto-detected
    /// `os:<os>` and `arch:<arch>` (for example `["git:cloneable", "tool:xcode"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Runners advertised in device heartbeats (defaults to the daemon's active runner,
    /// e.g. `["t3"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runners: Vec<String>,
    /// Protocol features advertised in device heartbeats (defaults to
    /// `["v1-json", "sse-v1"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protocol_features: Vec<String>,
    /// Local T3 project folders by repo key, for starting worker and judge threads.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, PathBuf>,
    /// Local T3 project folders by board slug, for a board none of whose repos is under
    /// `projects` (such as a feedback board with no repo).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub board_projects: BTreeMap<BoardSlug, PathBuf>,
    /// Optional base branch overrides by repo key (e.g. `"github.com/owner/repo" = "fork"`)
    /// or board slug, for fork repositories whose `origin/HEAD` mirrors an upstream branch
    /// while managed work lives on another branch.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "project_base_branches"
    )]
    pub project_base_branches: BTreeMap<String, String>,
    /// The T3 provider instance and model for the threads `board watch` starts; without
    /// it they get the project's defaults.
    pub thread_model: Option<ThreadModel>,
    /// The T3 provider instance and model for judge threads `board watch` starts; without
    /// it they use `[thread_model]`, else the project's defaults.
    pub judge_model: Option<ThreadModel>,
    /// Optional per-kind judge overrides (`[judge_models.review]`, `review_light`, `verdict`,
    /// `triage`, `goal`, `curate`); each unset kind falls back to `[judge_model]`, then
    /// `[thread_model]`, then the project's defaults.
    #[serde(default, skip_serializing_if = "JudgeModels::is_empty")]
    pub judge_models: JudgeModels,
    /// Whether `board watch` adapts `RunKind::Review` reasoning effort by diff/size/phase
    /// (defaults to `true` when omitted): keeps `[judge_models.review]` (`thought_level = "high"`)
    /// for substantive code PRs (`s`/`m`/`l`, unknown-size code PRs, and security-sensitive
    /// diffs), and uses `[judge_models.review_light]` or downgrades `thought_level` / `effort`
    /// to `"medium"` for non-security `xs` PRs, docs/prompt-only diffs, and `after-merge`
    /// verification reviews. Set `false` to always use `[judge_models.review]` unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_review_effort: Option<bool>,
    /// Optional headless ACP runner configuration (`[acp_runner]`) for invoking an official,
    /// unmodified ACP-compliant vendor binary over stdio JSON-RPC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_runner: Option<AcpRunnerConfig>,
    /// Optional transcript archival configuration (`[archive]`) for uploading completed run
    /// transcripts to S3-compatible object storage (`docs/SCHEDULER_DESIGN.md`, Section 5.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveConfig>,
    /// How `board watch` dispatches managed work: `"auto"` (default: capability-matched pull
    /// via `POST /v1/dispatch/claim` whenever the server supports `dispatch-claim-v1`, else
    /// legacy board leases), `"pull"`, or `"legacy"`.
    #[serde(default, skip_serializing_if = "DispatchMode::is_default")]
    pub dispatch_mode: DispatchMode,
    /// Optional host health telemetry reporting (`[telemetry]`, off unless `enabled = true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryConfig>,
    /// Optional scheduled-job reporting (`[schedules]`, off unless `enabled = true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedules: Option<SchedulesConfig>,
}

fn default_report_interval_secs() -> u64 {
    60
}

fn default_report_ttl_secs() -> u64 {
    board_proto::HOST_HEALTH_TTL_SECS_DEFAULT
}

fn default_collector_name() -> String {
    "host".to_owned()
}

fn default_true() -> bool {
    true
}

/// Optional `[telemetry]` configuration for `board report-host` and periodic `board watch`
/// reporting. Off unless `enabled = true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Periodic push interval in seconds for `board watch` (minimum 30, default 60).
    #[serde(default = "default_report_interval_secs")]
    pub interval_secs: u64,
    /// Snapshot TTL in seconds sent to `POST /v1/hosts/{host}/health` (default 300).
    #[serde(default = "default_report_ttl_secs")]
    pub ttl_secs: u64,
    /// Collector namespace for this host (default `"host"`).
    #[serde(default = "default_collector_name")]
    pub collector: String,
    /// Collect host CPU, memory, root disk, and uptime metrics (default true).
    #[serde(default = "default_true")]
    pub resources: bool,
    /// Collect Tailscale status (`tailscale status --json`) when available.
    #[serde(default)]
    pub tailscale: bool,
    /// Collect Docker containers (`docker ps -a --format '{{json .}}'`).
    #[serde(default)]
    pub docker: bool,
    /// Systemd unit names to inspect as long-running services on Linux.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub systemd_services: Vec<String>,
    /// macOS LaunchAgent labels (e.g. KeepAlive daemons) to inspect as services.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launchd_services: Vec<String>,
    /// Optional component version probe commands (`component = ["bin", "arg1", ...]`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub version_probes: BTreeMap<String, Vec<String>>,
    /// Optional Linux systemd slice name whose CPU quota is reported (e.g. `"build.slice"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cap_slice: Option<String>,
    /// True for laptops or intermittent runners expected to sleep offline (shown as `asleep`
    /// rather than stale/degraded when offline).
    #[serde(default)]
    pub sleepable: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: default_report_interval_secs(),
            ttl_secs: default_report_ttl_secs(),
            collector: default_collector_name(),
            resources: true,
            tailscale: false,
            docker: false,
            systemd_services: Vec::new(),
            launchd_services: Vec::new(),
            version_probes: BTreeMap::new(),
            cpu_cap_slice: None,
            sleepable: false,
        }
    }
}

/// Optional `[schedules]` configuration for `board report-host` and periodic `board watch`
/// reporting. Off unless `enabled = true`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulesConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Collect macOS `~/Library/LaunchAgents/*.plist` + `launchctl print`.
    #[serde(default)]
    pub launchd: bool,
    /// Optional LaunchAgents directory override (defaults to `~/Library/LaunchAgents`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launchd_dir: Option<PathBuf>,
    /// When non-empty, only include LaunchAgent labels matching at least one prefix/glob.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launchd_include: Vec<String>,
    /// Exclude LaunchAgent labels matching any prefix/glob (e.g. KeepAlive daemons).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launchd_exclude: Vec<String>,
    /// Collect Linux `systemctl list-timers --all --output=json`.
    #[serde(default)]
    pub systemd_timers: bool,
    /// Additional timer unit prefixes classified as `os` maintenance timers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os_timer_prefixes: Vec<String>,
    /// Collect user/root crontab entries (`crontab -l`).
    #[serde(default)]
    pub crontab: bool,
    /// Optional read-only path to a T3 Code `statev2.sqlite` database (`scheduled_tasks`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t3_state_db: Option<PathBuf>,
    /// Optional deep-link base URL for T3 threads (e.g. `"t3://thread/"` or `"https://..."`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t3_link_base: Option<String>,
    /// Category overrides matched as case-insensitive substrings against job id or name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub categories: BTreeMap<String, board_proto::ScheduleCategory>,
}

/// Optional `[archive]` configuration in `config.toml` for uploading completed run
/// transcripts to S3-compatible object storage (`docs/SCHEDULER_DESIGN.md`, Section 5.2).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveConfig {
    /// Enable transcript archival when managed runs reach a terminal state.
    #[serde(default)]
    pub enabled: bool,
    /// Command template executed directly (no shell) to upload a transcript file. Supports
    /// `{file}`, `{run_id}`, `{board}`, and `{uri}` placeholders in each argument. Prints the
    /// uploaded URI on stdout (falling back to `{uri}` derived from `uri_prefix` when stdout
    /// is empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Optional object storage URI prefix (for example `"s3://archive-bucket/runs/"`), used
    /// to build `{uri}` as `<prefix>/<board>/<run_id>.jsonl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri_prefix: Option<String>,
}

/// Dispatch mode for `board watch` (`docs/SCHEDULER_DESIGN.md`, Section 3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchMode {
    /// Use capability-matched pull (`POST /v1/dispatch/claim`) whenever the server advertises
    /// or accepts `dispatch-claim-v1`, falling back to legacy 45-minute board coordinator
    /// leases on older servers returning 404/405.
    #[default]
    Auto,
    /// Always use capability-matched pull dispatch (`POST /v1/dispatch/claim`).
    Pull,
    /// Use legacy per-board coordinator-lease dispatch (`POST /v1/boards/{slug}/coordinator`).
    Legacy,
}

impl DispatchMode {
    fn is_default(&self) -> bool {
        *self == Self::Auto
    }
}

/// Default per-turn timeout for [`AcpRunnerConfig::turn_timeout_secs`] (30 minutes).
pub const DEFAULT_ACP_TURN_TIMEOUT_SECS: u64 = 1800;

/// Default graceful cancel wait for [`AcpRunnerConfig::cancel_grace_secs`] (15 seconds).
pub const DEFAULT_ACP_CANCEL_GRACE_SECS: u64 = 15;

/// Policy for incoming ACP `session/request_permission` client requests in headless
/// [`crate::acp_runner::AcpRunner`] runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpPermissionPolicy {
    /// Select the first `allow_always` option, else the first `allow_once` option, else reply
    /// `cancelled` when no allow option is offered (default for headless autonomous runs).
    #[default]
    Allow,
    /// Select the first `reject_once` option, else the first `reject_always` option, else
    /// reply `cancelled` when no reject option is offered.
    Deny,
}

impl AcpPermissionPolicy {
    pub fn is_default(&self) -> bool {
        *self == Self::Allow
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// Optional `[acp_runner]` configuration in `config.toml` for headless ACP execution using an
/// official, unmodified vendor binary (`docs/SCHEDULER_DESIGN.md`, Section 4.3).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpRunnerConfig {
    /// Path to the official, unmodified ACP-compliant binary (overridden by `BOARD_ACP_BIN`
    /// when set and non-empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<PathBuf>,
    /// Arguments passed to the official binary (defaults to `["acp"]` when omitted).
    #[serde(default = "default_acp_args")]
    pub args: Vec<String>,
    /// Optional extra environment variables passed to the child process. Values are never
    /// logged or written to transcripts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Optional model slug forwarded via ACP session params/config when set.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "opt_non_empty"
    )]
    pub model: Option<String>,
    /// Optional session/model options forwarded via ACP session params/config when set.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "model_options"
    )]
    pub options: BTreeMap<String, String>,
    /// Maximum wall-clock seconds for a single ACP turn before timeout teardown (default 1800s).
    #[serde(
        default = "default_acp_turn_timeout_secs",
        deserialize_with = "positive_u64"
    )]
    pub turn_timeout_secs: u64,
    /// Seconds to wait after `session/cancel` before escalating to `SIGTERM` / `SIGKILL`
    /// (default 15s).
    #[serde(default = "default_acp_cancel_grace_secs")]
    pub cancel_grace_secs: u64,
    /// Policy for incoming ACP `session/request_permission` client requests (`"allow"` or
    /// `"deny"`, defaulting to `"allow"` for headless autonomous runs).
    #[serde(default, skip_serializing_if = "AcpPermissionPolicy::is_default")]
    pub permission_policy: AcpPermissionPolicy,
    /// Optional Linux `systemd-run --user --scope` cgroup isolation settings
    /// (`[acp_runner.cgroup]`).
    #[serde(default, skip_serializing_if = "CgroupConfig::is_default")]
    pub cgroup: CgroupConfig,
}

impl Default for AcpRunnerConfig {
    fn default() -> Self {
        Self {
            bin: None,
            args: default_acp_args(),
            env: BTreeMap::new(),
            model: None,
            options: BTreeMap::new(),
            turn_timeout_secs: DEFAULT_ACP_TURN_TIMEOUT_SECS,
            cancel_grace_secs: DEFAULT_ACP_CANCEL_GRACE_SECS,
            permission_policy: AcpPermissionPolicy::default(),
            cgroup: CgroupConfig::default(),
        }
    }
}

impl std::fmt::Debug for AcpRunnerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact env values so secret credentials passed to the child are never logged.
        let redacted_env: BTreeMap<&str, &str> = self
            .env
            .keys()
            .map(|k| (k.as_str(), "<redacted>"))
            .collect();
        f.debug_struct("AcpRunnerConfig")
            .field("bin", &self.bin)
            .field("args", &self.args)
            .field("env", &redacted_env)
            .field("model", &self.model)
            .field("options", &self.options)
            .field("turn_timeout_secs", &self.turn_timeout_secs)
            .field("cancel_grace_secs", &self.cancel_grace_secs)
            .field("permission_policy", &self.permission_policy)
            .field("cgroup", &self.cgroup)
            .finish()
    }
}

impl AcpRunnerConfig {
    /// Resolves the configured official ACP binary path, honoring a non-empty `BOARD_ACP_BIN`
    /// environment override before `self.bin`.
    pub fn resolved_bin(&self) -> Option<PathBuf> {
        let env_bin = std::env::var_os("BOARD_ACP_BIN");
        self.resolved_bin_with_env(env_bin.as_deref())
    }

    /// Pure binary resolver taking an explicit `BOARD_ACP_BIN` override (for unit tests).
    pub fn resolved_bin_with_env(&self, env_bin: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
        if let Some(raw) = env_bin {
            let text = raw.to_string_lossy();
            if !text.trim().is_empty() {
                return Some(PathBuf::from(raw));
            }
        }
        self.bin.clone().filter(|p| !p.as_os_str().is_empty())
    }
}

/// Optional Linux systemd user scope cgroup limits under `[acp_runner.cgroup]`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CgroupConfig {
    /// Wrap launches with `systemd-run --user --scope` on Linux when true.
    #[serde(default)]
    pub enabled: bool,
    /// systemd `MemoryMax=` property (for example `"4G"`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "opt_systemd_value"
    )]
    pub memory_max: Option<String>,
    /// systemd `CPUQuota=` property (for example `"200%"`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "opt_systemd_value"
    )]
    pub cpu_quota: Option<String>,
    /// systemd `TasksMax=` property (for example `256`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "opt_positive_u32"
    )]
    pub tasks_max: Option<u32>,
    /// systemd `Slice=` property (for example `"agent-board-runs.slice"`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "opt_systemd_value"
    )]
    pub slice: Option<String>,
}

impl CgroupConfig {
    pub fn is_default(&self) -> bool {
        !self.enabled
            && self.memory_max.is_none()
            && self.cpu_quota.is_none()
            && self.tasks_max.is_none()
            && self.slice.is_none()
    }
}

fn default_acp_args() -> Vec<String> {
    vec!["acp".to_owned()]
}

fn default_acp_turn_timeout_secs() -> u64 {
    DEFAULT_ACP_TURN_TIMEOUT_SECS
}

fn default_acp_cancel_grace_secs() -> u64 {
    DEFAULT_ACP_CANCEL_GRACE_SECS
}

fn opt_non_empty<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(text) if text.trim().is_empty() => Err(serde::de::Error::custom("must not be empty")),
        Some(text) => Ok(Some(text)),
    }
}

fn opt_systemd_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(text) => {
            if text.is_empty()
                || text
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || c == '=')
            {
                return Err(serde::de::Error::custom(format!(
                    "invalid systemd property value {text:?}: must be non-empty without whitespace, control characters, or '='"
                )));
            }
            Ok(Some(text))
        }
    }
}

fn positive_u64<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("must be greater than 0"));
    }
    Ok(value)
}

fn opt_positive_u32<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    let opt = Option::<u32>::deserialize(deserializer)?;
    if opt == Some(0) {
        return Err(serde::de::Error::custom("must be greater than 0"));
    }
    Ok(opt)
}

/// Adaptive reasoning-effort tier selected for a `RunKind::Review` judge run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewEffort {
    /// Substantive code PRs (`s`/`m`/`l`, unknown-size code PRs, or security-sensitive diffs).
    High,
    /// Lighter reviews: `after-merge` verification, docs/prompt-only diffs, or non-security `xs` PRs.
    Medium,
}

/// Optional per-kind judge model overrides in `config.toml` (`[judge_models.<kind>]`).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeModels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ThreadModel>,
    /// Optional explicit model/options override for lighter `ReviewEffort::Medium` review runs
    /// (`xs` non-security PRs, docs/prompt-only diffs, and `after-merge` verification).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_light: Option<ThreadModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<ThreadModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage: Option<ThreadModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<ThreadModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curate: Option<ThreadModel>,
}

impl JudgeModels {
    pub fn is_empty(&self) -> bool {
        self.review.is_none()
            && self.review_light.is_none()
            && self.verdict.is_none()
            && self.triage.is_none()
            && self.goal.is_none()
            && self.curate.is_none()
    }

    pub fn for_kind(&self, kind: JudgeKind) -> Option<&ThreadModel> {
        match kind {
            JudgeKind::Review => self.review.as_ref(),
            JudgeKind::Verdict => self.verdict.as_ref(),
            JudgeKind::Triage => self.triage.as_ref(),
            JudgeKind::Goal => self.goal.as_ref(),
        }
    }

    /// Any configured judge override, used as a fallback for usage-provider discovery
    /// when neither `[thread_model]` nor `[judge_model]` is set.
    pub fn first(&self) -> Option<&ThreadModel> {
        self.review
            .as_ref()
            .or(self.review_light.as_ref())
            .or(self.verdict.as_ref())
            .or(self.triage.as_ref())
            .or(self.goal.as_ref())
            .or(self.curate.as_ref())
    }
}

/// A T3 provider instance and a model of it: `t3 thread start` takes them only together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadModel {
    /// The provider instance's id in T3's settings.
    #[serde(deserialize_with = "non_empty")]
    pub provider: String,
    /// The model's slug.
    #[serde(deserialize_with = "non_empty")]
    pub model: String,
    /// Optional model options passed as repeated `--option <key>=<value>` flags.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "model_options"
    )]
    pub options: BTreeMap<String, String>,
}

/// A string with text in it: an empty one would fail every thread start.
fn non_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let text = String::deserialize(deserializer)?;
    if text.trim().is_empty() {
        return Err(serde::de::Error::custom(
            "is empty: give the T3 provider instance's id and a model slug",
        ));
    }
    Ok(text)
}

/// Model option map for `t3 thread start --option <key>=<value>`: rejects empty keys,
/// keys containing `=`, or empty values at load time.
fn model_options<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let options = BTreeMap::<String, String>::deserialize(deserializer)?;
    for (key, value) in &options {
        if key.trim().is_empty() {
            return Err(serde::de::Error::custom(
                "option key is empty: give non-empty id = value entries",
            ));
        }
        if key.contains('=') {
            return Err(serde::de::Error::custom(format!(
                "option key {key:?} contains '=': `--option` splits on the first '='"
            )));
        }
        if value.trim().is_empty() {
            return Err(serde::de::Error::custom(format!(
                "option {key:?} has an empty value"
            )));
        }
    }
    Ok(options)
}

fn is_high_reasoning_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "high" | "xhigh" | "max" | "very_high"
    )
}

/// Downgrades high reasoning effort options (`thought_level`, `effort`) to `"medium"` in
/// `target`. Checks `fallback_options` (e.g. `[acp_runner].options`) when a key is not
/// explicitly present in `target` so runner-level defaults are overridden cleanly.
fn downgrade_reasoning_options(
    target: &mut BTreeMap<String, String>,
    fallback_options: Option<&BTreeMap<String, String>>,
) {
    for key in ["thought_level", "effort"] {
        if let Some(val) = target.get(key) {
            if is_high_reasoning_value(val) {
                target.insert(key.to_owned(), "medium".to_owned());
            }
        } else if let Some(val) = fallback_options.and_then(|opts| opts.get(key))
            && is_high_reasoning_value(val)
        {
            target.insert(key.to_owned(), "medium".to_owned());
        }
    }
}

/// Base-branch overrides map (`[project_base_branches]`): rejects empty keys or invalid git
/// branch names at load time, normalizes repo keys via [`board_proto::normalize_repo`] when
/// applicable (matching `[projects]` lookup), and strips any leading `origin/` prefix.
fn project_base_branches<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let raw = BTreeMap::<String, String>::deserialize(deserializer)?;
    let mut normalized = BTreeMap::new();
    for (key, value) in raw {
        let key_trimmed = key.trim();
        if key_trimmed.is_empty() {
            return Err(serde::de::Error::custom(
                "project_base_branches key is empty: use a repo key or board slug",
            ));
        }
        let key_norm =
            board_proto::normalize_repo(key_trimmed).unwrap_or_else(|| key_trimmed.to_owned());
        let branch = value.trim();
        let branch = branch.strip_prefix("origin/").unwrap_or(branch);
        if !crate::worktree::valid_branch_name(branch) {
            return Err(serde::de::Error::custom(format!(
                "project_base_branches[{key_trimmed:?}] has invalid git branch name {value:?}"
            )));
        }
        normalized.insert(key_norm, branch.to_owned());
    }
    Ok(normalized)
}

impl Config {
    /// Reads `config.toml`; a missing file is an empty configuration.
    pub fn load(home: &Home) -> Result<Self> {
        let path = home.config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, home: &Home) -> Result<()> {
        std::fs::create_dir_all(home.root())
            .with_context(|| format!("creating {}", home.root().display()))?;
        let text = toml::to_string_pretty(self).context("serializing config")?;
        let path = home.config_path();
        let has_secrets = self
            .acp_runner
            .as_ref()
            .is_some_and(|acp| !acp.env.is_empty());
        if has_secrets || is_private_file(&path) {
            write_private(&path, text.as_bytes())
        } else {
            write_atomic(&path, text.as_bytes())
        }
    }

    pub fn feedback_board(&self) -> BoardSlug {
        self.feedback_board.clone().unwrap_or_else(|| {
            DEFAULT_FEEDBACK_BOARD
                .parse()
                .expect("default feedback board is a valid slug")
        })
    }

    /// Configured base branch override for `board` / `repo_key`, checking `repo_key` first
    /// (normalized via [`board_proto::normalize_repo`]) and then `board`. Returns the
    /// normalized branch name (without a leading `origin/`).
    pub fn base_branch(&self, board: &BoardSlug, repo_key: Option<&str>) -> Option<&str> {
        let raw = repo_key
            .and_then(|repo| {
                self.project_base_branches.get(repo).or_else(|| {
                    let norm = board_proto::normalize_repo(repo)?;
                    self.project_base_branches.get(&norm)
                })
            })
            .or_else(|| self.project_base_branches.get(board.as_str()))?;
        let trimmed = raw.trim();
        let name = trimmed.strip_prefix("origin/").unwrap_or(trimmed);
        (!name.is_empty()).then_some(name)
    }

    /// Server URL and device name, or an error saying how to log in.
    pub fn require_login(&self) -> Result<(&str, &DeviceName)> {
        match (self.server.as_deref(), self.device.as_ref()) {
            (Some(server), Some(device)) => Ok((server, device)),
            _ => bail!("not logged in: run `board login --server <url> --device <name>`"),
        }
    }

    /// Model selection for a judge of `kind`: `[judge_models.<kind>]`, else
    /// `[judge_model]`, else `[thread_model]`, else `None` (project defaults).
    pub fn model_for_judge(&self, kind: JudgeKind) -> Option<&ThreadModel> {
        self.judge_models
            .for_kind(kind)
            .or(self.judge_model.as_ref())
            .or(self.thread_model.as_ref())
    }

    /// Whether adaptive review reasoning effort is enabled (defaults to `true`).
    pub fn adaptive_review_effort(&self) -> bool {
        self.adaptive_review_effort.unwrap_or(true)
    }

    /// Model selection for a `RunKind::Review` judge with adaptive reasoning `effort` on
    /// `runner_kind` (`"acp"`, `"t3"`, etc.).
    ///
    /// - For [`ReviewEffort::High`] (or when `adaptive_review_effort = false`), returns the
    ///   standard review model (`[judge_models.review]`, else `[judge_model]`, else
    ///   `[thread_model]`, else `None`) unchanged.
    /// - For [`ReviewEffort::Medium`] with adaptive effort enabled:
    ///   1. Uses explicit `[judge_models.review_light]` when configured.
    ///   2. Otherwise clones the standard review model and downgrades any high reasoning
    ///      option (`thought_level` or `effort` set to `"high"`, `"xhigh"`, `"max"`, or
    ///      `"very_high"`) to `"medium"`, without upgrading lower configured levels (`"low"`,
    ///      `"minimal"`, etc.). When running on `"acp"`, also overrides a high
    ///      `[acp_runner].options` `thought_level`/`effort` down to `"medium"`.
    pub fn model_for_review(&self, effort: ReviewEffort, runner_kind: &str) -> Option<ThreadModel> {
        if effort == ReviewEffort::High || !self.adaptive_review_effort() {
            return self.model_for_judge(JudgeKind::Review).cloned();
        }
        if let Some(light) = &self.judge_models.review_light {
            return Some(light.clone());
        }
        let acp_opts = (runner_kind == "acp")
            .then(|| self.acp_runner.as_ref().map(|a| &a.options))
            .flatten();
        if let Some(base) = self.model_for_judge(JudgeKind::Review) {
            let mut adjusted = base.clone();
            downgrade_reasoning_options(&mut adjusted.options, acp_opts);
            return Some(adjusted);
        }
        // ACP-only host with `[acp_runner]` model + high reasoning option and no
        // `[judge_models.review]` / `[judge_model]` / `[thread_model]`.
        if runner_kind == "acp"
            && let Some(acp) = &self.acp_runner
            && let Some(model) = &acp.model
        {
            let mut override_opts = BTreeMap::new();
            downgrade_reasoning_options(&mut override_opts, Some(&acp.options));
            if !override_opts.is_empty() {
                return Some(ThreadModel {
                    provider: "acp".to_owned(),
                    model: model.clone(),
                    options: override_opts,
                });
            }
        }
        None
    }

    /// Model selection for a `RunKind::Curate` judge: `[judge_models.curate]`, else
    /// `[judge_model]`, else `[thread_model]`, else `None` (project defaults).
    pub fn model_for_curate(&self) -> Option<&ThreadModel> {
        self.judge_models
            .curate
            .as_ref()
            .or(self.judge_model.as_ref())
            .or(self.thread_model.as_ref())
    }

    /// Provider instance used to locate usage readings: `[thread_model]`, else
    /// `[judge_model]`, else the first configured `[judge_models.<kind>]`.
    pub fn usage_provider(&self) -> Option<&str> {
        self.thread_model
            .as_ref()
            .or(self.judge_model.as_ref())
            .or_else(|| self.judge_models.first())
            .map(|m| m.provider.as_str())
    }

    /// Target `(provider, account)` LLM quota bucket advertised to `POST /v1/dispatch/claim`,
    /// derived from configured provider/runner settings (`antigravity:default`,
    /// `openai:default`, or legacy `anthropic:default`).
    pub fn quota_bucket(&self) -> QuotaBucketRef {
        if let Some(provider) = self.usage_provider() {
            let lower = provider.to_ascii_lowercase();
            let bucket_provider = if lower.contains("antigravity") || lower.contains("gemini") {
                "antigravity"
            } else if lower.contains("codex") || lower.contains("openai") {
                "openai"
            } else {
                LEGACY_USAGE_PROVIDER
            };
            return QuotaBucketRef {
                provider: bucket_provider.to_owned(),
                account: LEGACY_USAGE_ACCOUNT.to_owned(),
            };
        }
        if let Some(acp) = &self.acp_runner
            && acp.resolved_bin().is_some()
        {
            let hint = acp
                .model
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let provider = if hint.contains("claude") || hint.contains("anthropic") {
                LEGACY_USAGE_PROVIDER
            } else if hint.contains("gpt") || hint.contains("codex") || hint.contains("openai") {
                "openai"
            } else {
                "antigravity"
            };
            return QuotaBucketRef {
                provider: provider.to_owned(),
                account: LEGACY_USAGE_ACCOUNT.to_owned(),
            };
        }
        QuotaBucketRef {
            provider: LEGACY_USAGE_PROVIDER.to_owned(),
            account: LEGACY_USAGE_ACCOUNT.to_owned(),
        }
    }
}

/// Where the device token is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStore {
    Keychain,
    File,
}

impl TokenStore {
    /// The macOS Keychain when available, unless `BOARD_KEYCHAIN=0`; otherwise a file.
    pub fn for_platform() -> Self {
        let disabled = std::env::var("BOARD_KEYCHAIN").is_ok_and(|v| v == "0");
        if cfg!(target_os = "macos") && !disabled {
            Self::Keychain
        } else {
            Self::File
        }
    }
}

/// The device token: `BOARD_TOKEN`, else the Keychain, else `$BOARD_HOME/token`.
pub fn load_token(home: &Home) -> Result<Option<String>> {
    load_token_from(
        home,
        std::env::var("BOARD_TOKEN").ok(),
        TokenStore::for_platform(),
        &SecurityCli,
    )
}

/// The token stored for `home` itself: the Keychain, else `home`'s token file. Never
/// `BOARD_TOKEN`, which names the session's device token whatever the home.
pub fn load_stored_token(home: &Home) -> Result<Option<String>> {
    load_token_from(home, None, TokenStore::for_platform(), &SecurityCli)
}

fn load_token_from(
    home: &Home,
    env_token: Option<String>,
    store: TokenStore,
    keychain: &dyn Keychain,
) -> Result<Option<String>> {
    if let Some(token) = env_token
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
    {
        return Ok(Some(token));
    }
    if store == TokenStore::Keychain {
        match keychain.read(&keychain_account_for(home)) {
            Ok(Some(token)) => return Ok(Some(token)),
            Ok(None) => {}
            // Locked, or no user interaction (SSH, launchd): the file may hold the token.
            Err(err) => tracing::debug!("reading the Keychain: {err:#}"),
        }
    }
    match std::fs::read_to_string(home.token_path()) {
        Ok(text) => Ok(Some(text.trim().to_owned()).filter(|t| !t.is_empty())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", home.token_path().display())),
    }
}

/// Where [`store_token`] put the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredToken {
    pub store: TokenStore,
    /// Why the token is in the file although the platform uses the Keychain.
    pub note: Option<String>,
}

/// Stores the token and returns where it went. Never prints the token.
///
/// When the Keychain refuses it (locked, or no user interaction as over SSH or under
/// launchd), the token goes to `$BOARD_HOME/token` (0600) instead, so a token the server
/// has already issued is not lost.
pub fn store_token(home: &Home, token: &str) -> Result<StoredToken> {
    store_token_in(home, token, TokenStore::for_platform(), &SecurityCli)
}

fn store_token_in(
    home: &Home,
    token: &str,
    store: TokenStore,
    keychain: &dyn Keychain,
) -> Result<StoredToken> {
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic() && b != b'"') {
        bail!("the token must be one line of printable characters without quotes");
    }
    let mut note = None;
    if store == TokenStore::Keychain {
        let account = keychain_account_for(home);
        let Err(err) = keychain.write(&account, token) else {
            // A token file from an earlier fallback would be read when the Keychain is not.
            remove_if_present(&home.token_path())?;
            return Ok(StoredToken { store, note });
        };
        // An older token for this home would still be read before the file.
        keychain.delete(&account);
        let remove = format!(
            "remove it from a desktop session with `security delete-generic-password -s \
             {KEYCHAIN_SERVICE} -a {account}`"
        );
        let stale = match keychain.read(&account) {
            Ok(Some(old)) if old != token => Some(format!(
                "an older token for this home is still in the Keychain and is read before \
                 the file: {remove}"
            )),
            Ok(_) => None,
            Err(_) => Some(format!(
                "the Keychain could not be checked for an older token for this home, which \
                 would be read before the file in a desktop session; if `board` then fails \
                 with unauthorized, {remove}"
            )),
        };
        let mut text = format!("the Keychain refused the token ({err:#})");
        if let Some(stale) = stale {
            text.push_str("; ");
            text.push_str(&stale);
        }
        note = Some(text);
    }
    std::fs::create_dir_all(home.root())
        .with_context(|| format!("creating {}", home.root().display()))?;
    write_private(&home.token_path(), token.as_bytes())?;
    Ok(StoredToken {
        store: TokenStore::File,
        note,
    })
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// The Keychain operations the token store needs; tests use a fake.
trait Keychain {
    /// `Ok(None)` when the account has no item; an error when the Keychain cannot be read.
    fn read(&self, account: &str) -> Result<Option<String>>;
    fn write(&self, account: &str, token: &str) -> Result<()>;
    /// Best effort: removes the account's item if it can.
    fn delete(&self, account: &str);
}

/// The macOS Keychain through the `security` tool.
struct SecurityCli;

impl Keychain for SecurityCli {
    fn read(&self, account: &str) -> Result<Option<String>> {
        keychain_read(account)
    }

    fn write(&self, account: &str, token: &str) -> Result<()> {
        keychain_write(account, token)
    }

    fn delete(&self, account: &str) {
        let _ = Command::new("security")
            .args([
                "delete-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                account,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The Keychain account holding the token of the home at `root`: [`KEYCHAIN_ACCOUNT`] for
/// the default home (where existing installs keep it), and for any other home that name
/// plus a hash of the home's canonical path, so a second login with another
/// `$BOARD_HOME` (such as a `--user` token) never replaces this device's token.
fn keychain_account(root: &Path, default_root: Option<&Path>) -> String {
    let root = canonical(root);
    if default_root.is_some_and(|default| canonical(default) == root) {
        return KEYCHAIN_ACCOUNT.to_owned();
    }
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    format!("{KEYCHAIN_ACCOUNT}-{}", hex(&digest[..8]))
}

/// `bytes` as lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn keychain_account_for(home: &Home) -> String {
    keychain_account(home.root(), Home::default_root().as_deref())
}

/// `path` made absolute with symlinks resolved as far as it exists, so a home that does
/// not exist yet names the same account once it does.
fn canonical(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            return rest.iter().rev().fold(resolved, |p, part| p.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return absolute,
        }
    }
}

fn keychain_read(account: &str) -> Result<Option<String>> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE])
        .args(["-a", account, "-w"])
        .stderr(Stdio::null())
        .output();
    let out = output.context("running `security`")?;
    if out.status.success() {
        let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        return Ok(Some(token).filter(|t| !t.is_empty()));
    }
    // errSecItemNotFound.
    if out.status.code() == Some(44) {
        return Ok(None);
    }
    bail!("`security find-generic-password` failed: {}", out.status)
}

/// Writes through `security -i` so the token never appears in a process's arguments.
fn keychain_write(account: &str, token: &str) -> Result<()> {
    let mut child = Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running `security`")?;
    let command =
        format!("add-generic-password -U -s {KEYCHAIN_SERVICE} -a {account} -w \"{token}\"\n");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(command.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() || !out.stderr.is_empty() {
        bail!(
            "storing the token in the Keychain failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Replaces `path` with `bytes` through a temporary file and a rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_path(path);
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// Like [`write_atomic`], with mode 0600 on Unix.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_path(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(bytes)?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

fn is_private_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o077 == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_model_needs_its_provider_and_its_model() {
        let config: Config =
            toml::from_str("[thread_model]\nprovider = \"claude-agents\"\nmodel = \"a-model\"\n")
                .unwrap();
        let model = ThreadModel {
            provider: "claude-agents".into(),
            model: "a-model".into(),
            options: BTreeMap::new(),
        };
        assert_eq!(config.thread_model, Some(model));
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(!text.contains("options"), "{text}");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);

        assert_eq!(toml::from_str::<Config>("").unwrap().thread_model, None);
        assert_eq!(toml::from_str::<Config>("").unwrap().judge_model, None);
        let err =
            toml::from_str::<Config>("[thread_model]\nprovider = \"claude-agents\"\n").unwrap_err();
        assert!(err.to_string().contains("model"), "{err}");
        let err = toml::from_str::<Config>("[thread_model]\nprovider = \"\"\nmodel = \"m\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("is empty"), "{err}");
    }

    #[test]
    fn thread_and_judge_models_validate_options_and_round_trip() {
        let toml = r#"
[thread_model]
provider = "antigravity"
model = "gemini-4-neon"

[thread_model.options]
thought_level = "high"

[judge_model]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "medium", verbosity = "low" }
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.thread_model,
            Some(ThreadModel {
                provider: "antigravity".into(),
                model: "gemini-4-neon".into(),
                options: [("thought_level".into(), "high".into())].into(),
            })
        );
        assert_eq!(
            config.judge_model,
            Some(ThreadModel {
                provider: "antigravity".into(),
                model: "gemini-4-neon".into(),
                options: [
                    ("thought_level".into(), "medium".into()),
                    ("verbosity".into(), "low".into()),
                ]
                .into(),
            })
        );
        let serialized = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&serialized).unwrap(), config);

        for (case, expected) in [
            (
                "[thread_model]\nprovider = \"p\"\nmodel = \"m\"\n[thread_model.options]\n\"\" = \"v\"\n",
                "option key is empty",
            ),
            (
                "[thread_model]\nprovider = \"p\"\nmodel = \"m\"\n[thread_model.options]\n\"  \" = \"v\"\n",
                "option key is empty",
            ),
            (
                "[thread_model]\nprovider = \"p\"\nmodel = \"m\"\n[thread_model.options]\n\"a=b\" = \"v\"\n",
                "contains '='",
            ),
            (
                "[judge_model]\nprovider = \"p\"\nmodel = \"m\"\n[judge_model.options]\nthought_level = \"\"\n",
                "empty value",
            ),
            (
                "[judge_model]\nprovider = \"p\"\nmodel = \"m\"\n[judge_model.options]\nthought_level = \"   \"\n",
                "empty value",
            ),
        ] {
            let err = toml::from_str::<Config>(case).unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "case {case:?} -> {err} (expected {expected:?})"
            );
        }
    }

    #[test]
    fn per_kind_judge_models_override_fallback_validate_and_round_trip() {
        let toml = r#"
[thread_model]
provider = "antigravity"
model = "gemini-4-neon"

[judge_model]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "medium" }

[judge_models.review]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "high" }

[judge_models.triage]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "low" }

[judge_models.goal]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "high" }
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let high = ThreadModel {
            provider: "antigravity".into(),
            model: "gemini-4-neon".into(),
            options: [("thought_level".into(), "high".into())].into(),
        };
        let medium = ThreadModel {
            provider: "antigravity".into(),
            model: "gemini-4-neon".into(),
            options: [("thought_level".into(), "medium".into())].into(),
        };
        let low = ThreadModel {
            provider: "antigravity".into(),
            model: "gemini-4-neon".into(),
            options: [("thought_level".into(), "low".into())].into(),
        };
        assert_eq!(config.model_for_judge(JudgeKind::Review), Some(&high));
        assert_eq!(config.model_for_judge(JudgeKind::Goal), Some(&high));
        assert_eq!(config.model_for_judge(JudgeKind::Triage), Some(&low));
        // Unset [judge_models.verdict] falls back to [judge_model].
        assert_eq!(config.model_for_judge(JudgeKind::Verdict), Some(&medium));

        let serialized = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&serialized).unwrap(), config);

        // Empty judge_models is omitted on serialization.
        let empty = Config::default();
        assert!(empty.judge_models.is_empty());
        assert!(
            !toml::to_string_pretty(&empty)
                .unwrap()
                .contains("judge_models")
        );
        assert_eq!(empty.model_for_judge(JudgeKind::Review), None);
        assert_eq!(empty.usage_provider(), None);

        // usage_provider falls back to judge_models when thread_model and judge_model are unset.
        let only_triage: Config = toml::from_str(
            "[judge_models.triage]\nprovider = \"antigravity\"\nmodel = \"gemini-4-neon\"\n",
        )
        .unwrap();
        assert_eq!(only_triage.usage_provider(), Some("antigravity"));
        assert_eq!(only_triage.model_for_judge(JudgeKind::Review), None);

        // Unknown judge kind key is rejected.
        let err =
            toml::from_str::<Config>("[judge_models.reviews]\nprovider = \"p\"\nmodel = \"m\"\n")
                .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn curate_judge_model_overrides_fallback_and_round_trips() {
        let curate_model = ThreadModel {
            provider: "antigravity".into(),
            model: "gemini-4-neon".into(),
            options: [("thought_level".into(), "medium".into())].into(),
        };
        let judge_model = ThreadModel {
            provider: "antigravity".into(),
            model: "gemini-4-neon".into(),
            options: [("thought_level".into(), "high".into())].into(),
        };
        let thread_model = ThreadModel {
            provider: "antigravity".into(),
            model: "gemini-4-neon".into(),
            options: BTreeMap::new(),
        };

        // Explicit [judge_models.curate] wins and round-trips through TOML.
        let cfg: Config = toml::from_str(
            r#"
[thread_model]
provider = "antigravity"
model = "gemini-4-neon"

[judge_model]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "high" }

[judge_models.curate]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "medium" }
"#,
        )
        .unwrap();
        assert_eq!(cfg.model_for_curate(), Some(&curate_model));
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        assert!(serialized.contains("[judge_models.curate]"), "{serialized}");
        assert_eq!(toml::from_str::<Config>(&serialized).unwrap(), cfg);

        // Falls back to [judge_model], then [thread_model], then None.
        let judge_fallback = Config {
            thread_model: Some(thread_model.clone()),
            judge_model: Some(judge_model.clone()),
            ..Config::default()
        };
        assert_eq!(judge_fallback.model_for_curate(), Some(&judge_model));

        let thread_fallback = Config {
            thread_model: Some(thread_model.clone()),
            ..Config::default()
        };
        assert_eq!(thread_fallback.model_for_curate(), Some(&thread_model));
        assert_eq!(Config::default().model_for_curate(), None);

        // usage_provider also discovers [judge_models.curate] when nothing else is set.
        let only_curate: Config = toml::from_str(
            "[judge_models.curate]\nprovider = \"antigravity\"\nmodel = \"gemini-4-neon\"\n",
        )
        .unwrap();
        assert_eq!(only_curate.usage_provider(), Some("antigravity"));
    }

    #[test]
    fn adaptive_review_effort_downgrades_high_honors_review_light_and_opt_out() {
        let cfg: Config = toml::from_str(
            r#"
[judge_models.review]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "high", verbosity = "low" }
"#,
        )
        .unwrap();
        assert!(cfg.adaptive_review_effort());
        let high = cfg.model_for_review(ReviewEffort::High, "acp").unwrap();
        assert_eq!(
            high.options.get("thought_level").map(String::as_str),
            Some("high")
        );
        assert_eq!(
            high.options.get("verbosity").map(String::as_str),
            Some("low")
        );

        let med = cfg.model_for_review(ReviewEffort::Medium, "acp").unwrap();
        assert_eq!(
            med.options.get("thought_level").map(String::as_str),
            Some("medium")
        );
        assert_eq!(
            med.options.get("verbosity").map(String::as_str),
            Some("low"),
            "sibling options must be preserved"
        );

        // Claude/OpenAI `effort = "high"` also downgrades to `"medium"`.
        let claude_cfg: Config = toml::from_str(
            r#"
[judge_models.review]
provider = "claude-agents"
model = "claude-opus"
options = { effort = "high" }
"#,
        )
        .unwrap();
        assert_eq!(
            claude_cfg
                .model_for_review(ReviewEffort::Medium, "t3")
                .unwrap()
                .options
                .get("effort")
                .map(String::as_str),
            Some("medium")
        );

        // Lower configured thought_level ("low") is never upgraded to "medium".
        let low_cfg: Config = toml::from_str(
            r#"
[judge_models.review]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "low" }
"#,
        )
        .unwrap();
        assert_eq!(
            low_cfg
                .model_for_review(ReviewEffort::Medium, "acp")
                .unwrap()
                .options
                .get("thought_level")
                .map(String::as_str),
            Some("low")
        );

        // Explicit [judge_models.review_light] wins for ReviewEffort::Medium and round-trips.
        let light_cfg: Config = toml::from_str(
            r#"
[judge_models.review]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "high" }

[judge_models.review_light]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "low" }
"#,
        )
        .unwrap();
        assert_eq!(
            light_cfg
                .model_for_review(ReviewEffort::High, "acp")
                .unwrap()
                .options
                .get("thought_level")
                .map(String::as_str),
            Some("high")
        );
        assert_eq!(
            light_cfg
                .model_for_review(ReviewEffort::Medium, "acp")
                .unwrap()
                .options
                .get("thought_level")
                .map(String::as_str),
            Some("low")
        );
        let serialized = toml::to_string_pretty(&light_cfg).unwrap();
        assert!(
            serialized.contains("[judge_models.review_light]"),
            "{serialized}"
        );
        assert_eq!(toml::from_str::<Config>(&serialized).unwrap(), light_cfg);

        // Opt-out (`adaptive_review_effort = false`) keeps [judge_models.review] unchanged.
        let opt_out: Config = toml::from_str(
            r#"
adaptive_review_effort = false

[judge_models.review]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "high" }

[judge_models.review_light]
provider = "antigravity"
model = "gemini-4-neon"
options = { thought_level = "low" }
"#,
        )
        .unwrap();
        assert!(!opt_out.adaptive_review_effort());
        assert_eq!(
            opt_out
                .model_for_review(ReviewEffort::Medium, "acp")
                .unwrap()
                .options
                .get("thought_level")
                .map(String::as_str),
            Some("high")
        );

        // ACP-only host with high thought_level in [acp_runner] overrides down to medium on acp.
        let acp_only: Config = toml::from_str(
            r#"
[acp_runner]
bin = "/bin/sh"
model = "gemini-4-neon"
options = { thought_level = "high" }
"#,
        )
        .unwrap();
        assert_eq!(acp_only.model_for_review(ReviewEffort::High, "acp"), None);
        let acp_med = acp_only
            .model_for_review(ReviewEffort::Medium, "acp")
            .unwrap();
        assert_eq!(acp_med.model, "gemini-4-neon");
        assert_eq!(
            acp_med.options.get("thought_level").map(String::as_str),
            Some("medium")
        );
        // Never synthesizes an ACP model when running on t3.
        assert_eq!(acp_only.model_for_review(ReviewEffort::Medium, "t3"), None);
    }

    #[test]
    fn config_round_trips_and_defaults_feedback_board() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let empty = Config::load(&home).unwrap();
        assert_eq!(empty, Config::default());
        assert_eq!(empty.feedback_board().as_str(), "board-feedback");
        assert!(empty.require_login().is_err());

        let config = Config {
            server: Some("http://127.0.0.1:1".into()),
            device: Some("mac-air".parse().unwrap()),
            projects: [("github.com/o/r".into(), PathBuf::from("/src/r"))].into(),
            board_projects: [("board-feedback".parse().unwrap(), PathBuf::from("/src/fb"))].into(),
            ..Config::default()
        };
        config.save(&home).unwrap();
        assert_eq!(Config::load(&home).unwrap(), config);
        let text = std::fs::read_to_string(home.config_path()).unwrap();
        assert!(
            text.contains("[board_projects]\nboard-feedback = \"/src/fb\""),
            "{text}"
        );
    }

    #[test]
    fn weighted_device_config_fields_are_optional_and_round_trip() {
        let empty = Config::default();
        let text = toml::to_string_pretty(&empty).unwrap();
        assert!(!text.contains("weight_units"), "{text}");
        assert!(!text.contains("capabilities"), "{text}");
        assert!(!text.contains("runners"), "{text}");
        assert!(!text.contains("protocol_features"), "{text}");

        let raw = r#"
server = "https://board.example.test"
device = "mac-pro"
runs_at_once = 6
reserve_runs = 1
weight_units = 12
capabilities = ["git:cloneable", "tool:xcode"]
runners = ["t3", "acp"]
protocol_features = ["v1-json", "sse-v1"]
"#;
        let parsed: Config = toml::from_str(raw).unwrap();
        assert_eq!(parsed.weight_units, Some(12));
        assert_eq!(parsed.capabilities, vec!["git:cloneable", "tool:xcode"]);
        assert_eq!(parsed.runners, vec!["t3", "acp"]);
        assert_eq!(parsed.protocol_features, vec!["v1-json", "sse-v1"]);
        let serialized = toml::to_string_pretty(&parsed).unwrap();
        assert_eq!(toml::from_str::<Config>(&serialized).unwrap(), parsed);
    }

    #[test]
    fn keychain_accounts_are_per_home_and_keep_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("share/agent-board");
        std::fs::create_dir_all(&default).unwrap();
        let account = |root: &Path| keychain_account(root, Some(&default));

        // The default home keeps the account existing installs use, however it is spelled.
        assert_eq!(account(&default), "device-token");
        assert_eq!(account(&default.join("../agent-board")), "device-token");
        assert_eq!(
            keychain_account(&default, None).len(),
            "device-token-".len() + 16
        );

        // Another home gets its own account, stable before and after the home exists.
        let user = dir.path().join("user-home");
        let before = account(&user);
        std::fs::create_dir_all(&user).unwrap();
        assert_eq!(account(&user), before);
        assert!(before.starts_with("device-token-"));
        assert_ne!(before, "device-token");
        assert_ne!(account(&dir.path().join("other")), before);
        assert!(
            before
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        );

        #[cfg(unix)]
        {
            // A symlink to a home names the same account as the home.
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&user, &link).unwrap();
            assert_eq!(account(&link), before);
            let default_link = dir.path().join("default-link");
            std::os::unix::fs::symlink(&default, &default_link).unwrap();
            assert_eq!(account(&default_link), "device-token");
        }
    }

    /// A Keychain that holds items in memory and can refuse writes and deletes.
    #[derive(Default)]
    struct FakeKeychain {
        items: std::cell::RefCell<BTreeMap<String, String>>,
        refuse_read: bool,
        refuse_write: bool,
        refuse_delete: bool,
    }

    impl Keychain for FakeKeychain {
        fn read(&self, account: &str) -> Result<Option<String>> {
            if self.refuse_read {
                bail!("User interaction is not allowed.");
            }
            Ok(self.items.borrow().get(account).cloned())
        }

        fn write(&self, account: &str, token: &str) -> Result<()> {
            if self.refuse_write {
                bail!("User interaction is not allowed. add-generic-password: returned -25308");
            }
            self.items
                .borrow_mut()
                .insert(account.to_owned(), token.to_owned());
            Ok(())
        }

        fn delete(&self, account: &str) {
            if !self.refuse_delete {
                self.items.borrow_mut().remove(account);
            }
        }
    }

    #[test]
    fn tokens_go_to_the_keychain_and_load_from_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let keychain = FakeKeychain::default();
        // A token file left by an earlier fallback goes once the Keychain has the token.
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(home.token_path(), "revoked").unwrap();
        let stored = store_token_in(&home, "tok-1", TokenStore::Keychain, &keychain).unwrap();
        assert_eq!(
            stored,
            StoredToken {
                store: TokenStore::Keychain,
                note: None
            }
        );
        assert!(!home.token_path().exists());
        let load = |env: Option<&str>| {
            load_token_from(
                &home,
                env.map(str::to_owned),
                TokenStore::Keychain,
                &keychain,
            )
            .unwrap()
        };
        assert_eq!(load(None).as_deref(), Some("tok-1"));
        // BOARD_TOKEN still comes first.
        assert_eq!(load(Some(" env-tok ")).as_deref(), Some("env-tok"));
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_keychain_write_falls_back_to_the_private_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let account = keychain_account_for(&home);
        // An older token for this home, which the fallback removes.
        let keychain = FakeKeychain {
            refuse_write: true,
            ..FakeKeychain::default()
        };
        keychain
            .items
            .borrow_mut()
            .insert(account.clone(), "old-tok".into());

        let stored = store_token_in(&home, "new-tok", TokenStore::Keychain, &keychain).unwrap();
        assert_eq!(stored.store, TokenStore::File);
        let note = stored.note.unwrap();
        assert!(note.contains("-25308"), "{note}");
        assert!(!note.contains("still in the Keychain"), "{note}");
        assert_eq!(
            std::fs::read_to_string(home.token_path()).unwrap(),
            "new-tok"
        );
        let mode = std::fs::metadata(home.token_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // The usual order (BOARD_TOKEN, Keychain, file) finds it.
        assert_eq!(
            load_token_from(&home, None, TokenStore::Keychain, &keychain)
                .unwrap()
                .as_deref(),
            Some("new-tok")
        );

        // When the older token cannot be removed either, the note says how.
        let stuck = FakeKeychain {
            refuse_write: true,
            refuse_delete: true,
            ..FakeKeychain::default()
        };
        stuck
            .items
            .borrow_mut()
            .insert(account.clone(), "old-tok".into());
        let note = store_token_in(&home, "new-tok", TokenStore::Keychain, &stuck)
            .unwrap()
            .note
            .unwrap();
        assert!(note.contains("still in the Keychain"), "{note}");
        assert!(note.contains(&format!("-a {account}")), "{note}");

        // A locked Keychain refuses reads too: the token is still found in the file, and
        // the note says an older Keychain token could not be ruled out.
        let locked = FakeKeychain {
            refuse_read: true,
            refuse_write: true,
            refuse_delete: true,
            ..FakeKeychain::default()
        };
        let note = store_token_in(&home, "locked-tok", TokenStore::Keychain, &locked)
            .unwrap()
            .note
            .unwrap();
        assert!(note.contains("could not be checked"), "{note}");
        assert_eq!(
            load_token_from(&home, None, TokenStore::Keychain, &locked)
                .unwrap()
                .as_deref(),
            Some("locked-tok")
        );
    }

    #[test]
    fn acp_runner_config_defaults_validates_redacts_env_and_round_trips() {
        // Omitting [acp_runner] stays backward compatible and omitted on serialization.
        let empty = Config::default();
        assert_eq!(empty.acp_runner, None);
        assert!(
            !toml::to_string_pretty(&empty)
                .unwrap()
                .contains("acp_runner")
        );

        // Minimal [acp_runner] applies defaults: args = ["acp"], timeouts, cgroup disabled.
        let minimal: Config =
            toml::from_str("[acp_runner]\nbin = \"/usr/local/bin/official-acp\"\n").unwrap();
        let acp = minimal.acp_runner.as_ref().unwrap();
        assert_eq!(
            acp.bin.as_deref(),
            Some(Path::new("/usr/local/bin/official-acp"))
        );
        assert_eq!(acp.args, vec!["acp"]);
        assert_eq!(acp.turn_timeout_secs, DEFAULT_ACP_TURN_TIMEOUT_SECS);
        assert_eq!(acp.cancel_grace_secs, DEFAULT_ACP_CANCEL_GRACE_SECS);
        assert_eq!(acp.permission_policy, AcpPermissionPolicy::Allow);
        assert!(!acp.cgroup.enabled);
        assert_eq!(
            acp.resolved_bin_with_env(None),
            Some(PathBuf::from("/usr/local/bin/official-acp"))
        );
        assert_eq!(
            acp.resolved_bin_with_env(Some(std::ffi::OsStr::new("/opt/override-acp"))),
            Some(PathBuf::from("/opt/override-acp"))
        );
        assert_eq!(
            acp.resolved_bin_with_env(Some(std::ffi::OsStr::new("   "))),
            Some(PathBuf::from("/usr/local/bin/official-acp"))
        );

        // Full [acp_runner] + [acp_runner.cgroup] round-trips and redacts env in Debug.
        let full_raw = r#"
[acp_runner]
bin = "/usr/local/bin/official-acp"
args = ["acp", "--stdio"]
model = "gemini-4-neon"
turn_timeout_secs = 900
cancel_grace_secs = 10
permission_policy = "deny"

[acp_runner.env]
API_SECRET = "super-secret-value"

[acp_runner.options]
thought_level = "high"

[acp_runner.cgroup]
enabled = true
memory_max = "4G"
cpu_quota = "200%"
tasks_max = 256
slice = "agent-board-runs.slice"
"#;
        let full: Config = toml::from_str(full_raw).unwrap();
        let acp = full.acp_runner.as_ref().unwrap();
        assert_eq!(acp.args, vec!["acp", "--stdio"]);
        assert_eq!(acp.model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(acp.turn_timeout_secs, 900);
        assert_eq!(acp.cancel_grace_secs, 10);
        assert_eq!(acp.permission_policy, AcpPermissionPolicy::Deny);
        assert!(acp.cgroup.enabled);
        assert_eq!(acp.cgroup.memory_max.as_deref(), Some("4G"));
        assert_eq!(acp.cgroup.cpu_quota.as_deref(), Some("200%"));
        assert_eq!(acp.cgroup.tasks_max, Some(256));
        assert_eq!(acp.cgroup.slice.as_deref(), Some("agent-board-runs.slice"));
        let debug_text = format!("{acp:?}");
        assert!(debug_text.contains("API_SECRET"), "{debug_text}");
        assert!(debug_text.contains("<redacted>"), "{debug_text}");
        assert!(
            !debug_text.contains("super-secret-value"),
            "env secret leaked in Debug: {debug_text}"
        );
        let serialized = toml::to_string_pretty(&full).unwrap();
        assert!(
            serialized.contains("permission_policy = \"deny\""),
            "{serialized}"
        );
        assert_eq!(toml::from_str::<Config>(&serialized).unwrap(), full);

        let allow_cfg: Config =
            toml::from_str("[acp_runner]\npermission_policy = \"allow\"\n").unwrap();
        assert_eq!(
            allow_cfg.acp_runner.as_ref().unwrap().permission_policy,
            AcpPermissionPolicy::Allow
        );
        let allow_serialized = toml::to_string_pretty(&allow_cfg).unwrap();
        assert_eq!(
            toml::from_str::<Config>(&allow_serialized).unwrap(),
            allow_cfg
        );

        // Validation rejects invalid values and unknown fields.
        for bad in [
            "[acp_runner]\nturn_timeout_secs = 0\n",
            "[acp_runner]\nmodel = \"  \"\n",
            "[acp_runner]\npermission_policy = \"ask\"\n",
            "[acp_runner]\npermission_policy = \"Allow\"\n",
            "[acp_runner]\nunknown_key = 1\n",
            "[acp_runner.cgroup]\ntasks_max = 0\n",
            "[acp_runner.cgroup]\nmemory_max = \"4G --evil\"\n",
            "[acp_runner.cgroup]\nslice = \"a=b\"\n",
        ] {
            assert!(
                toml::from_str::<Config>(bad).is_err(),
                "expected error for {bad:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn config_save_uses_0600_for_acp_env_secrets_and_preserves_private_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));

        // Saving a config with [acp_runner.env] writes mode 0600 automatically.
        let mut cfg = Config {
            acp_runner: Some(AcpRunnerConfig {
                env: [("API_KEY".to_owned(), "secret-val".to_owned())].into(),
                ..AcpRunnerConfig::default()
            }),
            ..Config::default()
        };
        cfg.save(&home).unwrap();
        let mode = std::fs::metadata(home.config_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        // Subsequent saves (e.g., board login) preserve 0600 even if env is later cleared.
        cfg.acp_runner = None;
        cfg.server = Some("https://board.example.test".into());
        cfg.save(&home).unwrap();
        let mode = std::fs::metadata(home.config_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn private_files_are_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        write_private(&path, b"secret").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn dispatch_mode_and_quota_bucket_parse_and_default_cleanly() {
        let default_cfg: Config = toml::from_str("").unwrap();
        assert_eq!(default_cfg.dispatch_mode, DispatchMode::Auto);
        assert_eq!(
            default_cfg.quota_bucket(),
            QuotaBucketRef {
                provider: LEGACY_USAGE_PROVIDER.to_owned(),
                account: LEGACY_USAGE_ACCOUNT.to_owned(),
            }
        );

        let pull_cfg: Config = toml::from_str("dispatch_mode = \"pull\"\n").unwrap();
        assert_eq!(pull_cfg.dispatch_mode, DispatchMode::Pull);

        let legacy_cfg: Config = toml::from_str("dispatch_mode = \"legacy\"\n").unwrap();
        assert_eq!(legacy_cfg.dispatch_mode, DispatchMode::Legacy);

        let antigravity_cfg: Config = toml::from_str(
            r#"
[thread_model]
provider = "antigravity"
model = "gemini-4-neon"
"#,
        )
        .unwrap();
        assert_eq!(
            antigravity_cfg.quota_bucket(),
            QuotaBucketRef {
                provider: "antigravity".to_owned(),
                account: "default".to_owned(),
            }
        );

        let acp_cfg: Config = toml::from_str(
            r#"
[acp_runner]
bin = "/usr/local/bin/official-acp"
model = "gemini-4-neon"
"#,
        )
        .unwrap();
        assert_eq!(
            acp_cfg.quota_bucket(),
            QuotaBucketRef {
                provider: "antigravity".to_owned(),
                account: "default".to_owned(),
            }
        );

        let archive_cfg: Config = toml::from_str(
            r#"
[archive]
enabled = true
command = ["archive-tool", "put", "{file}", "{uri}"]
uri_prefix = "s3://archive-bucket/runs/"
"#,
        )
        .unwrap();
        let archive = archive_cfg.archive.as_ref().expect("archive parsed");
        assert!(archive.enabled);
        assert_eq!(
            archive.command.as_deref(),
            Some(
                [
                    "archive-tool".to_owned(),
                    "put".to_owned(),
                    "{file}".to_owned(),
                    "{uri}".to_owned(),
                ]
                .as_slice()
            )
        );
        assert_eq!(
            archive.uri_prefix.as_deref(),
            Some("s3://archive-bucket/runs/")
        );
        let serialized = toml::to_string_pretty(&archive_cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.archive, archive_cfg.archive);
    }

    #[test]
    fn project_base_branches_parse_normalize_and_validate() {
        let cfg: Config = toml::from_str(
            r#"
[project_base_branches]
"https://github.com/MyOrg/Fork-Repo.git" = "origin/fork"
"fallback-board" = "develop"
"#,
        )
        .unwrap();
        let b1: BoardSlug = "fork-board".parse().unwrap();
        let b2: BoardSlug = "fallback-board".parse().unwrap();
        assert_eq!(
            cfg.base_branch(&b1, Some("github.com/myorg/fork-repo")),
            Some("fork")
        );
        // Mixed-case repo lookup also matches the normalized key.
        assert_eq!(
            cfg.base_branch(&b1, Some("github.com/MyOrg/Fork-Repo")),
            Some("fork")
        );
        // Repo key wins over board slug when both are present.
        assert_eq!(
            cfg.base_branch(&b2, Some("github.com/myorg/fork-repo")),
            Some("fork")
        );
        // Falls back to board slug when repo key has no override.
        assert_eq!(
            cfg.base_branch(&b2, Some("github.com/org/other")),
            Some("develop")
        );
        assert_eq!(cfg.base_branch(&b1, None), None);

        // Invalid git branch names fail fast at config load time.
        let bad: Result<Config, _> = toml::from_str(
            r#"
[project_base_branches]
"github.com/org/fork-repo" = "bad..branch"
"#,
        );
        assert!(bad.is_err());
    }
}
