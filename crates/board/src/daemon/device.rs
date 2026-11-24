//! The device-wide limit on the runs the dispatcher has at once (`docs/DESIGN.md`, "Daemon
//! dispatch", Device limit).
//!
//! A board's own limits (`parallel_tasks` workers, [`super::judges::JUDGES_AT_ONCE`]
//! judges) say nothing of the device: each run is an agent session of some hundred MiB,
//! and its builds take more, so the boards a device dispatches together must share one
//! limit. Workers and judges count alike. One slot is kept for judges, since a judge ends
//! work already in progress, and a board that was refused a slot gets the next free one
//! before a board that asked later ([`Turns`]).
//!
//! The device's memory is taken by every agent turn that runs on it, not only by the
//! dispatcher's runs, so the turns of the other threads count too ([`running_threads`]).
//! The dispatcher yields to them: it never stops a thread, and it starts none of its own
//! while the device is full.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use board_proto::{
    BoardSlug, DeviceCapacity, JudgeKind, ResourceWeight, Task, Timestamp, normalize_repo,
};
use serde::Serialize;
use tokio::time::Instant;

use crate::config::Config;
use crate::t3::{T3, T3Cli};

/// Default weighted resource units for a light run (verdict/triage/goal/merge/housekeeping).
pub const LIGHT_RUN_UNITS: u32 = 1;
/// Default weighted resource units for a standard run (ordinary worker, review, interactive).
pub const STANDARD_RUN_UNITS: u32 = 2;
/// Default weighted resource units for a heavy run (heavy workspace build, ML/data crunching).
pub const HEAVY_RUN_UNITS: u32 = 4;
/// Default protocol features advertised in device heartbeats when not overridden in config.
pub const DEFAULT_PROTOCOL_FEATURES: &[&str] = &["v1-json", "sse-v1"];

/// Environment variable T3 Code sets to its own `t3` CLI binary.
pub const T3_CLI_ENV: &str = "T3CODE_CLI_PATH";

/// Host memory that allows one run, when `runs_at_once` is not set.
pub const MEMORY_PER_RUN: u64 = 4 << 30;

/// The least default limit: one worker and one judge.
pub const MIN_RUNS: usize = 2;

/// The limit of a device whose memory could not be read.
pub const FALLBACK_RUNS: usize = 4;

/// Device turns the daemon leaves free by default for threads it did not start.
pub const DEFAULT_RESERVE: usize = 1;

/// How many of the other threads' ids a record of a hold names; the rest are counted.
pub const THREADS_SHOWN: usize = 6;

/// How long T3 has to list the running threads for one check.
pub const COUNT_BUDGET: Duration = Duration::from_millis(4500);

/// Default timeout for `board device wait`.
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum timeout accepted by `board device wait` (24 hours).
pub const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Default polling interval for `board device wait`.
pub const DEFAULT_WAIT_POLL: Duration = Duration::from_secs(5);

/// Debug builds only: overrides [`DEFAULT_WAIT_POLL`] in milliseconds so integration tests
/// do not sleep for seconds. Release builds ignore it.
pub const WAIT_POLL_ENV: &str = "BOARD_DEVICE_WAIT_POLL_MS";

/// The limit a device with `host_memory` bytes gets when `runs_at_once` is not set. The
/// memory is rounded to the nearest run: a host reports somewhat less than it has.
pub fn default_run_limit(host_memory: Option<u64>) -> usize {
    host_memory.map_or(FALLBACK_RUNS, |bytes| {
        usize::try_from(bytes.saturating_add(MEMORY_PER_RUN / 2) / MEMORY_PER_RUN)
            .unwrap_or(usize::MAX)
            .max(MIN_RUNS)
    })
}

/// Device turns the daemon leaves free for launches from threads it did not start:
/// `reserve_runs` (or alias `reserve`) of `config`, else [`DEFAULT_RESERVE`].
pub fn reserve(config: &Config) -> usize {
    config.reserve_runs.map_or(DEFAULT_RESERVE, |n| n as usize)
}

/// How many of `limit` device runs the daemon may take, leaving `reserve` turns free for
/// threads it did not start. An unlimited device (`usize::MAX`) stays unlimited.
pub fn daemon_limit(limit: usize, reserve: usize) -> usize {
    if limit == usize::MAX {
        usize::MAX
    } else {
        limit.saturating_sub(reserve)
    }
}

/// How many of `limit` daemon runs workers may take: all but the judges' slot, or 0 when
/// the daemon has no runs to start.
pub fn worker_limit(limit: usize) -> usize {
    if limit == 0 {
        0
    } else {
        limit.saturating_sub(1).max(1)
    }
}

/// The memory of this host in bytes, when it can be read.
pub fn host_memory() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8(out.stdout).ok()?.trim().parse().ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        mem_total(&std::fs::read_to_string("/proc/meminfo").ok()?)
    }
}

/// The `MemTotal` of a `/proc/meminfo` text, in bytes.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    kib.checked_mul(1024)
}

