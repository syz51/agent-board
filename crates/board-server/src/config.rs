use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use board_proto::{AutotuneMode, BoardSlug};
use chrono::{FixedOffset, NaiveTime};
use serde::{Deserialize, Deserializer, Serialize};

use crate::Error;

/// Environment variable that supplies the Telegram bot token, overriding the config file.
pub const TELEGRAM_TOKEN_ENV: &str = "BOARD_TELEGRAM_TOKEN";

/// Server configuration, usually read from a TOML file:
///
/// ```toml
/// db = "/var/lib/agent-board/board.db"
/// listen = "127.0.0.1:7377"
/// users = ["user@example.com"]
/// public_url = "https://board.example.ts.net"
///
/// [telegram]
/// chat_id = 123456789          # optional; else paired with `board admin telegram-pair`
/// stuck_after_secs = 14400      # 0 turns the stuck-task alert off
/// stalled_after_secs = 7200     # 0 turns the stalled-board alert off
/// digest_at = "09:00"           # daily Needs you digest, local time; "off" turns it off
/// utc_offset = "+00:00"         # the local time's offset from UTC
///
/// [defaults]
/// owner_stale_after_secs = 86400
/// ```
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// SQLite database file; created and migrated on start.
    pub db: PathBuf,
    /// Address to listen on. Keep it on loopback and publish it with `tailscale serve`.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Tailscale logins (`Tailscale-User-Login`) treated as the user.
    #[serde(default)]
    pub users: Vec<String>,
    /// Trust the `Tailscale-User-Login` header. Safe only when the server is reachable
    /// solely through `tailscale serve`, which sets that header and strips any sent by the
    /// client; anyone who can reach the listener directly could forge it.
    #[serde(default = "default_true")]
    pub trust_identity_header: bool,
    /// Base URL of the web view, for links in notifications (no trailing slash needed).
    #[serde(default)]
    pub public_url: Option<String>,
    /// Telegram notifier settings. The notifier runs whenever a token is set
    /// (`BOARD_TELEGRAM_TOKEN`), with or without this table.
    #[serde(default)]
    pub telegram: Option<TelegramConfig>,
    /// Overrides of the rule and rendering defaults (`docs/API.md`, "Defaults").
    #[serde(default)]
    pub defaults: Settings,
    /// Scheduler subsystem configuration (`[scheduler]` / `[scheduler.autotune]`).
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    /// The commit the server was built from, which `/healthz` and every answer report. Set
    /// by the binary that runs the server, never read from the file.
    #[serde(skip, default = "unknown_commit")]
    pub commit: String,
    /// The build's order stamp (seconds since the Unix epoch, or `unknown`), which every
    /// answer reports. Set by the binary that runs the server, never read from the file.
    #[serde(skip, default = "unknown_commit")]
    pub built: String,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 7377))
}

fn unknown_commit() -> String {
    "unknown".to_owned()
}

fn default_true() -> bool {
    true
}

impl ServerConfig {
    /// A configuration with every default, storing the database at `db`.
    pub fn new(db: impl Into<PathBuf>) -> Self {
        Self {
            db: db.into(),
            listen: default_listen(),
            users: Vec::new(),
            trust_identity_header: true,
            public_url: None,
            telegram: None,
            defaults: Settings::default(),
            scheduler: SchedulerConfig::default(),
            commit: unknown_commit(),
            built: unknown_commit(),
        }
    }

    /// Parses and checks a TOML configuration.
    pub fn from_toml(text: &str) -> Result<Self, Error> {
        let config: Self =
            toml::from_str(text).map_err(|e| Error::Config(format!("invalid config: {e}")))?;
        config.check()?;
        Ok(config)
    }

    /// Reads and checks a TOML configuration file.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        Self::from_toml(&text)
    }

    pub(crate) fn check(&self) -> Result<(), Error> {
        if self.users.iter().any(|u| u.trim().is_empty()) {
            return Err(Error::Config("users must not contain empty logins".into()));
        }
        if let Some(url) = &self.public_url
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            return Err(Error::Config(
                "public_url must start with http:// or https://".into(),
            ));
        }
        if let Some(telegram) = &self.telegram
            && !(telegram.api_base.starts_with("http://")
                || telegram.api_base.starts_with("https://"))
        {
            return Err(Error::Config(
                "telegram.api_base must start with http:// or https://".into(),
            ));
        }
        self.scheduler.check()?;
        self.defaults.check()
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("db", &self.db)
            .field("listen", &self.listen)
            .field("users", &self.users)
            .field("trust_identity_header", &self.trust_identity_header)
            .field("public_url", &self.public_url)
            .field("telegram", &self.telegram)
            .field("defaults", &self.defaults)
            .field("scheduler", &self.scheduler)
            .finish()
    }
}

