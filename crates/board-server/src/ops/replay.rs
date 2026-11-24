//! Deterministic offline scheduler replay simulator (`docs/SCHEDULER_DESIGN.md`, Section 6.4).
//!
//! Opens a `board.db` SQLite file strictly read-only, extracts historical boards, devices,
//! tasks, and managed runs over a configurable time window, and replays the workload
//! deterministically under both the baseline (active) policy and an optional candidate policy
//! so agents can prove empirical improvement before opening a PR.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use board_proto::{
    BoardSlug, ConflictKeySpec, DeviceCapacity, DeviceEntry, DeviceName, DeviceRole, DeviceStatus,
    Priority, ReplayComparison, ReplayMetrics, ReplayPolicyConfig, ResourceWeight, RunKind,
    SCHEDULER_POLICY_VERSION_DEFAULT, SchedulerWeights, Taint, TaintEffect, TaintToleration,
    TaskAffinity, TaskId, Timestamp,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{open_read_only, parse_enum, parse_id, parse_json, ts};
use crate::error::{Error, Result};
use crate::ops::dispatch::{
    ScoreInputs, effective_device_taints, is_tolerated, merge_conflict_keys, score_candidate_base,
};
use crate::ops::policy::validate_weights;
use crate::ops::stats::{median_of, percentile_of};

/// Default simulated duration when a historical item has no recorded runtime (5 minutes).
const DEFAULT_SIM_DURATION_MS: i64 = 5 * 60_000;
/// Minimum simulated duration so runs advance simulated time cleanly (1 second).
const MIN_SIM_DURATION_MS: i64 = 1_000;
/// Rolling fair-share consumption window (24 hours), matching live dispatch.
const FAIR_SHARE_WINDOW_MS: i64 = 24 * 3_600_000;
/// Milliseconds in one day.
const DAY_MS: i64 = 86_400_000;

/// Options for [`replay_schedule`].
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Start of the historical window. When `None`, derived as `until - days`.
    pub since: Option<Timestamp>,
    /// End of the historical window.
    pub until: Timestamp,
    /// Window length in days when `since` is `None` (1..=365, default 7).
    pub days: u32,
    /// Optional candidate policy to evaluate side-by-side against the active baseline.
    pub candidate: Option<ReplayPolicyConfig>,
}

/// Parses a candidate policy file (JSON or TOML) for `board admin replay-schedule --policy`.
pub fn load_policy_config(path: &Path) -> Result<ReplayPolicyConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("cannot read policy file {}: {e}", path.display())))?;
    let is_json = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    let mut cfg: ReplayPolicyConfig = if is_json {
        serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("invalid JSON policy {}: {e}", path.display())))?
    } else if let Ok(parsed) = toml::from_str::<ReplayPolicyConfig>(&text) {
        parsed
    } else {
        serde_json::from_str::<ReplayPolicyConfig>(&text).map_err(|e| {
            Error::Config(format!(
                "invalid policy {} (expected TOML or JSON): {e}",
                path.display()
            ))
        })?
    };
    cfg.weights = validate_weights(cfg.weights)?;
    if cfg.version.trim().is_empty() {
        cfg.version = "candidate".to_owned();
    }
    Ok(cfg)
}

#[derive(Debug, Clone)]
struct SimBoard {
    repos: Vec<String>,
    parallel_tasks: u32,
    home_devices: Vec<DeviceName>,
}

#[derive(Debug, Clone)]
struct SimDevice {
    name: DeviceName,
    capacity_units: u32,
    capabilities: Vec<String>,
    runners: Vec<String>,
    repos: Vec<String>,
    taints: Vec<Taint>,
    preference: i32,
    drain: bool,
}

#[derive(Debug, Clone)]
struct SimItem {
    key: String,
    board: BoardSlug,
    kind: RunKind,
    priority: Priority,
    rank: Option<u32>,
    ready_at_ms: i64,
    duration_ms: i64,
    cost_units: u32,
    weight_tier: ResourceWeight,
    repo: Option<String>,
    needs_local_repo: bool,
    requires: Vec<String>,
    tolerations: Vec<TaintToleration>,
    affinity: Option<TaskAffinity>,
    preferred_runner: Option<String>,
    conflict_keys: Vec<ConflictKeySpec>,
}