/// The limit of this device: `runs_at_once` of `config`, else `default`, else none.
pub fn run_limit(config: &Config, default: Option<usize>) -> usize {
    config
        .runs_at_once
        .map_or_else(|| default.unwrap_or(usize::MAX), |n| n.get() as usize)
}

/// The effective agent-turn limit of this device: `runs_at_once` of `config`, else the
/// host memory default ([`default_run_limit`]). Shared by the launch hook and
/// `board device wait`.
pub fn effective_run_limit(config: &Config) -> usize {
    if config.runs_at_once.is_some() {
        run_limit(config, None)
    } else {
        run_limit(config, Some(default_run_limit(host_memory())))
    }
}

/// Whether `asked` additional agent turns fit under `limit` alongside `running` turns.
pub fn fits(running: usize, asked: usize, limit: usize) -> bool {
    running.saturating_add(asked) <= limit
}

/// Weighted resource units consumed by a task's worker run: explicit `resource_units`
/// (when positive), else `weight` (`light=1`, `standard=2`, `heavy=4`), else
/// [`STANDARD_RUN_UNITS`] (`2`).
pub fn cost_units_for_task(task: &Task) -> u32 {
    if let Some(units) = task.resource_units.filter(|&u| u > 0) {
        return units;
    }
    task.weight
        .and_then(ResourceWeight::default_units)
        .unwrap_or(STANDARD_RUN_UNITS)
}

/// Weighted resource units consumed by a judge run: `review` is standard (`2`), while
/// `verdict`, `triage`, and `goal` are light (`1`).
pub fn cost_units_for_judge(kind: JudgeKind) -> u32 {
    match kind {
        JudgeKind::Review => STANDARD_RUN_UNITS,
        JudgeKind::Verdict | JudgeKind::Triage | JudgeKind::Goal => LIGHT_RUN_UNITS,
    }
}

/// Default weighted capacity units derived from host memory: `default_run_limit(mem) * 2`
/// (so each 4 GiB run slot equals one standard 2-unit run, minimum 4 units).
pub fn default_weight_units(host_memory: Option<u64>) -> u32 {
    u32::try_from(default_run_limit(host_memory))
        .unwrap_or(u32::MAX)
        .saturating_mul(STANDARD_RUN_UNITS)
}

/// Effective total weighted capacity units of this device:
/// 1. explicit `config.weight_units`, else
/// 2. `config.runs_at_once * STANDARD_RUN_UNITS`, else
/// 3. `default_run_limit * STANDARD_RUN_UNITS` (when finite), else
/// 4. [`default_weight_units`] from `host_memory`.
pub fn effective_weight_units(
    config: &Config,
    default_limit: Option<usize>,
    host_memory: Option<u64>,
) -> u32 {
    if let Some(units) = config.weight_units {
        return units;
    }
    if let Some(runs) = config.runs_at_once {
        return runs.get().saturating_mul(STANDARD_RUN_UNITS);
    }
    if let Some(limit) = default_limit.filter(|&l| l != usize::MAX) {
        return u32::try_from(limit)
            .unwrap_or(u32::MAX)
            .saturating_mul(STANDARD_RUN_UNITS);
    }
    default_weight_units(host_memory)
}

/// Weighted units reserved for interactive / non-daemon turns (`reserve(config) * 2`).
pub fn reserve_units(config: &Config) -> u32 {
    u32::try_from(reserve(config))
        .unwrap_or(u32::MAX)
        .saturating_mul(STANDARD_RUN_UNITS)
}

/// Normalized host operating system tag (`macos`, `linux`, ...).
pub fn host_os() -> &'static str {
    std::env::consts::OS
}

/// Normalized host CPU architecture tag (`arm64`, `x86_64`, ...).
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    }
}

/// Host memory rounded to the nearest GiB (at least 1 GiB when readable).
pub fn host_mem_gib(host_memory: Option<u64>) -> Option<u32> {
    host_memory.map(|bytes| {
        let gib = bytes.saturating_add(1 << 29) >> 30;
        u32::try_from(gib).unwrap_or(u32::MAX).max(1)
    })
}

/// Logical CPU core count when readable from the OS.
pub fn host_cpu_cores() -> Option<u32> {
    std::thread::available_parallelism()
        .ok()
        .and_then(|n| u32::try_from(n.get()).ok())
}

/// Hardware and weighted scheduling capacity for `PUT /v1/devices/{name}/heartbeat`.
pub fn device_capacity(
    config: &Config,
    default_limit: Option<usize>,
    host_memory: Option<u64>,
) -> DeviceCapacity {
    DeviceCapacity {
        weight_units: effective_weight_units(config, default_limit, host_memory),
        mem_gib: host_mem_gib(host_memory),
        cpu_cores: host_cpu_cores(),
    }
}