/// Scheduler subsystem configuration (`[scheduler]` in `server.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulerConfig {
    /// Automated scheduler evaluation and guardrailed self-tuning (`[scheduler.autotune]`).
    pub autotune: AutotuneConfig,
}

impl SchedulerConfig {
    pub(crate) fn check(&self) -> Result<(), Error> {
        self.autotune.check()
    }
}

/// Configuration for the automated scheduler evaluation and self-tuning loop
/// (`[scheduler.autotune]` in `server.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutotuneConfig {
    /// Whether periodic background evaluations run in the server sweeper. Default `true`.
    pub enabled: bool,
    /// Evaluation mode (`off`, `propose`, or `apply`). Default `apply`.
    pub mode: AutotuneMode,
    /// Minimum interval between scheduled evaluations in seconds (default 6h, min 60s).
    pub interval_secs: u64,
    /// Historical replay window in days (default 7, range 1..=90).
    pub window_days: u32,
    /// Minimum recorded `run_outcomes` in the window before running candidate search (default 20).
    pub min_runs: u32,
    /// Minimum composite replay improvement percent required to promote or propose (default 5.0).
    pub min_improvement_pct: f64,
    /// Minimum cooldown in seconds between promotions/proposals/rollbacks (default 24h).
    pub cooldown_secs: u64,
    /// Minimum post-promotion `run_outcomes` required before the rollback watchdog evaluates (default 10).
    pub rollback_min_runs: u32,
    /// Maximum tolerated increase in live `orphan_rate` over baseline before auto-rollback (default 0.05).
    pub rollback_orphan_delta: f64,
    /// Maximum tolerated ratio of live `ready_to_leased` p90 over baseline before auto-rollback (default 1.25).
    pub rollback_p90_ratio: f64,
}

impl Default for AutotuneConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: AutotuneMode::Apply,
            interval_secs: 21_600,
            window_days: 7,
            min_runs: 20,
            min_improvement_pct: 5.0,
            cooldown_secs: 86_400,
            rollback_min_runs: 10,
            rollback_orphan_delta: 0.05,
            rollback_p90_ratio: 1.25,
        }
    }
}

impl PartialEq for AutotuneConfig {
    fn eq(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.mode == other.mode
            && self.interval_secs == other.interval_secs
            && self.window_days == other.window_days
            && self.min_runs == other.min_runs
            && self
                .min_improvement_pct
                .total_cmp(&other.min_improvement_pct)
                .is_eq()
            && self.cooldown_secs == other.cooldown_secs
            && self.rollback_min_runs == other.rollback_min_runs
            && self
                .rollback_orphan_delta
                .total_cmp(&other.rollback_orphan_delta)
                .is_eq()
            && self
                .rollback_p90_ratio
                .total_cmp(&other.rollback_p90_ratio)
                .is_eq()
    }
}

impl Eq for AutotuneConfig {}