/// Runs a deterministic read-only offline replay against `db_path`, comparing the DB's
/// active baseline policy against `opts.candidate` (when supplied).
pub fn replay_schedule(db_path: &Path, opts: ReplayOptions) -> Result<ReplayComparison> {
    if !db_path.is_file() {
        return Err(Error::Config(format!(
            "{} is not a database file; pass the server's database (`--db`)",
            db_path.display()
        )));
    }
    let conn = open_read_only(db_path)?;

    let until_ms = opts.until.timestamp_millis();
    let (since_ms, days) = match opts.since {
        Some(since) => {
            let s = since.timestamp_millis();
            let span_days = ((until_ms - s).max(DAY_MS) as f64 / DAY_MS as f64).ceil() as u32;
            (s, span_days.clamp(1, 365))
        }
        None => {
            let d = opts.days.clamp(1, 365);
            (until_ms.saturating_sub(i64::from(d) * DAY_MS), d)
        }
    };

    let (baseline_version, baseline_weights) = load_baseline_policy(&conn)?;
    let boards = load_sim_boards(&conn)?;
    let devices = load_sim_devices(&conn)?;
    let (items, historical_tasks, historical_runs) =
        load_sim_items(&conn, &boards, since_ms, until_ms)?;

    let baseline = simulate(
        &items,
        &boards,
        &devices,
        &baseline_version,
        &baseline_weights,
        until_ms,
    );
    let candidate = opts.candidate.as_ref().map(|cand| {
        let weights = cand.weights;
        simulate(&items, &boards, &devices, &cand.version, &weights, until_ms)
    });

    Ok(ReplayComparison {
        since: ts(since_ms),
        until: ts(until_ms),
        days,
        historical_tasks,
        historical_runs,
        devices_count: devices.len() as u32,
        boards_count: boards.len() as u32,
        baseline,
        candidate,
    })
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |r| r.get(0),
    )?)
}

fn load_baseline_policy(conn: &Connection) -> Result<(String, SchedulerWeights)> {
    if !table_exists(conn, "scheduler_policies")? {
        return Ok((
            SCHEDULER_POLICY_VERSION_DEFAULT.to_owned(),
            SchedulerWeights::default(),
        ));
    }
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT version, weights_json FROM scheduler_policies WHERE active = 1 LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((version, weights_json)) => Ok((version, parse_json(&weights_json)?)),
        None => Ok((
            SCHEDULER_POLICY_VERSION_DEFAULT.to_owned(),
            SchedulerWeights::default(),
        )),
    }
}

fn load_sim_boards(conn: &Connection) -> Result<BTreeMap<BoardSlug, SimBoard>> {
    let mut out = BTreeMap::new();
    let mut stmt =
        conn.prepare("SELECT slug, repos, parallel_tasks, home_devices FROM boards ORDER BY slug")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let slug: BoardSlug = parse_id(&r.get::<_, String>(0)?)?;
        let repos: Vec<String> = parse_json(&r.get::<_, String>(1)?)?;
        let parallel_tasks: u32 = r.get::<_, i64>(2)?.clamp(1, 32) as u32;
        let home_devices: Vec<DeviceName> = parse_json(&r.get::<_, String>(3)?)?;
        out.insert(
            slug,
            SimBoard {
                repos,
                parallel_tasks,
                home_devices,
            },
        );
    }
    Ok(out)
}