/// Advertised device capability tags: auto-detected `os:<os>` and `arch:<arch>` plus any
/// non-empty user-configured `config.capabilities` (deduplicated preserving order).
pub fn effective_capabilities(config: &Config) -> Vec<String> {
    let mut out = vec![format!("os:{}", host_os()), format!("arch:{}", host_arch())];
    for cap in &config.capabilities {
        let trimmed = cap.trim();
        if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_owned());
        }
    }
    out
}

/// Advertised runner kinds: configured `config.runners` when non-empty, else `[active_runner]`.
pub fn effective_runners(config: &Config, active_runner: &str) -> Vec<String> {
    let mut out = Vec::new();
    for runner in &config.runners {
        let trimmed = runner.trim();
        if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_owned());
        }
    }
    if out.is_empty() {
        out.push(active_runner.to_owned());
    }
    out
}

/// Advertised protocol features: configured `config.protocol_features` when non-empty, else
/// [`DEFAULT_PROTOCOL_FEATURES`].
pub fn effective_protocol_features(config: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for feat in &config.protocol_features {
        let trimmed = feat.trim();
        if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_owned());
        }
    }
    if out.is_empty() {
        out.extend(DEFAULT_PROTOCOL_FEATURES.iter().map(|s| (*s).to_owned()));
    }
    out
}

/// Normalized, deduplicated repository keys configured under `[projects]` in `config.toml`.
/// Keys that cannot be normalized by [`board_proto::normalize_repo`] are skipped so
/// `PUT /v1/devices/{name}/heartbeat` is never rejected with HTTP 400.
pub fn configured_repos(config: &Config) -> Vec<String> {
    let mut repos = BTreeSet::new();
    for key in config.projects.keys() {
        if let Some(norm) = normalize_repo(key) {
            repos.insert(norm);
        } else {
            tracing::debug!(
                "skipping non-normalizable [projects] key {key:?} in device heartbeat repos"
            );
        }
    }
    repos.into_iter().collect()
}

/// The `t3` binary this thread's T3 names (`T3CODE_CLI_PATH`), else `config.t3_bin`, else
/// the default location.
pub fn t3_cli(config: &Config) -> T3Cli {
    let bin = std::env::var_os(T3_CLI_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| config.t3_bin.clone());
    T3Cli::locate(bin.as_deref())
}

/// Options for [`wait_for_slot`].
#[derive(Debug, Clone, Copy)]
pub struct WaitOptions {
    pub slots: usize,
    pub timeout: Duration,
    pub poll: Duration,
    pub count_budget: Duration,
}

/// Snapshot reported when [`wait_for_slot`] observes enough free agent-turn slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaitReady {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    pub running: usize,
    pub limit: usize,
    pub slots: usize,
    pub free: usize,
    pub waited_secs: u64,
}

impl WaitReady {
    /// Human-readable summary of the available device capacity.
    pub fn summary(&self) -> String {
        let where_ = self
            .device
            .as_deref()
            .map_or_else(|| "this device".to_owned(), |d| format!("device {d}"));
        let noun = if self.free == 1 { "slot" } else { "slots" };
        format!(
            "{where_} has {} free agent-turn {noun} ({} of {} running; waited {}s)",
            self.free, self.running, self.limit, self.waited_secs
        )
    }
}

/// Why [`wait_for_slot`] exited without finding enough free slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitError {
    /// `slots` is 0 or exceeds the device's `limit`, so it can never fit.
    Impossible { slots: usize, limit: usize },
    /// The timeout elapsed before `slots` free agent-turn slots were observed.
    TimedOut {
        device: Option<String>,
        timeout: Duration,
        slots: usize,
        running: usize,
        limit: usize,
        unknown: Option<String>,
    },
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Impossible { slots, limit } => {
                let noun = if *slots == 1 { "slot" } else { "slots" };
                if *slots > *limit && *limit > 0 {
                    write!(
                        f,
                        "asked for {slots} {noun}, which exceeds this device's limit of {limit} \
                         agent turns; pass --slots {limit} or fewer, or do the work in this thread"
                    )
                } else {
                    write!(
                        f,
                        "asked for {slots} {noun} on a device with a limit of {limit} agent \
                         turns; pass --slots 1 or more, or do the work in this thread"
                    )
                }
            }
            Self::TimedOut {
                device,
                timeout,
                slots,
                running,
                limit,
                unknown,
            } => {
                let where_ = device
                    .as_deref()
                    .map_or_else(|| "this device".to_owned(), |d| format!("device {d}"));
                let noun = if *slots == 1 { "slot" } else { "slots" };
                let secs = timeout.as_secs();
                match unknown {
                    Some(cause) if *running == 0 => write!(
                        f,
                        "timed out after {secs}s waiting for {slots} free agent-turn {noun}: \
                         could not count agent turns on {where_}: {cause}; check that T3 is \
                         running, or do the work in this thread"
                    ),
                    Some(cause) => write!(
                        f,
                        "timed out after {secs}s waiting for {slots} free agent-turn {noun}: \
                         {where_} has at least {running} of {limit} agent turns running; {cause}; \
                         check that T3 is running, run `board device wait` again with a longer \
                         --timeout, or do the work in this thread"
                    ),
                    None => write!(
                        f,
                        "timed out after {secs}s waiting for {slots} free agent-turn {noun}: \
                         {where_} still has {running} of {limit} agent turns running; run \
                         `board device wait` again with a longer --timeout, or do the work in \
                         this thread"
                    ),
                }
            }
        }
    }
}