impl AutotuneConfig {
    pub(crate) fn check(&self) -> Result<(), Error> {
        if self.mode == AutotuneMode::Unknown {
            return Err(Error::Config(
                "scheduler.autotune.mode must be off, propose, or apply".into(),
            ));
        }
        if self.interval_secs < 60 {
            return Err(Error::Config(
                "scheduler.autotune.interval_secs must be at least 60".into(),
            ));
        }
        if !(1..=90).contains(&self.window_days) {
            return Err(Error::Config(
                "scheduler.autotune.window_days must be between 1 and 90".into(),
            ));
        }
        if self.min_runs == 0 {
            return Err(Error::Config(
                "scheduler.autotune.min_runs must be positive".into(),
            ));
        }
        if !self.min_improvement_pct.is_finite() || self.min_improvement_pct < 0.0 {
            return Err(Error::Config(
                "scheduler.autotune.min_improvement_pct must be finite and non-negative".into(),
            ));
        }
        if self.rollback_min_runs == 0 {
            return Err(Error::Config(
                "scheduler.autotune.rollback_min_runs must be positive".into(),
            ));
        }
        if !self.rollback_orphan_delta.is_finite()
            || !(0.0..=1.0).contains(&self.rollback_orphan_delta)
        {
            return Err(Error::Config(
                "scheduler.autotune.rollback_orphan_delta must be between 0.0 and 1.0".into(),
            ));
        }
        if !self.rollback_p90_ratio.is_finite() || self.rollback_p90_ratio <= 1.0 {
            return Err(Error::Config(
                "scheduler.autotune.rollback_p90_ratio must be finite and greater than 1.0".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn interval_ms(&self) -> i64 {
        secs_ms(self.interval_secs)
    }

    pub(crate) fn cooldown_ms(&self) -> i64 {
        secs_ms(self.cooldown_secs)
    }
}

/// Telegram notifier settings. The token is a secret: it is read from
/// `BOARD_TELEGRAM_TOKEN` when set, otherwise from `bot_token`, and never logged.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    #[serde(default)]
    pub bot_token: Option<String>,
    /// The only chat that receives asks and whose answers are accepted. When absent, the
    /// chat paired with a `board admin telegram-pair` code is used.
    #[serde(default)]
    pub chat_id: Option<i64>,
    /// Bot API base URL; tests point it at a local fake.
    #[serde(default = "default_telegram_api")]
    pub api_base: String,
    /// A task in doing or review with no event from an agent or the user for this long is
    /// reported once as stuck, and again only after it changes and goes quiet again. 0
    /// turns the alert off. Default 4 hours.
    #[serde(default = "default_stuck_after")]
    pub stuck_after_secs: u64,
    /// An active autopilot board with ready auto work, no task in doing, and no task moved
    /// into or out of doing for this long is reported once, and again only after such a
    /// move. 0 turns the alert off. Default 2 hours.
    #[serde(default = "default_stalled_after")]
    pub stalled_after_secs: u64,
    /// Local time of the daily Needs you digest (`HH:MM`, at `utc_offset`); `None`
    /// (`"off"` in the file) sends none. Default 09:00.
    #[serde(default = "default_digest_at", deserialize_with = "digest_time")]
    pub digest_at: Option<NaiveTime>,
    /// Offset of `digest_at` from UTC, such as `+08:00`; fixed, so daylight saving time
    /// moves the digest by an hour. Default `+00:00`.
    #[serde(default = "utc", deserialize_with = "utc_offset")]
    pub utc_offset: FixedOffset,
}

fn default_stuck_after() -> u64 {
    4 * 3600
}

fn default_stalled_after() -> u64 {
    2 * 3600
}

fn default_digest_at() -> Option<NaiveTime> {
    NaiveTime::from_hms_opt(9, 0, 0)
}

fn utc() -> FixedOffset {
    FixedOffset::east_opt(0).expect("zero is a valid offset")
}

fn digest_time<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NaiveTime>, D::Error> {
    let text = String::deserialize(d)?;
    if text == "off" {
        return Ok(None);
    }
    NaiveTime::parse_from_str(&text, "%H:%M")
        .map(Some)
        .map_err(|_| {
            serde::de::Error::custom(format!("digest_at: `{text}` is not HH:MM or \"off\""))
        })
}

fn utc_offset<'de, D: Deserializer<'de>>(d: D) -> Result<FixedOffset, D::Error> {
    let text = String::deserialize(d)?;
    text.parse().map_err(|_| {
        serde::de::Error::custom(format!(
            "utc_offset: `{text}` is not an offset such as \"+08:00\""
        ))
    })
}

fn default_telegram_api() -> String {
    "https://api.telegram.org".to_owned()
}

impl Default for TelegramConfig {
    /// No token and no chat: the chat is paired.
    fn default() -> Self {
        Self {
            bot_token: None,
            chat_id: None,
            api_base: default_telegram_api(),
            stuck_after_secs: default_stuck_after(),
            stalled_after_secs: default_stalled_after(),
            digest_at: default_digest_at(),
            utc_offset: utc(),
        }
    }
}

impl TelegramConfig {
    /// Settings with a configured chat.
    pub fn new(chat_id: i64) -> Self {
        Self {
            chat_id: Some(chat_id),
            ..Self::default()
        }
    }

    /// The bot token: `BOARD_TELEGRAM_TOKEN` if set and not empty, else `bot_token`.
    pub fn token(&self) -> Option<String> {
        std::env::var(TELEGRAM_TOKEN_ENV)
            .ok()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| self.bot_token.clone().filter(|t| !t.trim().is_empty()))
    }
}

impl fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("bot_token", &self.bot_token.as_ref().map(|_| "<redacted>"))
            .field("chat_id", &self.chat_id)
            .field("api_base", &self.api_base)
            .field("stuck_after_secs", &self.stuck_after_secs)
            .field("stalled_after_secs", &self.stalled_after_secs)
            .field("digest_at", &self.digest_at)
            .field("utc_offset", &self.utc_offset)
            .finish()
    }
}