fn load_sim_devices(conn: &Connection) -> Result<Vec<SimDevice>> {
    let has_active_runners: bool = conn.query_row(
        "SELECT EXISTS ( \
             SELECT 1 FROM devices \
             WHERE revoked_at IS NULL \
               AND (heartbeat_at IS NOT NULL OR runners_json != '[]') \
         )",
        [],
        |r| r.get(0),
    )?;
    let has_runs = table_exists(conn, "runs")?;
    let mut out = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT d.name, d.role, d.capacity_json, d.capabilities_json, d.runners_json, \
                d.repos_json, d.taints_json, d.preference, d.drain, d.heartbeat_at, d.user \
         FROM devices d WHERE d.revoked_at IS NULL ORDER BY d.name",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let name: DeviceName = parse_id(&r.get::<_, String>(0)?)?;
        let role: DeviceRole = parse_enum(&r.get::<_, String>(1)?)?;
        let capacity: Option<DeviceCapacity> = r
            .get::<_, Option<String>>(2)?
            .map(|s| parse_json(&s))
            .transpose()?;
        let mut capabilities: Vec<String> = parse_json(&r.get::<_, String>(3)?)?;
        let mut runners: Vec<String> = parse_json(&r.get::<_, String>(4)?)?;
        let repos: Vec<String> = parse_json(&r.get::<_, String>(5)?)?;
        let raw_taints: Vec<Taint> = parse_json(&r.get::<_, String>(6)?)?;
        let preference: i32 = r.get(7)?;
        let drain: bool = r.get::<_, i64>(8)? != 0;
        let heartbeat_at: Option<i64> = r.get(9)?;
        let is_user: bool = r.get::<_, i64>(10)? != 0;

        let used_in_runs = has_runs
            && conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM runs WHERE device = ?1 AND origin = 'managed')",
                [name.as_str()],
                |r| r.get::<_, bool>(0),
            )?;
        // Exclude non-runner user-only tokens and unheartbeated devices when the DB has
        // real heartbeated/active runner devices.
        if has_active_runners && heartbeat_at.is_none() && runners.is_empty() && !used_in_runs {
            continue;
        }
        if !has_active_runners && is_user && !used_in_runs {
            continue;
        }

        if runners.is_empty() && role == DeviceRole::Runner {
            runners = vec!["acp".into(), "deterministic".into(), "t3".into()];
        }
        if capabilities.is_empty() && role == DeviceRole::Runner {
            capabilities = vec!["git:cloneable".into()];
        }

        // Reuse `effective_device_taints` so role=control / role=service_host isolation
        // matches live dispatch identically.
        let entry = DeviceEntry {
            name: name.clone(),
            role,
            status: DeviceStatus::Online,
            drain,
            os: None,
            arch: None,
            capacity,
            free_units: None,
            capabilities: capabilities.clone(),
            runners: runners.clone(),
            repos: repos.clone(),
            taints: raw_taints,
            preference,
            protocol_features: Vec::new(),
            heartbeat_at: None,
        };
        let taints = effective_device_taints(&entry);
        let capacity_units = capacity.map_or(4, |c| c.weight_units.max(1));

        out.push(SimDevice {
            name,
            capacity_units,
            capabilities,
            runners,
            repos,
            taints,
            preference,
            drain,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct TaskMeta {
    board: BoardSlug,
    priority: Priority,
    rank: Option<u32>,
    created_at: i64,
    repo: Option<String>,
    weight: Option<ResourceWeight>,
    resource_units: Option<u32>,
    requires: Vec<String>,
    tolerations: Vec<TaintToleration>,
    conflict_keys: Vec<ConflictKeySpec>,
    affinity: Option<TaskAffinity>,
    preferred_runner: Option<String>,
}

fn weight_and_units_from_meta(
    weight: Option<ResourceWeight>,
    resource_units: Option<u32>,
    fallback_units: u32,
) -> (ResourceWeight, u32) {
    let units = resource_units
        .or_else(|| weight.and_then(ResourceWeight::default_units))
        .unwrap_or(fallback_units)
        .max(1);
    let tier = match weight {
        Some(w) if w != ResourceWeight::Unknown => w,
        _ => {
            if units >= 4 {
                ResourceWeight::Heavy
            } else if units <= 1 {
                ResourceWeight::Light
            } else {
                ResourceWeight::Standard
            }
        }
    };
    (tier, units)
}

fn load_sim_items(
    conn: &Connection,
    boards: &BTreeMap<BoardSlug, SimBoard>,
    since_ms: i64,
    until_ms: i64,
) -> Result<(Vec<SimItem>, u32, u32)> {
    let has_runs = table_exists(conn, "runs")?;
    // Load task metadata for tasks either created in window or (when `runs` exists)
    // referenced by runs in window.
    let mut task_meta: BTreeMap<TaskId, TaskMeta> = BTreeMap::new();
    let task_sql = if has_runs {
        "SELECT id, board, priority, rank, created_at, repo, weight, resource_units, \
                requires_json, tolerations_json, conflict_keys_json, affinity_json, \
                preferred_runner \
         FROM tasks \
         WHERE (created_at >= ?1 AND created_at <= ?2) \
            OR id IN (SELECT task_id FROM runs WHERE task_id IS NOT NULL AND created_at >= ?1 AND created_at <= ?2)"
    } else {
        "SELECT id, board, priority, rank, created_at, repo, weight, resource_units, \
                requires_json, tolerations_json, conflict_keys_json, affinity_json, \
                preferred_runner \
         FROM tasks \
         WHERE created_at >= ?1 AND created_at <= ?2"
    };
    let mut stmt = conn.prepare(task_sql)?;
    let mut rows = stmt.query(params![since_ms, until_ms])?;
    while let Some(r) = rows.next()? {
        let id: TaskId = parse_id(&r.get::<_, String>(0)?)?;
        let meta = TaskMeta {
            board: parse_id(&r.get::<_, String>(1)?)?,
            priority: parse_enum(&r.get::<_, String>(2)?)?,
            rank: r.get::<_, Option<i64>>(3)?.map(|v| v.max(0) as u32),
            created_at: r.get(4)?,
            repo: r.get(5)?,
            weight: r
                .get::<_, Option<String>>(6)?
                .map(|s| parse_enum(&s))
                .transpose()?,
            resource_units: r.get::<_, Option<i64>>(7)?.map(|v| v.max(1) as u32),
            requires: r
                .get::<_, Option<String>>(8)?
                .map(|s| parse_json(&s))
                .transpose()?
                .unwrap_or_default(),
            tolerations: r
                .get::<_, Option<String>>(9)?
                .map(|s| parse_json(&s))
                .transpose()?
                .unwrap_or_default(),
            conflict_keys: r
                .get::<_, Option<String>>(10)?
                .map(|s| parse_json(&s))
                .transpose()?
                .unwrap_or_default(),
            affinity: r
                .get::<_, Option<String>>(11)?
                .map(|s| parse_json(&s))
                .transpose()?,
            preferred_runner: r.get(12)?,
        };
        task_meta.insert(id, meta);
    }
    drop(rows);

    let mut items: Vec<SimItem> = Vec::new();
    let mut seen_run_tasks: BTreeSet<TaskId> = BTreeSet::new();
    let mut historical_runs: u32 = 0;

    if has_runs {
        let has_outcomes = table_exists(conn, "run_outcomes")?;
        let sql = if has_outcomes {
            "SELECT r.id, r.task_id, r.board, r.kind, r.cost_units, r.created_at, \
                    r.started_at, r.ended_at, ro.ready_to_leased_secs, ro.running_secs \
             FROM runs r \
             LEFT JOIN run_outcomes ro ON ro.run_id = r.id \
             WHERE r.origin = 'managed' AND r.board IS NOT NULL \
               AND r.created_at >= ?1 AND r.created_at <= ?2 \
             ORDER BY r.created_at, r.id"
        } else {
            "SELECT r.id, r.task_id, r.board, r.kind, r.cost_units, r.created_at, \
                    r.started_at, r.ended_at, NULL, NULL \
             FROM runs r \
             WHERE r.origin = 'managed' AND r.board IS NOT NULL \
               AND r.created_at >= ?1 AND r.created_at <= ?2 \
             ORDER BY r.created_at, r.id"
        };
        let mut rstmt = conn.prepare(sql)?;
        let mut rrows = rstmt.query(params![since_ms, until_ms])?;
        while let Some(r) = rrows.next()? {
            let run_id: String = r.get(0)?;
            let task_id: Option<TaskId> = r
                .get::<_, Option<String>>(1)?
                .map(|s| parse_id(&s))
                .transpose()?;
            let board_slug: BoardSlug = parse_id(&r.get::<_, String>(2)?)?;
            let Some(board) = boards.get(&board_slug) else {
                continue;
            };
            let kind: RunKind = parse_enum(&r.get::<_, String>(3)?)?;
            if matches!(kind, RunKind::Interactive | RunKind::Unknown) {
                continue;
            }
            historical_runs += 1;
            let cost_units = r.get::<_, i64>(4)?.max(1) as u32;
            let created_at: i64 = r.get(5)?;
            let started_at: Option<i64> = r.get(6)?;
            let ended_at: Option<i64> = r.get(7)?;
            let ready_to_leased_secs: Option<i64> = r.get(8)?;
            let outcome_running_secs: Option<i64> = r.get(9)?;

            let meta = task_id.as_ref().and_then(|tid| task_meta.get(tid));
            if let Some(ref tid) = task_id {
                seen_run_tasks.insert(tid.clone());
            }

            let ready_at_ms = if let Some(wait_s) = ready_to_leased_secs {
                created_at.saturating_sub(wait_s.max(0).saturating_mul(1000))
            } else if let Some(m) = meta {
                m.created_at.min(created_at)
            } else {
                created_at
            };
            let duration_ms = outcome_running_secs
                .map(|s| s.max(1).saturating_mul(1000))
                .or_else(|| match (started_at, ended_at) {
                    (Some(s), Some(e)) => Some((e - s).max(MIN_SIM_DURATION_MS)),
                    _ => None,
                })
                .unwrap_or(DEFAULT_SIM_DURATION_MS)
                .max(MIN_SIM_DURATION_MS);

            let repo = meta
                .and_then(|m| m.repo.clone())
                .or_else(|| board.repos.first().cloned());
            let (weight_tier, eff_units) = match meta {
                Some(m) if kind == RunKind::Worker => {
                    weight_and_units_from_meta(m.weight, m.resource_units, cost_units)
                }
                _ => weight_and_units_from_meta(None, Some(cost_units), cost_units),
            };
            let derived_lock = match kind {
                RunKind::Worker => repo.as_ref().map(|r| ConflictKeySpec {
                    key: format!("repo:{r}:write"),
                    max: board.parallel_tasks,
                }),
                RunKind::Merge => repo.as_ref().map(|r| ConflictKeySpec {
                    key: format!("repo:{r}:merge"),
                    max: 1,
                }),
                _ => None,
            };
            let explicit_locks = meta.map(|m| m.conflict_keys.as_slice()).unwrap_or(&[]);
            let conflict_keys = merge_conflict_keys(derived_lock, explicit_locks);

            items.push(SimItem {
                key: format!("run:{run_id}"),
                board: board_slug,
                kind,
                priority: meta.map_or(Priority::P2, |m| m.priority),
                rank: meta.and_then(|m| m.rank),
                ready_at_ms,
                duration_ms,
                cost_units: eff_units,
                weight_tier,
                repo,
                needs_local_repo: !matches!(kind, RunKind::Merge | RunKind::Housekeeping),
                requires: if matches!(kind, RunKind::Merge | RunKind::Housekeeping) {
                    Vec::new()
                } else {
                    meta.map(|m| m.requires.clone()).unwrap_or_default()
                },
                tolerations: meta.map(|m| m.tolerations.clone()).unwrap_or_default(),
                affinity: meta.and_then(|m| m.affinity.clone()),
                preferred_runner: if matches!(kind, RunKind::Merge | RunKind::Housekeeping) {
                    Some("deterministic".to_owned())
                } else {
                    meta.and_then(|m| m.preferred_runner.clone())
                },
                conflict_keys,
            });
        }
    }

    // Also include auto tasks created inside the window that had no managed run recorded
    // (for example, historical backlogs before Slice 2.1 or queued work).
    let mut historical_tasks: u32 = 0;
    let mut tstmt = conn.prepare(
        "SELECT id FROM tasks \
         WHERE auto = 1 AND created_at >= ?1 AND created_at <= ?2 \
         ORDER BY created_at, id",
    )?;
    let mut trows = tstmt.query(params![since_ms, until_ms])?;
    while let Some(r) = trows.next()? {
        let tid: TaskId = parse_id(&r.get::<_, String>(0)?)?;
        historical_tasks += 1;
        if seen_run_tasks.contains(&tid) {
            continue;
        }
        let Some(meta) = task_meta.get(&tid) else {
            continue;
        };
        let Some(board) = boards.get(&meta.board) else {
            continue;
        };
        let repo = meta.repo.clone().or_else(|| board.repos.first().cloned());
        let (weight_tier, cost_units) =
            weight_and_units_from_meta(meta.weight, meta.resource_units, 2);
        let derived = repo.as_ref().map(|r| ConflictKeySpec {
            key: format!("repo:{r}:write"),
            max: board.parallel_tasks,
        });
        let conflict_keys = merge_conflict_keys(derived, &meta.conflict_keys);
        items.push(SimItem {
            key: format!("task:{tid}"),
            board: meta.board.clone(),
            kind: RunKind::Worker,
            priority: meta.priority,
            rank: meta.rank,
            ready_at_ms: meta.created_at,
            duration_ms: DEFAULT_SIM_DURATION_MS,
            cost_units,
            weight_tier,
            repo,
            needs_local_repo: true,
            requires: meta.requires.clone(),
            tolerations: meta.tolerations.clone(),
            affinity: meta.affinity.clone(),
            preferred_runner: meta.preferred_runner.clone(),
            conflict_keys,
        });
    }

    // Deterministic canonical arrival order.
    items.sort_by(|a, b| {
        a.ready_at_ms
            .cmp(&b.ready_at_ms)
            .then_with(|| a.board.cmp(&b.board))
            .then_with(|| a.key.cmp(&b.key))
    });

    Ok((items, historical_tasks, historical_runs))
}

fn device_can_run(dev: &SimDevice, item: &SimItem) -> bool {
    if dev.drain {
        return false;
    }
    for taint in &dev.taints {
        if taint.effect == TaintEffect::NoSchedule && !is_tolerated(taint, &item.tolerations) {
            return false;
        }
    }
    for req_cap in &item.requires {
        if !dev.capabilities.contains(req_cap) {
            return false;
        }
    }
    let runner_ok = match item.kind {
        RunKind::Merge | RunKind::Housekeeping => dev.runners.iter().any(|r| r == "deterministic"),
        RunKind::Command => {
            let want = item.preferred_runner.as_deref().unwrap_or("deterministic");
            dev.runners.iter().any(|r| r == want)
        }
        _ => {
            if let Some(ref pref) = item.preferred_runner
                && dev.runners.iter().any(|r| r == pref)
            {
                true
            } else {
                dev.runners.iter().any(|r| r == "acp" || r == "t3")
            }
        }
    };
    if !runner_ok {
        return false;
    }
    if item.needs_local_repo
        && let Some(ref repo) = item.repo
    {
        let has_warm = dev.repos.iter().any(|r| r == repo);
        let cloneable = dev.capabilities.iter().any(|c| c == "git:cloneable");
        if !has_warm && !cloneable {
            return false;
        }
    }
    true
}

#[derive(Debug)]
struct ActiveSimRun {
    finish_at: i64,
    device: DeviceName,
    board: BoardSlug,
    kind: RunKind,
    cost_units: u32,
    conflict_keys: Vec<ConflictKeySpec>,
}

fn simulate(
    items: &[SimItem],
    boards: &BTreeMap<BoardSlug, SimBoard>,
    devices: &[SimDevice],
    policy_version: &str,
    weights: &SchedulerWeights,
    until_ms: i64,
) -> ReplayMetrics {
    if items.is_empty() || devices.is_empty() || items[0].ready_at_ms > until_ms {
        return ReplayMetrics {
            policy_version: policy_version.to_owned(),
            tasks_scheduled: 0,
            p50_wait_secs: None,
            p90_wait_secs: None,
            fair_share_gini: 0.0,
            conflict_blocks: 0,
            backfill_count: 0,
            locality_hits: 0,
        };
    }

    let mut next_arrival: usize = 0;
    let mut pending: Vec<usize> = Vec::new();
    let mut active: Vec<ActiveSimRun> = Vec::new();
    let mut used_units: BTreeMap<DeviceName, u32> = BTreeMap::new();
    let mut board_workers: BTreeMap<BoardSlug, usize> = BTreeMap::new();
    let mut conflict_locks: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    // Rolling 24h consumption history: (started_at_ms, board, cost_units).
    let mut consumption: Vec<(i64, BoardSlug, u32)> = Vec::new();

    let mut waits_secs: Vec<f64> = Vec::new();
    // Include every board with arriving workload in the window so starved boards count as 0.0.
    let mut board_wum: BTreeMap<BoardSlug, f64> = BTreeMap::new();
    for item in items {
        if item.ready_at_ms <= until_ms {
            board_wum.entry(item.board.clone()).or_insert(0.0);
        }
    }
    let mut conflict_blocks: u32 = 0;
    let mut backfill_count: u32 = 0;
    let mut locality_hits: u32 = 0;

    let mut sim_time = items[0].ready_at_ms;

    loop {
        // 1. Complete finished runs at or before `sim_time`.
        let mut still_active = Vec::with_capacity(active.len());
        for run in active {
            if run.finish_at <= sim_time {
                if let Some(u) = used_units.get_mut(&run.device) {
                    *u = u.saturating_sub(run.cost_units);
                }
                if run.kind == RunKind::Worker
                    && let Some(c) = board_workers.get_mut(&run.board)
                {
                    *c = c.saturating_sub(1);
                }
                for spec in &run.conflict_keys {
                    if let Some((holders, _)) = conflict_locks.get_mut(&spec.key) {
                        *holders = holders.saturating_sub(1);
                        if *holders == 0 {
                            conflict_locks.remove(&spec.key);
                        }
                    }
                }
            } else {
                still_active.push(run);
            }
        }
        active = still_active;

        // 2. Admit newly arrived items.
        while next_arrival < items.len() && items[next_arrival].ready_at_ms <= sim_time {
            pending.push(next_arrival);
            next_arrival += 1;
        }

        // Prune rolling 24h consumption window.
        let fair_cutoff = sim_time.saturating_sub(FAIR_SHARE_WINDOW_MS);
        consumption.retain(|(at, _, _)| *at >= fair_cutoff);

        // 3. Schedule greedily across available devices until no device can claim another item.
        let mut conflict_blocked_this_tick: BTreeSet<usize> = BTreeSet::new();
        loop {
            let mut scheduled_any = false;

            // Deterministic device order: highest free units, highest preference, then name.
            let mut dev_order: Vec<usize> = (0..devices.len())
                .filter(|&i| {
                    !devices[i].drain
                        && devices[i]
                            .capacity_units
                            .saturating_sub(used_units.get(&devices[i].name).copied().unwrap_or(0))
                            > 0
                })
                .collect();
            dev_order.sort_by(|&a, &b| {
                let fa = devices[a]
                    .capacity_units
                    .saturating_sub(used_units.get(&devices[a].name).copied().unwrap_or(0));
                let fb = devices[b]
                    .capacity_units
                    .saturating_sub(used_units.get(&devices[b].name).copied().unwrap_or(0));
                fb.cmp(&fa)
                    .then_with(|| devices[b].preference.cmp(&devices[a].preference))
                    .then_with(|| devices[a].name.cmp(&devices[b].name))
            });

            // Compute current 24h fair-share totals once per scheduling round.
            let mut recent_board_units: BTreeMap<BoardSlug, u64> = BTreeMap::new();
            let mut total_recent_units: u64 = 0;
            for (_, b, u) in &consumption {
                *recent_board_units.entry(b.clone()).or_insert(0) += u64::from(*u);
                total_recent_units += u64::from(*u);
            }

            for dev_idx in dev_order {
                let dev = &devices[dev_idx];
                let free = dev
                    .capacity_units
                    .saturating_sub(used_units.get(&dev.name).copied().unwrap_or(0));
                if free == 0 {
                    continue;
                }

                let mut eligible: Vec<(usize, board_proto::ScoreBreakdown)> = Vec::new();
                for (pos, &item_idx) in pending.iter().enumerate() {
                    let item = &items[item_idx];
                    let Some(board) = boards.get(&item.board) else {
                        continue;
                    };
                    if item.kind == RunKind::Worker {
                        let in_flight = board_workers.get(&item.board).copied().unwrap_or(0);
                        if in_flight >= board.parallel_tasks as usize {
                            continue;
                        }
                    }
                    let conflict_blocked = item.conflict_keys.iter().any(|spec| {
                        conflict_locks
                            .get(&spec.key)
                            .is_some_and(|&(h, m)| h >= m.min(spec.max))
                    });
                    if conflict_blocked {
                        conflict_blocked_this_tick.insert(item_idx);
                        continue;
                    }
                    if !device_can_run(dev, item) {
                        continue;
                    }
                    let board_recent = recent_board_units.get(&item.board).copied().unwrap_or(0);
                    let breakdown = score_candidate_base(
                        &ScoreInputs {
                            now_ms: sim_time,
                            kind: item.kind,
                            priority: item.priority,
                            rank: item.rank,
                            created_at_ms: item.ready_at_ms,
                            weight_tier: item.weight_tier,
                            repo: item.repo.as_deref(),
                            tolerations: &item.tolerations,
                            affinity: item.affinity.as_ref(),
                            board_home_devices: &board.home_devices,
                            board_recent_units: board_recent,
                            total_recent_units,
                            device_name: &dev.name,
                            device_preference: dev.preference,
                            device_taints: &dev.taints,
                            device_repos: &dev.repos,
                        },
                        weights,
                    );
                    eligible.push((pos, breakdown));
                }

                let mut max_blocked_score = f64::NEG_INFINITY;
                let mut fitting: Vec<(usize, board_proto::ScoreBreakdown)> = Vec::new();
                for (pos, breakdown) in eligible {
                    let item = &items[pending[pos]];
                    if item.cost_units > free {
                        let total = breakdown.total();
                        if total > max_blocked_score {
                            max_blocked_score = total;
                        }
                    } else {
                        fitting.push((pos, breakdown));
                    }
                }
                if fitting.is_empty() {
                    continue;
                }
                for (pos, breakdown) in &mut fitting {
                    if max_blocked_score.is_finite() && max_blocked_score > breakdown.total() {
                        let item = &items[pending[*pos]];
                        breakdown.backfill = weights.w_backfill / f64::from(item.cost_units.max(1));
                    }
                }
                fitting.sort_by(|(pa, sa), (pb, sb)| {
                    let a = &items[pending[*pa]];
                    let b = &items[pending[*pb]];
                    sb.total()
                        .total_cmp(&sa.total())
                        .then_with(|| a.ready_at_ms.cmp(&b.ready_at_ms))
                        .then_with(|| a.board.cmp(&b.board))
                        .then_with(|| a.key.cmp(&b.key))
                });

                let (win_pos, breakdown) = fitting.remove(0);
                let item_idx = pending.remove(win_pos);
                let item = &items[item_idx];

                *used_units.entry(dev.name.clone()).or_insert(0) += item.cost_units;
                if item.kind == RunKind::Worker {
                    *board_workers.entry(item.board.clone()).or_insert(0) += 1;
                }
                for spec in &item.conflict_keys {
                    let entry = conflict_locks
                        .entry(spec.key.clone())
                        .or_insert((0, spec.max));
                    entry.0 += 1;
                    entry.1 = entry.1.min(spec.max);
                }
                consumption.push((sim_time, item.board.clone(), item.cost_units));
                *recent_board_units.entry(item.board.clone()).or_insert(0) +=
                    u64::from(item.cost_units);
                total_recent_units += u64::from(item.cost_units);

                waits_secs.push((sim_time - item.ready_at_ms).max(0) as f64 / 1000.0);
                let wum = f64::from(item.cost_units) * (item.duration_ms as f64 / 60_000.0);
                *board_wum.entry(item.board.clone()).or_insert(0.0) += wum;
                if breakdown.backfill > 0.0 {
                    backfill_count += 1;
                }
                if breakdown.locality > 0.0 {
                    locality_hits += 1;
                }
                active.push(ActiveSimRun {
                    finish_at: sim_time.saturating_add(item.duration_ms),
                    device: dev.name.clone(),
                    board: item.board.clone(),
                    kind: item.kind,
                    cost_units: item.cost_units,
                    conflict_keys: item.conflict_keys.clone(),
                });
                scheduled_any = true;
            }

            if !scheduled_any {
                break;
            }
        }
        conflict_blocks = conflict_blocks.saturating_add(conflict_blocked_this_tick.len() as u32);

        // 4. Advance simulated time to the next event (earliest run completion or next arrival)
        //    within `[since_ms, until_ms]`.
        let next_finish = active.iter().map(|r| r.finish_at).min();
        let next_arrive = items.get(next_arrival).map(|i| i.ready_at_ms);
        let next_time = match (next_finish, next_arrive) {
            (Some(f), Some(a)) => f.min(a).max(sim_time),
            (Some(f), None) => f.max(sim_time),
            (None, Some(a)) => a.max(sim_time),
            (None, None) => break,
        };
        if next_time > until_ms {
            break;
        }
        sim_time = next_time;
    }

    let gini_values: Vec<f64> = board_wum.values().copied().collect();
    ReplayMetrics {
        policy_version: policy_version.to_owned(),
        tasks_scheduled: waits_secs.len() as u32,
        p50_wait_secs: median_of(waits_secs.clone()),
        p90_wait_secs: percentile_of(waits_secs, 0.9),
        fair_share_gini: gini_coefficient(&gini_values),
        conflict_blocks,
        backfill_count,
        locality_hits,
    }
}

/// Gini coefficient of non-negative values (`0.0` = completely equal, approaches `1.0` = max skew).
fn gini_coefficient(values: &[f64]) -> f64 {
    let n = values.len();
    if n <= 1 {
        return 0.0;
    }
    let sum: f64 = values.iter().sum();
    if sum <= 0.0 {
        return 0.0;
    }
    let mut abs_diff_sum = 0.0;
    for &x in values {
        for &y in values {
            abs_diff_sum += (x - y).abs();
        }
    }
    (abs_diff_sum / (2.0 * (n as f64) * sum)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::gini_coefficient;

    #[test]
    fn gini_coefficient_bounds() {
        assert_eq!(gini_coefficient(&[]), 0.0);
        assert_eq!(gini_coefficient(&[10.0]), 0.0);
        assert!((gini_coefficient(&[10.0, 10.0, 10.0]) - 0.0).abs() < 1e-9);
        assert!((gini_coefficient(&[0.0, 0.0, 10.0]) - (2.0 / 3.0)).abs() < 1e-9);
    }
}