impl std::error::Error for WaitError {}

/// Polls T3 until the device has at least `options.slots` free agent-turn slots (counted
/// the same way as the launch hook: `running + slots <= limit` with every configured
/// project listed), or returns [`WaitError::TimedOut`] at `options.timeout`.
pub async fn wait_for_slot<T: T3>(
    t3: &T,
    config: &Config,
    limit: usize,
    options: WaitOptions,
) -> Result<WaitReady, WaitError> {
    if options.slots == 0 || options.slots > limit {
        return Err(WaitError::Impossible {
            slots: options.slots,
            limit,
        });
    }
    let started = Instant::now();
    let deadline = started.checked_add(options.timeout);
    let poll = if options.timeout.is_zero() {
        options.poll
    } else {
        options.poll.min(options.timeout / 2)
    }
    .max(Duration::from_millis(1));
    let device = config.device.as_ref().map(|d| d.as_str().to_owned());
    let mut last_running: usize = 0;
    let mut last_unknown: Option<String> = None;

    loop {
        let budget = if options.timeout.is_zero() {
            options.count_budget
        } else {
            let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if remaining == Some(Duration::ZERO) {
                return Err(WaitError::TimedOut {
                    device,
                    timeout: options.timeout,
                    slots: options.slots,
                    running: last_running,
                    limit,
                    unknown: last_unknown,
                });
            }
            remaining.map_or(options.count_budget, |r| options.count_budget.min(r))
        };
        match tokio::time::timeout(budget, running_threads(t3, config)).await {
            Ok(running) => {
                let count = running.threads.len();
                last_running = count;
                last_unknown = running.unknown();
                if last_unknown.is_none() && fits(count, options.slots, limit) {
                    return Ok(WaitReady {
                        device,
                        running: count,
                        limit,
                        slots: options.slots,
                        free: limit.saturating_sub(count),
                        waited_secs: started.elapsed().as_secs(),
                    });
                }
            }
            Err(_) => {
                last_running = 0;
                last_unknown = Some(format!("T3 listed no threads within {budget:?}"));
            }
        }
        if options.timeout.is_zero() {
            return Err(WaitError::TimedOut {
                device,
                timeout: options.timeout,
                slots: options.slots,
                running: last_running,
                limit,
                unknown: last_unknown,
            });
        }
        let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if remaining == Some(Duration::ZERO) {
            return Err(WaitError::TimedOut {
                device,
                timeout: options.timeout,
                slots: options.slots,
                running: last_running,
                limit,
                unknown: last_unknown,
            });
        }
        let sleep_for = remaining.map_or(poll, |r| poll.min(r));
        tokio::time::sleep(sleep_for).await;
    }
}

/// What T3 said of the turns that run on the device.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Running {
    /// The threads with a run in progress, in the projects T3 listed.
    pub threads: BTreeSet<String>,
    /// The projects T3 could not list, each with the error. While one is here the count
    /// is a lower bound, and the device's load is unknown.
    pub unread: Vec<String>,
}

/// How much of the first unlisted project and its error [`Running::unknown`] quotes; a
/// record's `unread` list has every project in full.
pub const UNREAD_CHARS: usize = 240;

impl Running {
    /// Why the count is not whole, in words short enough for a note; `None` when T3 listed
    /// every project. It names the first project with its error, counts the rest, and says
    /// what ends it.
    pub fn unknown(&self) -> Option<String> {
        let first = self.unread.first()?;
        let mut text = String::from("T3 could not list the running threads of ");
        if first.chars().count() > UNREAD_CHARS {
            text.extend(first.chars().take(UNREAD_CHARS));
            text.push('…');
        } else {
            text.push_str(first);
        }
        match self.unread.len() - 1 {
            0 => {}
            more => text.push_str(&format!(" and of {more} more projects")),
        }
        text.push_str(
            " (start T3, or take the project T3 fails for out of config.toml, to end this)",
        );
        Some(text)
    }
}

/// The threads with a run in progress in the T3 projects of `config`, whoever started
/// them: the user, an agent or the dispatcher. These are the turns that take the device's
/// memory now. A project T3 cannot list adds no thread and is named in
/// [`Running::unread`]: its caller decides what an unknown load means.
pub async fn running_threads<T: T3>(t3: &T, config: &Config) -> Running {
    let projects: BTreeSet<&Path> = config
        .projects
        .values()
        .chain(config.board_projects.values())
        .map(std::path::PathBuf::as_path)
        .collect();
    let lists = projects.iter().map(|project| t3.running_threads(project));
    let lists = futures_util::future::join_all(lists).await;
    let mut running = Running::default();
    for (project, list) in projects.iter().zip(lists) {
        match list {
            Ok(threads) => running.threads.extend(threads),
            Err(err) => running
                .unread
                .push(format!("{} ({err:#})", project.display())),
        }
    }
    running
}