/// Rule and rendering defaults (`docs/API.md`, "Defaults"); every one can be overridden in
/// the `[defaults]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// An owner unseen this long is stale and its tasks may be taken.
    pub owner_stale_after_secs: u64,
    /// File lease TTL when the request names none; heartbeats renew to at least this.
    pub lease_ttl_secs: u64,
    /// Coordinator lease TTL, renewed by the daemon and the coordinator's heartbeats.
    pub coordinator_ttl_secs: u64,
    /// Touches and leases this recent count as overlapping.
    pub touch_window_secs: u64,
    /// Tasks one agent may hold in `doing` at once.
    pub max_doing_per_agent: u32,
    /// Done tasks older than this are left out of snapshots.
    pub done_hidden_after_days: u32,
    /// A temporary board idle this long (no event on it, no open ask, no live owner of a
    /// task in `doing`) is closed. A paused one is never closed.
    pub temporary_close_after_secs: u64,
    /// A closed temporary board idle this long is deleted.
    pub temporary_delete_after_secs: u64,
    pub snapshot_full_chars: usize,
    pub snapshot_delta_chars: usize,
    pub snapshot_alerts_chars: usize,
    /// Ready tasks listed in a full snapshot.
    pub snapshot_ready_items: usize,
    /// Recent events listed in a full snapshot.
    pub snapshot_recent_items: usize,
    /// Peers listed in a full snapshot.
    pub snapshot_peer_items: usize,
    pub title_chars: usize,
    pub note_chars: usize,
    /// Handoff `next` and `blockers`.
    pub handoff_field_chars: usize,
    /// Handoff `now`, which says where the work stands and runs longer than `next`; never
    /// less than `handoff_field_chars` ([`Self::handoff_now_max`]).
    pub handoff_now_chars: usize,
    pub question_chars: usize,
    pub body_chars: usize,
    /// Board that feedback items are filed on; created on first use.
    pub feedback_board: BoardSlug,
    /// API errors with one signature file feedback once they reach this many...
    pub signal_min_reports: u32,
    /// ...from at least this many agents...
    pub signal_min_agents: u32,
    /// ...within this many days.
    pub signal_window_days: u32,
    /// API error records are kept this many days, for revisits, and never less than
    /// `signal_window_days`.
    pub error_retention_days: u32,
    /// A declined feedback item is reopened once it was declined at least this many days
    /// ago...
    pub declined_reopen_after_days: u32,
    /// ...and has gained at least this many reports since the decline...
    pub declined_reopen_reports: u32,
    /// ...from at least this many agents that had not reported it before the decline.
    pub declined_reopen_agents: u32,
    /// Creating an ask warns of the board's other open asks younger than this.
    pub duplicate_ask_window_secs: u64,
    /// A task in review this long with no note from its owner raises one alert.
    pub review_quiet_secs: u64,
    /// A task in doing whose owner has been away this long, with no thread it delegated
    /// still at work, is announced once per spell away.
    pub doing_idle_secs: u64,
    /// A session still marked `running` and unseen this long stopped without an end hook:
    /// the sweeper marks it `ended`, and on a board nobody runs an agent on its
    /// repositories may drop or close its tasks.
    pub owner_left_secs: u64,
    /// A device whose latest heartbeat is older than this counts as `stale` in
    /// `GET /v1/devices`.
    pub device_stale_secs: u64,
    /// Default lease TTL for managed runs before the sweeper marks them `orphaned`.
    pub run_lease_ttl_secs: u64,
    /// Grace period before pending sticky wakes and `waiting_external` runs on a stale
    /// device fall back to the global pool.
    pub sticky_grace_secs: u64,
    /// Ready `auto` task matching zero online untainted devices for this long fires
    /// `signal:scheduler:unschedulable` on `feedback_board`.
    pub unschedulable_after_secs: u64,
    /// A board's ready `auto` task waiting this long while other boards claim runs fires
    /// `signal:scheduler:starvation` on `feedback_board`.
    pub starvation_after_secs: u64,
    /// Orphaned runs on one device within a rolling 1-hour window that fire
    /// `signal:scheduler:orphan_storm`.
    pub orphan_storm_count: u32,
    /// Percentage of active hours held by one `conflict_key` (with at least 3 holds) that
    /// fires `signal:scheduler:conflict_hot`.
    pub conflict_hot_percent: u32,
    /// Fast `failed` runs (`running_secs <= 30` or never started) on one `(device, runner)`
    /// within a rolling 1-hour window that fire `signal:runner:spawn_fail`.
    pub spawn_fail_count: u32,
    /// Batch-coalescing debounce window in seconds for `RunKind::Triage` and `RunKind::Verdict`
    /// when fewer than `judge_batch_min_size` non-P1 items are queued. `0` disables debounce.
    pub judge_batch_debounce_secs: u64,
    /// Minimum eligible items in a triage or verdict group that bypasses
    /// `judge_batch_debounce_secs` and dispatches immediately (clamped to `>= 1`).
    pub judge_batch_min_size: usize,
    /// Deploy-train quiet debounce window in seconds when sibling PRs on the same board are
    /// still in pre-merge `review` or active `merge`/`review`. `0` disables debounce.
    pub deploy_batch_debounce_secs: u64,
    /// Maximum time in seconds a merged deploy-eligible task waits in a deploy train before
    /// flushing even while sibling PRs remain in flight.
    pub deploy_batch_max_wait_secs: u64,
    /// Maximum merged tasks coalesced into one deploy train run (clamped to `>= 1`).
    pub deploy_batch_max_size: usize,
    /// Optional server-wide fallback cooldown in seconds between `RunKind::Goal` reviews on
    /// active project boards whose own `goal_review_every_secs` is unset (`None`).
    pub default_goal_review_every_secs: Option<u64>,
    /// Automated scheduler evaluation and self-tuning settings loaded from
    /// `[scheduler.autotune]` (not `[defaults]`).
    #[serde(skip)]
    pub scheduler_autotune: AutotuneConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            owner_stale_after_secs: 24 * 3600,
            lease_ttl_secs: 45 * 60,
            coordinator_ttl_secs: 45 * 60,
            touch_window_secs: 3600,
            max_doing_per_agent: 1,
            done_hidden_after_days: 30,
            temporary_close_after_secs: 24 * 3600,
            temporary_delete_after_secs: 7 * 24 * 3600,
            snapshot_full_chars: 2000,
            snapshot_delta_chars: 600,
            snapshot_alerts_chars: 400,
            snapshot_ready_items: 3,
            snapshot_recent_items: 3,
            snapshot_peer_items: 8,
            title_chars: 80,
            note_chars: 800,
            handoff_field_chars: 300,
            handoff_now_chars: 400,
            question_chars: 1500,
            body_chars: 4000,
            feedback_board: "board-feedback".parse().expect("a valid slug"),
            signal_min_reports: 3,
            signal_min_agents: 2,
            signal_window_days: 7,
            error_retention_days: 35,
            declined_reopen_after_days: 1,
            declined_reopen_reports: 5,
            declined_reopen_agents: 3,
            duplicate_ask_window_secs: 30 * 60,
            review_quiet_secs: 24 * 3600,
            doing_idle_secs: 30 * 60,
            owner_left_secs: 3600,
            device_stale_secs: board_proto::DEVICE_STALE_SECS_DEFAULT,
            run_lease_ttl_secs: board_proto::RUN_LEASE_TTL_SECS_DEFAULT,
            sticky_grace_secs: board_proto::STICKY_GRACE_SECS_DEFAULT,
            unschedulable_after_secs: 3600,
            starvation_after_secs: 6 * 3600,
            orphan_storm_count: 3,
            conflict_hot_percent: 30,
            spawn_fail_count: 3,
            judge_batch_debounce_secs: 90,
            judge_batch_min_size: 3,
            deploy_batch_debounce_secs: 300,
            deploy_batch_max_wait_secs: 1500,
            deploy_batch_max_size: 5,
            default_goal_review_every_secs: None,
            scheduler_autotune: AutotuneConfig::default(),
        }
    }
}

impl Settings {
    fn check(&self) -> Result<(), Error> {
        let positive = [
            ("owner_stale_after_secs", self.owner_stale_after_secs),
            ("lease_ttl_secs", self.lease_ttl_secs),
            ("coordinator_ttl_secs", self.coordinator_ttl_secs),
            ("touch_window_secs", self.touch_window_secs),
            ("max_doing_per_agent", u64::from(self.max_doing_per_agent)),
            (
                "temporary_close_after_secs",
                self.temporary_close_after_secs,
            ),
            (
                "temporary_delete_after_secs",
                self.temporary_delete_after_secs,
            ),
            ("title_chars", self.title_chars as u64),
            ("note_chars", self.note_chars as u64),
            ("handoff_field_chars", self.handoff_field_chars as u64),
            ("handoff_now_chars", self.handoff_now_chars as u64),
            ("question_chars", self.question_chars as u64),
            ("body_chars", self.body_chars as u64),
            ("signal_min_reports", u64::from(self.signal_min_reports)),
            ("signal_min_agents", u64::from(self.signal_min_agents)),
            ("signal_window_days", u64::from(self.signal_window_days)),
            ("error_retention_days", u64::from(self.error_retention_days)),
            (
                "declined_reopen_after_days",
                u64::from(self.declined_reopen_after_days),
            ),
            (
                "declined_reopen_reports",
                u64::from(self.declined_reopen_reports),
            ),
            (
                "declined_reopen_agents",
                u64::from(self.declined_reopen_agents),
            ),
            ("duplicate_ask_window_secs", self.duplicate_ask_window_secs),
            ("review_quiet_secs", self.review_quiet_secs),
            ("doing_idle_secs", self.doing_idle_secs),
            ("owner_left_secs", self.owner_left_secs),
            ("device_stale_secs", self.device_stale_secs),
            ("run_lease_ttl_secs", self.run_lease_ttl_secs),
            ("sticky_grace_secs", self.sticky_grace_secs),
            ("unschedulable_after_secs", self.unschedulable_after_secs),
            ("starvation_after_secs", self.starvation_after_secs),
            ("orphan_storm_count", u64::from(self.orphan_storm_count)),
            ("conflict_hot_percent", u64::from(self.conflict_hot_percent)),
            ("spawn_fail_count", u64::from(self.spawn_fail_count)),
        ];
        if let Some((name, _)) = positive.iter().find(|(_, v)| *v == 0) {
            return Err(Error::Config(format!("defaults.{name} must be positive")));
        }
        if self.snapshot_full_chars < 600 {
            return Err(Error::Config(
                "defaults.snapshot_full_chars must be at least 600".into(),
            ));
        }
        if self.snapshot_delta_chars < 200 || self.snapshot_alerts_chars < 200 {
            return Err(Error::Config(
                "defaults.snapshot_delta_chars and snapshot_alerts_chars must be at least 200"
                    .into(),
            ));
        }
        if self.conflict_hot_percent > 100 {
            return Err(Error::Config(
                "defaults.conflict_hot_percent must be at most 100".into(),
            ));
        }
        if let Some(secs) = self.default_goal_review_every_secs
            && secs != 0
            && !(60..=604_800).contains(&secs)
        {
            return Err(Error::Config(
                "defaults.default_goal_review_every_secs must be 0 or between 60 and 604800".into(),
            ));
        }
        if self.deploy_batch_debounce_secs > 0 && self.deploy_batch_max_wait_secs == 0 {
            return Err(Error::Config(
                "defaults.deploy_batch_max_wait_secs must be positive when deploy_batch_debounce_secs is enabled".into(),
            ));
        }
        Ok(())
    }