/// The boards that were refused a slot of one kind, oldest first served. Kept in memory:
/// after a restart the boards ask again in the order they are read.
#[derive(Debug, Default)]
pub struct Turns {
    waiting: BTreeMap<BoardSlug, Waiting>,
}

#[derive(Debug, Clone, Copy)]
struct Waiting {
    since: Timestamp,
    asked: Timestamp,
}

impl Turns {
    /// Takes `slug` out of the queue and gives the time it has waited since, if it
    /// waited. A board that still wants a slot and gets none goes back with [`Self::wait`].
    pub fn take(&mut self, slug: &BoardSlug) -> Option<Timestamp> {
        self.waiting.remove(slug).map(|w| w.since)
    }

    /// Whether a board other than `slug` has waited for a slot since before `since`,
    /// `slug`'s own wait, if it waited.
    pub fn another_first(&self, slug: &BoardSlug, since: Option<Timestamp>) -> bool {
        self.waiting
            .iter()
            .any(|(other, w)| other != slug && since.is_none_or(|mine| w.since < mine))
    }

    /// Drops the boards that have not asked since `last`, the reconcile before this one:
    /// a board that no longer wants a slot, or that the device no longer dispatches, must
    /// not hold the others back.
    pub fn keep_asked_since(&mut self, last: Timestamp) {
        self.waiting.retain(|_, w| w.asked >= last);
    }