    /// The handoff `now` limit: `handoff_now_chars`, or `handoff_field_chars` when that is
    /// larger, so a config that raised the field limit before `now` had its own keeps it.
    pub fn handoff_now_max(&self) -> usize {
        self.handoff_now_chars.max(self.handoff_field_chars)
    }

    pub(crate) fn stale_ms(&self) -> i64 {
        secs_ms(self.owner_stale_after_secs)
    }

    pub(crate) fn lease_ms(&self) -> i64 {
        secs_ms(self.lease_ttl_secs)
    }

    pub(crate) fn coordinator_ms(&self) -> i64 {
        secs_ms(self.coordinator_ttl_secs)
    }

    pub(crate) fn temporary_close_ms(&self) -> i64 {
        secs_ms(self.temporary_close_after_secs)
    }

    pub(crate) fn temporary_delete_ms(&self) -> i64 {
        secs_ms(self.temporary_delete_after_secs)
    }

    pub(crate) fn duplicate_ask_ms(&self) -> i64 {
        secs_ms(self.duplicate_ask_window_secs)
    }

    pub(crate) fn review_quiet_ms(&self) -> i64 {
        secs_ms(self.review_quiet_secs)
    }

    pub(crate) fn doing_idle_ms(&self) -> i64 {
        secs_ms(self.doing_idle_secs)
    }

    pub(crate) fn owner_left_ms(&self) -> i64 {
        secs_ms(self.owner_left_secs)
    }

    pub(crate) fn device_stale_ms(&self) -> i64 {
        secs_ms(self.device_stale_secs)
    }

    pub(crate) fn run_lease_ms(&self) -> i64 {
        secs_ms(self.run_lease_ttl_secs)
    }

    pub(crate) fn sticky_grace_ms(&self) -> i64 {
        secs_ms(self.sticky_grace_secs)
    }

    pub(crate) fn unschedulable_after_ms(&self) -> i64 {
        secs_ms(self.unschedulable_after_secs)
    }

    pub(crate) fn starvation_after_ms(&self) -> i64 {
        secs_ms(self.starvation_after_secs)
    }

    pub(crate) fn deploy_batch_debounce_ms(&self) -> i64 {
        secs_ms(self.deploy_batch_debounce_secs)
    }

    pub(crate) fn deploy_batch_max_wait_ms(&self) -> i64 {
        secs_ms(self.deploy_batch_max_wait_secs)
    }

    pub(crate) fn deploy_batch_max_size_clamped(&self) -> usize {
        self.deploy_batch_max_size.max(1)
    }