    /// Records that `slug` was refused a slot at `now`, having waited since `since` if it
    /// waited before. True when its wait starts now.
    pub fn wait(&mut self, slug: &BoardSlug, since: Option<Timestamp>, now: Timestamp) -> bool {
        self.waiting.insert(
            slug.clone(),
            Waiting {
                since: since.unwrap_or(now),
                asked: now,
            },
        );
        since.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slug(s: &str) -> BoardSlug {
        s.parse().unwrap()
    }

    fn at(minute: i64) -> Timestamp {
        Timestamp::from_timestamp(1_800_000_000 + minute * 60, 0).unwrap()
    }

    #[test]
    fn the_default_limit_is_one_run_per_four_gib_and_at_least_two() {
        assert_eq!(default_run_limit(Some(16 << 30)), 4);
        // A 16 GiB Linux host reports about 15.5 GiB.
        assert_eq!(default_run_limit(Some(16_303_412 * 1024)), 4);
        assert_eq!(default_run_limit(Some(36 << 30)), 9);
        assert_eq!(default_run_limit(Some(4 << 30)), MIN_RUNS);
        assert_eq!(default_run_limit(Some(0)), MIN_RUNS);
        assert_eq!(default_run_limit(None), FALLBACK_RUNS);
    }

    #[test]
    fn an_unknown_load_names_one_project_and_stays_short() {
        assert_eq!(Running::default().unknown(), None);
        let unread = (0..15).map(|n| format!("/projects/{n} ({})", "e".repeat(400)));
        let running = Running {
            threads: BTreeSet::new(),
            unread: unread.collect(),
        };
        let text = running.unknown().unwrap();
        assert!(text.contains("/projects/0 (eee"), "{text}");
        assert!(text.contains("and of 14 more projects"), "{text}");
        assert!(text.contains("config.toml"), "{text}");
        assert!(text.chars().count() < 450, "{text}");
    }

    #[test]
    fn workers_leave_one_slot_to_the_judges() {
        assert_eq!(worker_limit(4), 3);
        assert_eq!(worker_limit(2), 1);
        assert_eq!(worker_limit(1), 1);
        assert_eq!(worker_limit(0), 0);
    }

    #[test]
    fn reserve_defaults_to_one_and_lowers_only_bounded_daemon_limits() {
        let mut config = Config::default();
        assert_eq!(reserve(&config), DEFAULT_RESERVE);
        assert_eq!(daemon_limit(4, reserve(&config)), 3);
        assert_eq!(daemon_limit(2, reserve(&config)), 1);
        assert_eq!(daemon_limit(1, reserve(&config)), 0);
        assert_eq!(daemon_limit(usize::MAX, reserve(&config)), usize::MAX);

        config.reserve_runs = Some(0);
        assert_eq!(reserve(&config), 0);
        assert_eq!(daemon_limit(4, reserve(&config)), 4);

        let parsed: Config = toml::from_str("reserve = 2\n").unwrap();
        assert_eq!(parsed.reserve_runs, Some(2));
        assert_eq!(daemon_limit(4, reserve(&parsed)), 2);
    }

    #[test]
    fn mem_total_reads_proc_meminfo() {
        let text = "MemTotal:       16303412 kB\nMemFree:          812340 kB\n";
        assert_eq!(mem_total(text), Some(16_303_412 * 1024));
        assert_eq!(mem_total("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn the_board_that_waited_longest_goes_first() {
        let (a, b) = (slug("alpha"), slug("beta"));
        let mut turns = Turns::default();
        assert!(!turns.another_first(&a, None));
        assert!(turns.wait(&b, None, at(0)));
        // `a` never waited, so `b` goes first; `b` itself is held back by nobody.
        assert!(turns.another_first(&a, None));
        assert!(turns.wait(&a, None, at(1)));
        let since = turns.take(&b);
        assert_eq!(since, Some(at(0)));
        assert!(!turns.another_first(&b, since));
        // Refused again without a start, `b` keeps its place.
        assert!(!turns.wait(&b, since, at(2)));
        let since = turns.take(&a);
        assert!(turns.another_first(&a, since));
        // Once `b` is served, `a` is next.
        turns.take(&b);
        assert!(!turns.another_first(&a, since));
    }

    #[test]
    fn a_board_that_was_served_waits_behind_the_boards_that_asked() {
        let (a, b) = (slug("alpha"), slug("beta"));
        let mut turns = Turns::default();
        turns.wait(&a, None, at(0));
        turns.wait(&b, None, at(1));
        // `a` gets a slot, then is refused for its next task: its wait starts anew.
        assert!(turns.take(&a).is_some());
        assert!(turns.wait(&a, None, at(2)));
        let since = turns.take(&b);
        assert!(!turns.another_first(&b, since));
        turns.wait(&b, since, at(3));
        let since = turns.take(&a);
        assert!(turns.another_first(&a, since));
    }

    #[test]
    fn a_board_that_stopped_asking_loses_its_place() {
        let (a, b) = (slug("alpha"), slug("beta"));
        let mut turns = Turns::default();
        turns.wait(&b, None, at(0));
        // It asked in the last pass: kept.
        turns.keep_asked_since(at(0));
        assert!(turns.another_first(&a, None));
        // It did not ask in the pass after: dropped, with its place.
        turns.keep_asked_since(at(1));
        assert!(!turns.another_first(&a, None));
        assert_eq!(turns.take(&b), None);
    }

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::t3::{ListedThread, NewWorktree, RecentMessages, ThreadInfo};

    type PollAnswer = Box<dyn Fn(&Path) -> Result<Vec<String>, String> + Send + Sync>;

    struct ScriptedT3 {
        answers: Mutex<VecDeque<(Duration, PollAnswer)>>,
        calls: Mutex<usize>,
    }

    impl ScriptedT3 {
        fn new(steps: Vec<(Duration, Result<Vec<&'static str>, &'static str>)>) -> Self {
            let answers = steps
                .into_iter()
                .map(|(delay, res)| {
                    let f: PollAnswer = Box::new(move |_| {
                        res.as_ref()
                            .map(|ids| ids.iter().map(|s| (*s).to_owned()).collect())
                            .map_err(|e| (*e).to_owned())
                    });
                    (delay, f)
                })
                .collect();
            Self {
                answers: Mutex::new(answers),
                calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl T3 for ScriptedT3 {
        async fn start_thread(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: Option<&NewWorktree>,
            _: Option<&crate::config::ThreadModel>,
        ) -> anyhow::Result<String> {
            unimplemented!()
        }

        async fn send(&self, _: &str, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }

        async fn thread_info(&self, _: &str) -> anyhow::Result<Option<ThreadInfo>> {
            unimplemented!()
        }

        async fn recent_messages(&self, _: &str) -> anyhow::Result<Option<RecentMessages>> {
            unimplemented!()
        }

        async fn thread_link(&self, _: &str) -> anyhow::Result<Option<String>> {
            unimplemented!()
        }

        async fn threads_titled(&self, _: &Path, _: &str) -> anyhow::Result<Vec<ListedThread>> {
            unimplemented!()
        }

        async fn running_threads(&self, project: &Path) -> anyhow::Result<Vec<String>> {
            *self.calls.lock().unwrap() += 1;
            let next = self.answers.lock().unwrap().pop_front();
            let Some((delay, f)) = next else {
                return Ok(Vec::new());
            };
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            f(project).map_err(|msg| anyhow::anyhow!("{msg}"))
        }
    }

    fn config_with_project() -> Config {
        Config {
            device: Some("air".parse().unwrap()),
            projects: BTreeMap::from([(
                "github.com/syz51/agent-board".into(),
                PathBuf::from("/p/a"),
            )]),
            ..Default::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_slot_succeeds_immediately_including_at_daemon_reserved_ceiling() {
        let config = config_with_project();
        // Device limit 4 with default reserve 1 -> daemon caps itself at 3, leaving the 4th
        // slot free for an owner launch (agent-board-636). wait_for_slot must return ready
        // immediately at 3 running turns, matching hook/launch.rs.
        assert_eq!(daemon_limit(4, reserve(&config)), 3);
        let t3 = ScriptedT3::new(vec![(Duration::ZERO, Ok(vec!["t1", "t2", "t3"]))]);
        let ready = wait_for_slot(
            &t3,
            &config,
            4,
            WaitOptions {
                slots: 1,
                timeout: Duration::from_secs(30),
                poll: Duration::from_secs(5),
                count_budget: COUNT_BUDGET,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            ready,
            WaitReady {
                device: Some("air".into()),
                running: 3,
                limit: 4,
                slots: 1,
                free: 1,
                waited_secs: 0,
            }
        );
        assert_eq!(t3.calls(), 1);
        assert!(
            ready
                .summary()
                .contains("device air has 1 free agent-turn slot")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_slot_polls_until_enough_slots_free() {
        let config = config_with_project();
        let t3 = ScriptedT3::new(vec![
            (Duration::ZERO, Ok(vec!["t1", "t2", "t3", "t4"])),
            (Duration::ZERO, Ok(vec!["t1", "t2", "t3"])), // only 1 free, need 2
            (Duration::ZERO, Ok(vec!["t1", "t2"])),       // 2 free -> ready
        ]);
        let ready = wait_for_slot(
            &t3,
            &config,
            4,
            WaitOptions {
                slots: 2,
                timeout: Duration::from_secs(30),
                poll: Duration::from_secs(5),
                count_budget: COUNT_BUDGET,
            },
        )
        .await
        .unwrap();
        assert_eq!(ready.running, 2);
        assert_eq!(ready.free, 2);
        assert_eq!(ready.slots, 2);
        assert_eq!(ready.waited_secs, 10);
        assert_eq!(t3.calls(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_slot_times_out_when_device_stays_full_or_timeout_is_zero() {
        let config = config_with_project();
        // Zero timeout checks once without sleeping and fails when full.
        let t3 = ScriptedT3::new(vec![(Duration::ZERO, Ok(vec!["t1", "t2"]))]);
        let err = wait_for_slot(
            &t3,
            &config,
            2,
            WaitOptions {
                slots: 1,
                timeout: Duration::ZERO,
                poll: Duration::from_secs(5),
                count_budget: COUNT_BUDGET,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(t3.calls(), 1);
        let msg = err.to_string();
        assert!(msg.contains("timed out after 0s"), "{msg}");
        assert!(
            msg.contains("still has 2 of 2 agent turns running"),
            "{msg}"
        );

        // Nonzero timeout polls until deadline and fails when still full without overshooting.
        let started = Instant::now();
        let t3 = ScriptedT3::new(vec![
            (Duration::ZERO, Ok(vec!["t1", "t2"])),
            (Duration::ZERO, Ok(vec!["t1", "t2"])),
        ]);
        let err = wait_for_slot(
            &t3,
            &config,
            2,
            WaitOptions {
                slots: 1,
                timeout: Duration::from_secs(10),
                poll: Duration::from_secs(5),
                count_budget: COUNT_BUDGET,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(t3.calls(), 2);
        assert_eq!(started.elapsed(), Duration::from_secs(10));
        let msg = err.to_string();
        assert!(msg.contains("timed out after 10s"), "{msg}");
        assert!(
            msg.contains("still has 2 of 2 agent turns running"),
            "{msg}"
        );
        assert!(msg.contains("do the work in this thread"), "{msg}");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_slot_never_overshoots_timeout_or_panics_on_huge_timeout() {
        let config = config_with_project();
        // Poll 1 finishes fast while full (2/2); poll 2 hangs forever. Wait must exit right at
        // the 3s deadline with last_running reset to 0 (no stale "at least 2 of 2" claim).
        let started = Instant::now();
        let t3 = ScriptedT3::new(vec![
            (Duration::from_millis(50), Ok(vec!["t1", "t2"])),
            (Duration::from_secs(60), Ok(vec![])),
        ]);
        let err = wait_for_slot(
            &t3,
            &config,
            2,
            WaitOptions {
                slots: 1,
                timeout: Duration::from_secs(3),
                poll: Duration::from_secs(5),
                count_budget: COUNT_BUDGET,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(started.elapsed(), Duration::from_secs(3));
        let msg = err.to_string();
        assert!(msg.contains("could not count agent turns"), "{msg}");
        assert!(!msg.contains("at least 2"), "{msg}");
        assert!(msg.contains("check that T3 is running"), "{msg}");

        // Huge Duration::MAX timeout does not panic on Instant addition.
        let t3 = ScriptedT3::new(vec![(Duration::ZERO, Ok(vec!["t1"]))]);
        let ready = wait_for_slot(
            &t3,
            &config,
            2,
            WaitOptions {
                slots: 1,
                timeout: Duration::MAX,
                poll: Duration::from_secs(5),
                count_budget: COUNT_BUDGET,
            },
        )
        .await
        .unwrap();
        assert_eq!(ready.free, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_slot_rejects_impossible_slot_count_without_polling() {
        let config = config_with_project();
        let t3 = ScriptedT3::new(vec![]);
        let err = wait_for_slot(
            &t3,
            &config,
            2,
            WaitOptions {
                slots: 3,
                timeout: Duration::from_secs(30),
                poll: Duration::from_secs(5),
                count_budget: COUNT_BUDGET,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(t3.calls(), 0);
        assert_eq!(err, WaitError::Impossible { slots: 3, limit: 2 });
        assert!(err.to_string().contains("exceeds this device's limit of 2"));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_slot_waits_through_transient_t3_error_and_reports_persistent_failure() {
        let config = config_with_project();
        // Transient T3 error on poll 1 does not falsely succeed as 0 running; poll 2 succeeds.
        let t3 = ScriptedT3::new(vec![
            (Duration::ZERO, Err("server restarting")),
            (Duration::ZERO, Ok(vec!["t1"])),
        ]);
        let ready = wait_for_slot(
            &t3,
            &config,
            2,
            WaitOptions {
                slots: 1,
                timeout: Duration::from_secs(10),
                poll: Duration::from_secs(5),
                count_budget: COUNT_BUDGET,
            },
        )
        .await
        .unwrap();
        assert_eq!(ready.running, 1);
        assert_eq!(t3.calls(), 2);

        // Persistent T3 hang times out at the deadline with the unknown cause.
        let t3 = ScriptedT3::new(vec![(Duration::from_secs(60), Ok(vec![]))]);
        let err = wait_for_slot(
            &t3,
            &config,
            2,
            WaitOptions {
                slots: 1,
                timeout: Duration::from_secs(3),
                poll: Duration::from_secs(5),
                count_budget: COUNT_BUDGET,
            },
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("timed out after 3s"), "{msg}");
        assert!(msg.contains("T3 listed no threads within"), "{msg}");
    }

    #[test]
    fn weighted_capacity_capabilities_runners_and_repos_defaults_and_overrides() {
        let mut config = Config::default();
        assert_eq!(default_weight_units(Some(16 << 30)), 8);
        assert_eq!(default_weight_units(Some(48 << 30)), 24);
        assert_eq!(default_weight_units(None), 8);
        assert_eq!(effective_weight_units(&config, Some(4), None), 8);
        assert_eq!(reserve_units(&config), 2);
        assert_eq!(host_mem_gib(Some(16_303_412 * 1024)), Some(16));
        assert_eq!(host_mem_gib(Some(48 << 30)), Some(48));

        // runs_at_once derives weight_units = runs_at_once * 2 when weight_units is unset.
        config.runs_at_once = std::num::NonZeroU32::new(6);
        assert_eq!(effective_weight_units(&config, Some(4), None), 12);
        // Explicit weight_units wins over runs_at_once.
        config.weight_units = Some(10);
        assert_eq!(effective_weight_units(&config, Some(4), None), 10);

        // Capabilities include auto-detected os/arch plus deduped configured tags.
        config.capabilities = vec![
            "git:cloneable".into(),
            "tool:xcode".into(),
            "git:cloneable".into(),
            format!("os:{}", host_os()),
        ];
        let caps = effective_capabilities(&config);
        assert_eq!(caps[0], format!("os:{}", host_os()));
        assert_eq!(caps[1], format!("arch:{}", host_arch()));
        assert_eq!(caps.len(), 4);
        assert!(caps.contains(&"git:cloneable".to_owned()));
        assert!(caps.contains(&"tool:xcode".to_owned()));

        // Runners and protocol features default cleanly and honor overrides.
        assert_eq!(effective_runners(&config, "t3"), vec!["t3"]);
        config.runners = vec!["acp".into(), "t3".into(), "acp".into()];
        assert_eq!(effective_runners(&config, "t3"), vec!["acp", "t3"]);

        assert_eq!(
            effective_protocol_features(&config),
            vec!["v1-json", "sse-v1"]
        );
        config.protocol_features = vec!["v1-json".into(), "run-events-v1".into()];
        assert_eq!(
            effective_protocol_features(&config),
            vec!["v1-json", "run-events-v1"]
        );

        // Repos normalize keys from [projects].
        config.projects.insert(
            "https://github.com/Org/Agent-Board.git".into(),
            PathBuf::from("/src/ab"),
        );
        config.projects.insert(
            "github.com/org/agent-board".into(),
            PathBuf::from("/src/ab2"),
        );
        config
            .projects
            .insert("local-alias-without-host".into(), PathBuf::from("/src/loc"));
        assert_eq!(
            configured_repos(&config),
            vec!["github.com/org/agent-board"]
        );

        // Judge weights: review=2, verdict/triage/goal=1.
        assert_eq!(cost_units_for_judge(JudgeKind::Review), 2);
        assert_eq!(cost_units_for_judge(JudgeKind::Verdict), 1);
        assert_eq!(cost_units_for_judge(JudgeKind::Triage), 1);
        assert_eq!(cost_units_for_judge(JudgeKind::Goal), 1);
    }
}