    pub(crate) fn touch_ms(&self) -> i64 {
        secs_ms(self.touch_window_secs)
    }

    pub(crate) fn signal_window_ms(&self) -> i64 {
        secs_ms(u64::from(self.signal_window_days) * 86_400)
    }

    /// Days of API error records kept: the setting, and never less than the signal window.
    pub(crate) fn error_retention(&self) -> u32 {
        self.error_retention_days.max(self.signal_window_days)
    }

    pub(crate) fn error_retention_ms(&self) -> i64 {
        secs_ms(u64::from(self.error_retention()) * 86_400)
    }

    pub(crate) fn declined_reopen_ms(&self) -> i64 {
        secs_ms(u64::from(self.declined_reopen_after_days) * 86_400)
    }

    pub(crate) fn done_hidden_ms(&self) -> i64 {
        secs_ms(u64::from(self.done_hidden_after_days) * 86_400)
    }

    pub(crate) fn judge_batch_debounce_ms(&self) -> i64 {
        secs_ms(self.judge_batch_debounce_secs)
    }

    pub(crate) fn effective_judge_batch_min_size(&self) -> usize {
        self.judge_batch_min_size.max(1)
    }
}

fn secs_ms(secs: u64) -> i64 {
    i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_and_full_configs() {
        let minimal = ServerConfig::from_toml("db = \"board.db\"").unwrap();
        assert_eq!(minimal.listen.to_string(), "127.0.0.1:7377");
        assert!(minimal.trust_identity_header);
        assert_eq!(minimal.defaults, Settings::default());

        let full = ServerConfig::from_toml(
            r#"
            db = "b.db"
            listen = "127.0.0.1:9000"
            users = ["user@example.com"]
            trust_identity_header = false
            public_url = "https://board.example.ts.net"
            [telegram]
            bot_token = "secret-token"
            chat_id = -100
            [defaults]
            owner_stale_after_secs = 60
            snapshot_full_chars = 1500
            feedback_board = "setup-feedback"
            "#,
        )
        .unwrap();
        assert_eq!(full.defaults.owner_stale_after_secs, 60);
        assert_eq!(full.defaults.snapshot_full_chars, 1500);
        assert_eq!(full.defaults.feedback_board.as_str(), "setup-feedback");
        assert_eq!(minimal.defaults.feedback_board.as_str(), "board-feedback");
        assert_eq!(
            full.defaults.lease_ttl_secs,
            Settings::default().lease_ttl_secs
        );
        assert_eq!(full.telegram.as_ref().unwrap().chat_id, Some(-100));
        let paired = ServerConfig::from_toml("db = \"b\"\n[telegram]\n").unwrap();
        let paired = paired.telegram.unwrap();
        assert_eq!(paired.chat_id, None);
        assert_eq!(paired.stuck_after_secs, 4 * 3600);
        assert_eq!(paired.stalled_after_secs, 2 * 3600);
        assert_eq!(paired.digest_at, NaiveTime::from_hms_opt(9, 0, 0));
        assert_eq!(paired.utc_offset, FixedOffset::east_opt(0).unwrap());
        let alerts = ServerConfig::from_toml(
            "db = \"b\"\n[telegram]\nstuck_after_secs = 0\nstalled_after_secs = 0\n\
             digest_at = \"off\"\nutc_offset = \"+08:00\"",
        )
        .unwrap()
        .telegram
        .unwrap();
        assert_eq!(
            (
                alerts.stuck_after_secs,
                alerts.stalled_after_secs,
                alerts.digest_at,
                alerts.utc_offset
            ),
            (0, 0, None, FixedOffset::east_opt(8 * 3600).unwrap())
        );
        let at = ServerConfig::from_toml("db = \"b\"\n[telegram]\ndigest_at = \"18:30\"").unwrap();
        assert_eq!(
            at.telegram.unwrap().digest_at,
            NaiveTime::from_hms_opt(18, 30, 0)
        );
        assert!(!format!("{full:?}").contains("secret-token"));
    }

    #[test]
    fn rejects_unknown_and_bad_values() {
        for bad in [
            "digest_at = \"9am\"",
            "digest_at = \"25:00\"",
            "utc_offset = \"Europe/London\"",
            "stuck_after_secs = -1",
        ] {
            assert!(
                ServerConfig::from_toml(&format!("db = \"b\"\n[telegram]\n{bad}")).is_err(),
                "{bad}"
            );
        }
        assert!(ServerConfig::from_toml("db = \"b\"\nlisten_addr = \"x\"").is_err());
        assert!(
            ServerConfig::from_toml("db = \"b\"\n[defaults]\nmax_doing_per_agent = 0").is_err()
        );
        assert!(ServerConfig::from_toml("db = \"b\"\npublic_url = \"board\"").is_err());
        assert!(
            ServerConfig::from_toml("db = \"b\"\n[defaults]\nfeedback_board = \"Bad Slug\"")
                .is_err()
        );
        for name in [
            "temporary_close_after_secs",
            "temporary_delete_after_secs",
            "signal_min_reports",
            "signal_min_agents",
            "signal_window_days",
            "error_retention_days",
            "declined_reopen_after_days",
            "declined_reopen_reports",
            "declined_reopen_agents",
            "duplicate_ask_window_secs",
            "handoff_now_chars",
        ] {
            assert!(
                ServerConfig::from_toml(&format!("db = \"b\"\n[defaults]\n{name} = 0")).is_err(),
                "{name}"
            );
            let set =
                ServerConfig::from_toml(&format!("db = \"b\"\n[defaults]\n{name} = 60")).unwrap();
            assert_ne!(set.defaults, Settings::default(), "{name}");
        }
    }

    #[test]
    fn handoff_now_takes_the_larger_of_its_limit_and_the_field_limit() {
        assert_eq!(Settings::default().handoff_now_max(), 400);
        let raised =
            ServerConfig::from_toml("db = \"b\"\n[defaults]\nhandoff_field_chars = 600").unwrap();
        assert_eq!(raised.defaults.handoff_now_max(), 600);
        let own = ServerConfig::from_toml(
            "db = \"b\"\n[defaults]\nhandoff_field_chars = 200\nhandoff_now_chars = 500",
        )
        .unwrap();
        assert_eq!(own.defaults.handoff_now_max(), 500);
    }

    #[test]
    fn parses_and_validates_scheduler_autotune_config() {
        let minimal = ServerConfig::from_toml("db = \"board.db\"").unwrap();
        assert_eq!(minimal.scheduler.autotune, AutotuneConfig::default());
        assert_eq!(
            minimal.defaults.scheduler_autotune,
            AutotuneConfig::default()
        );
        assert!(minimal.scheduler.autotune.enabled);
        assert_eq!(minimal.scheduler.autotune.mode, AutotuneMode::Apply);
        assert_eq!(minimal.scheduler.autotune.interval_secs, 21_600);
        assert_eq!(minimal.scheduler.autotune.window_days, 7);

        let custom = ServerConfig::from_toml(
            r#"
            db = "board.db"
            [scheduler.autotune]
            enabled = false
            mode = "propose"
            interval_secs = 3600
            window_days = 14
            min_runs = 50
            min_improvement_pct = 7.5
            cooldown_secs = 43200
            rollback_min_runs = 15
            rollback_orphan_delta = 0.08
            rollback_p90_ratio = 1.4
            "#,
        )
        .unwrap();
        assert!(!custom.scheduler.autotune.enabled);
        assert_eq!(custom.scheduler.autotune.mode, AutotuneMode::Propose);
        assert_eq!(custom.scheduler.autotune.interval_secs, 3600);
        assert_eq!(custom.scheduler.autotune.window_days, 14);
        assert_eq!(custom.scheduler.autotune.min_runs, 50);

        for bad in [
            "mode = \"invalid_mode\"",
            "interval_secs = 30",
            "window_days = 0",
            "window_days = 91",
            "min_runs = 0",
            "min_improvement_pct = -1.0",
            "rollback_min_runs = 0",
            "rollback_orphan_delta = 1.5",
            "rollback_p90_ratio = 1.0",
            "unknown_key = true",
        ] {
            assert!(
                ServerConfig::from_toml(&format!("db = \"b\"\n[scheduler.autotune]\n{bad}"))
                    .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn parses_judge_batch_debounce_defaults_and_clamps_min_size() {
        let minimal = ServerConfig::from_toml("db = \"board.db\"").unwrap();
        assert_eq!(minimal.defaults.judge_batch_debounce_secs, 90);
        assert_eq!(minimal.defaults.judge_batch_min_size, 3);
        assert_eq!(minimal.defaults.judge_batch_debounce_ms(), 90_000);
        assert_eq!(minimal.defaults.effective_judge_batch_min_size(), 3);

        let custom = ServerConfig::from_toml(
            r#"
            db = "board.db"
            [defaults]
            judge_batch_debounce_secs = 0
            judge_batch_min_size = 0
            "#,
        )
        .unwrap();
        assert_eq!(custom.defaults.judge_batch_debounce_secs, 0);
        assert_eq!(custom.defaults.judge_batch_debounce_ms(), 0);
        assert_eq!(custom.defaults.effective_judge_batch_min_size(), 1);
    }
}
