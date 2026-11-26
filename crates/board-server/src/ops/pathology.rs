//! Automated scheduling pathology signals (`docs/SCHEDULER_DESIGN.md`, Section 6.2).
//!
//! Runs periodically in the server sweeper with an injectable clock, detecting chronic
//! pathologies and filing deduplicated `FeedbackSource::Signal` items on `feedback_board`
//! while using `pathology_state` to prevent 60-second sweeper alert storms.

use std::collections::{BTreeMap, HashMap, HashSet};

use board_proto::{
    AgentId, Board, BoardSlug, BoardStatus, DeviceEntry, DeviceName, DeviceStatus,
    FEEDBACK_SIGNATURE_MAX, RunKind, TaintEffect, Task, TaskId,
};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::auth::hex;
use crate::db::{Ctx, parse_id, ready_sql};
use crate::error::Result;
use crate::ops::devices::list_entries as list_device_entries;
use crate::ops::dispatch::{effective_device_taints, is_tolerated, task_weight_and_units};
use crate::ops::feedback::file_signal;
use crate::ops::runs::last_ready_ms_for_run;
use crate::store::{list_boards, load_tasks};

/// Minimum interval between repeated reports for the same pathology signature (1 hour).
const PATHOLOGY_COOLDOWN_MS: i64 = 3_600_000;
/// Rolling window for orphan storms and runner spawn failures (1 hour).
const ROLLING_HOUR_MS: i64 = 3_600_000;
/// Rolling window for conflict-key contention ratio (24 hours).
const ROLLING_DAY_MS: i64 = 24 * 3_600_000;
/// Maximum running duration (ms) for a failed run to count as a fast spawn failure (30s).
const FAST_SPAWN_FAIL_MS: i64 = 30_000;
/// Minimum conflict-key holds in 24h before `conflict_hot` can trigger.
const CONFLICT_HOT_MIN_HOLDS: u32 = 3;
/// Minimum distinct UTC hours with conflict-key holds in 24h before `conflict_hot` can trigger.
const CONFLICT_HOT_MIN_HELD_HOURS: u32 = 3;
/// Minimum non-duplicate/non-shipped agent drops in 24h on one board before `drop_burst` fires.
const DROP_BURST_MIN_DROPS: usize = 5;
/// Minimum `cut: ..., dropped:` verdict clauses in 24h on one board before `drop_burst` fires.
const DROP_BURST_MIN_CUTS: usize = 3;

/// Summary of one [`sweep_pathologies`] execution (used for logging and tests).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PathologySweepSummary {
    pub filed: Vec<TaskId>,
    pub updated: Vec<TaskId>,
    pub skipped_cooldown: usize,
}

fn server_reporter() -> AgentId {
    "dispatch:server"
        .parse()
        .expect("dispatch:server is a valid AgentId")
}

fn device_reporter(device: &DeviceName) -> AgentId {
    AgentId::dispatch(device)
}

fn bounded_signature(prefix: &str, suffix: &str) -> String {
    let raw = format!("{prefix}:{suffix}");
    if raw.chars().count() <= FEEDBACK_SIGNATURE_MAX {
        return raw;
    }
    let digest = hex(&Sha256::digest(raw.as_bytes()));
    let keep = FEEDBACK_SIGNATURE_MAX.saturating_sub(13);
    let head: String = raw.chars().take(keep).collect();
    format!("{head}:{}", &digest[..12])
}

struct SignalEmission<'a> {
    kind: &'a str,
    about: &'a str,
    signature: String,
    title: String,
    body: String,
    summary: String,
    reporter: AgentId,
    first_seen_at: i64,
}

fn emit_with_cooldown(
    cx: &mut Ctx<'_>,
    sig: SignalEmission<'_>,
    out: &mut PathologySweepSummary,
) -> Result<()> {
    let now = cx.now_ms();
    let existing: Option<(Option<i64>, Option<i64>)> = cx
        .db
        .prepare_cached(
            "SELECT first_seen_at, last_filed_at FROM pathology_state WHERE signature = ?1",
        )?
        .query_row([&sig.signature], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;

    let first_seen = existing
        .and_then(|(f, _)| f)
        .map_or(sig.first_seen_at, |f| f.min(sig.first_seen_at));
    if let Some((_, Some(last_filed))) = existing
        && now.saturating_sub(last_filed) < PATHOLOGY_COOLDOWN_MS
    {
        cx.db.execute(
            "UPDATE pathology_state SET last_seen_at = ?2 WHERE signature = ?1",
            params![sig.signature, now],
        )?;
        out.skipped_cooldown += 1;
        return Ok(());
    }

    let (task_id, created) = file_signal(
        cx,
        sig.about,
        &sig.signature,
        &sig.title,
        &sig.body,
        &sig.summary,
        &sig.reporter,
    )?;
    if created {
        out.filed.push(task_id);
    } else {
        out.updated.push(task_id);
    }

    cx.db.execute(
        "INSERT INTO pathology_state (signature, kind, first_seen_at, last_seen_at, last_filed_at) \
         VALUES (?1, ?2, ?3, ?4, ?4) \
         ON CONFLICT(signature) DO UPDATE SET \
             first_seen_at = COALESCE(pathology_state.first_seen_at, excluded.first_seen_at), \
             last_seen_at = excluded.last_seen_at, \
             last_filed_at = excluded.last_filed_at",
        params![sig.signature, sig.kind, first_seen, now],
    )?;
    Ok(())
}

/// Runs all 6 automated scheduler/board pathology detectors against the current clock and
/// database state.
pub(crate) fn sweep_pathologies(cx: &mut Ctx<'_>) -> Result<PathologySweepSummary> {
    let mut out = PathologySweepSummary::default();
    detect_unschedulable(cx, &mut out)?;
    detect_starvation(cx, &mut out)?;
    detect_orphan_storm(cx, &mut out)?;
    detect_conflict_hot(cx, &mut out)?;
    detect_spawn_fail(cx, &mut out)?;
    detect_drop_burst(cx, &mut out)?;
    Ok(out)
}

fn device_matches_worker_task(
    device: &DeviceEntry,
    task: &Task,
    repo: Option<&str>,
    cost_units: u32,
) -> bool {
    let taints = effective_device_taints(device);
    for taint in &taints {
        if taint.effect == TaintEffect::NoSchedule && !is_tolerated(taint, &task.tolerations) {
            return false;
        }
    }
    for req_cap in &task.requires {
        if !device.capabilities.contains(req_cap) {
            return false;
        }
    }
    let has_runner = if let Some(ref pref) = task.preferred_runner {
        device.runners.iter().any(|r| r == pref)
            || device.runners.iter().any(|r| r == "acp" || r == "t3")
    } else {
        device.runners.iter().any(|r| r == "acp" || r == "t3")
    };
    if !has_runner {
        return false;
    }
    if let Some(repo) = repo {
        let has_warm = device.repos.iter().any(|r| r == repo);
        let cloneable = device.capabilities.iter().any(|c| c == "git:cloneable");
        if !has_warm && !cloneable {
            return false;
        }
    }
    if let Some(cap) = device.capacity
        && cap.weight_units < cost_units
    {
        return false;
    }
    true
}

/// 1. `signal:scheduler:unschedulable` — ready `auto` task on an active autopilot board
///    matches zero online untainted non-draining devices for > `unschedulable_after_secs`.
fn detect_unschedulable(cx: &mut Ctx<'_>, out: &mut PathologySweepSummary) -> Result<()> {
    let now = cx.now_ms();
    let threshold_ms = cx.s.unschedulable_after_ms();

    let boards: HashMap<BoardSlug, Board> = list_boards(cx)?
        .into_iter()
        .filter(|b| b.status == BoardStatus::Active && b.autopilot)
        .map(|b| (b.slug.clone(), b))
        .collect();
    if boards.is_empty() {
        return Ok(());
    }

    let online_devices: Vec<DeviceEntry> = list_device_entries(cx)?
        .into_iter()
        .filter(|d| d.status == DeviceStatus::Online && !d.drain)
        .collect();

    let filter = format!(
        "{} AND t.auto = 1 \
         AND NOT EXISTS (SELECT 1 FROM feedback rf WHERE rf.task = t.id) \
         AND EXISTS (SELECT 1 FROM boards b WHERE b.slug = t.board AND b.status = 'active' AND b.autopilot = 1) \
         AND NOT EXISTS ( \
             SELECT 1 FROM runs r \
             WHERE r.task_id = t.id AND r.kind = 'worker' \
               AND r.state IN ('queued', 'leased', 'starting', 'running', 'waiting_external') \
         )",
        ready_sql("t")
    );
    let ready_tasks = load_tasks(cx, &filter, &Vec::new())?;

    let mut active_unschedulable_sigs: HashSet<String> = HashSet::new();
    for task in ready_tasks {
        let Some(board) = boards.get(&task.board) else {
            continue;
        };
        let repo = task
            .repo
            .as_deref()
            .or_else(|| board.repos.first().map(String::as_str));
        let (_, cost_units) = task_weight_and_units(&task);
        let any_match = online_devices
            .iter()
            .any(|dev| device_matches_worker_task(dev, &task, repo, cost_units));
        if any_match {
            continue;
        }

        let signature = bounded_signature("signal:scheduler:unschedulable", task.id.as_str());
        active_unschedulable_sigs.insert(signature.clone());

        // Track consecutive unschedulable duration starting from when zero online devices
        // first matched this task (not when the task was created), preserving `last_filed_at`.
        let prev_first_seen: Option<i64> = cx
            .db
            .prepare_cached("SELECT first_seen_at FROM pathology_state WHERE signature = ?1")?
            .query_row([&signature], |r| r.get::<_, Option<i64>>(0))
            .optional()?
            .flatten();
        let observed_since = prev_first_seen.unwrap_or(now);
        cx.db.execute(
            "INSERT INTO pathology_state (signature, kind, first_seen_at, last_seen_at, last_filed_at) \
             VALUES (?1, 'unschedulable', ?2, ?3, NULL) \
             ON CONFLICT(signature) DO UPDATE SET \
                 first_seen_at = COALESCE(pathology_state.first_seen_at, excluded.first_seen_at), \
                 last_seen_at = excluded.last_seen_at",
            params![signature, observed_since, now],
        )?;

        let waited_ms = now.saturating_sub(observed_since);
        if waited_ms < threshold_ms {
            continue;
        }

        let waited_mins = waited_ms / 60_000;
        let reqs = if task.requires.is_empty() {
            "none".to_owned()
        } else {
            task.requires.join(", ")
        };
        let title = format!(
            "scheduler: {} unschedulable for {}m (0/{} online devices match)",
            task.id,
            waited_mins,
            online_devices.len()
        );
        let body = format!(
            "{title}\n\nReady auto task {} on board {} has matched zero online untainted \
             non-draining devices for {} minutes (threshold: {}s).\n\n\
             Requires: {reqs}\n\
             Cost units: {cost_units}\n\
             Repo: {}\n\
             Online runner devices checked: {}",
            task.id,
            task.board,
            waited_mins,
            cx.s.unschedulable_after_secs,
            repo.unwrap_or("none"),
            online_devices.len(),
        );
        let summary = format!(
            "{} unschedulable for {}m (0/{} online devices match)",
            task.id,
            waited_mins,
            online_devices.len()
        );
        emit_with_cooldown(
            cx,
            SignalEmission {
                kind: "unschedulable",
                about: "scheduler",
                signature,
                title,
                body,
                summary,
                reporter: server_reporter(),
                first_seen_at: observed_since,
            },
            out,
        )?;
    }

    // Reset `first_seen_at` for tasks that became schedulable or closed, deleting the row
    // once its cooldown has expired (preserving `last_filed_at` while inside cooldown so a
    // flapping device cannot bypass the 1-hour deduplication window).
    let tracked: Vec<(String, Option<i64>)> = cx
        .db
        .prepare_cached(
            "SELECT signature, last_filed_at FROM pathology_state WHERE kind = 'unschedulable'",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (sig, last_filed_at) in tracked {
        if !active_unschedulable_sigs.contains(&sig) {
            let in_cooldown =
                last_filed_at.is_some_and(|lf| now.saturating_sub(lf) < PATHOLOGY_COOLDOWN_MS);
            if in_cooldown {
                cx.db.execute(
                    "UPDATE pathology_state SET first_seen_at = NULL WHERE signature = ?1",
                    [&sig],
                )?;
            } else {
                cx.db
                    .execute("DELETE FROM pathology_state WHERE signature = ?1", [&sig])?;
            }
        }
    }

    Ok(())
}

/// 2. `signal:scheduler:starvation` — an active autopilot board's ready `auto` task has
///    waited > `starvation_after_secs` (default 6h) while other boards claimed runs.
fn detect_starvation(cx: &mut Ctx<'_>, out: &mut PathologySweepSummary) -> Result<()> {
    let now = cx.now_ms();
    let window_ms = cx.s.starvation_after_ms();
    let from = now.saturating_sub(window_ms);

    let boards: HashMap<BoardSlug, Board> = list_boards(cx)?
        .into_iter()
        .filter(|b| b.status == BoardStatus::Active && b.autopilot)
        .map(|b| (b.slug.clone(), b))
        .collect();
    if boards.is_empty() {
        return Ok(());
    }

    let online_devices: Vec<DeviceEntry> = list_device_entries(cx)?
        .into_iter()
        .filter(|d| d.status == DeviceStatus::Online && !d.drain)
        .collect();

    // Load fresh usage readings so boards intentionally held by quota (`usage_stop_at` or
    // `pace_limit == 0`) are never falsely reported as starved.
    let fresh_usages: Vec<board_proto::Usage> = crate::ops::usage::list_buckets(cx)?
        .into_iter()
        .filter_map(|b| b.as_usage())
        .filter(|u| u.fresh(cx.now))
        .collect();

    // Count live worker occupancy per board (`doing` tasks union active managed worker runs)
    // so boards currently at their `parallel_tasks` cap are not falsely reported as starved.
    let mut board_occupied_tasks: HashMap<BoardSlug, HashSet<TaskId>> = HashMap::new();
    let mut board_taskless_workers: HashMap<BoardSlug, u32> = HashMap::new();
    {
        let mut stmt = cx
            .db
            .prepare_cached("SELECT board, id FROM tasks WHERE status = 'doing'")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let b: BoardSlug = parse_id(&r.get::<_, String>(0)?)?;
            let tid: TaskId = parse_id(&r.get::<_, String>(1)?)?;
            board_occupied_tasks.entry(b).or_default().insert(tid);
        }
    }
    {
        let mut stmt = cx.db.prepare_cached(
            "SELECT board, task_id FROM runs \
             WHERE board IS NOT NULL AND origin = 'managed' AND kind = 'worker' \
               AND state IN ('leased', 'starting', 'running', 'waiting_external') \
               AND (lease_expires_at IS NULL OR lease_expires_at > ?1)",
        )?;
        let mut rows = stmt.query([now])?;
        while let Some(r) = rows.next()? {
            let b: BoardSlug = parse_id(&r.get::<_, String>(0)?)?;
            if let Some(tid) = r.get::<_, Option<String>>(1)? {
                board_occupied_tasks
                    .entry(b)
                    .or_default()
                    .insert(parse_id(&tid)?);
            } else {
                *board_taskless_workers.entry(b).or_insert(0) += 1;
            }
        }
    }

    // Count tasks in `review` per board so boards intentionally paused by worker
    // review-backlog backpressure (`review_count >= review_backlog_limit`) are not
    // falsely reported as starved.
    let mut review_count_by_board: HashMap<BoardSlug, usize> = HashMap::new();
    {
        let mut stmt = cx.db.prepare_cached(
            "SELECT board, COUNT(*) FROM tasks WHERE status = 'review' GROUP BY board",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let b: BoardSlug = parse_id(&r.get::<_, String>(0)?)?;
            let count: i64 = r.get(1)?;
            review_count_by_board.insert(b, usize::try_from(count.max(0)).unwrap_or(0));
        }
    }

    let filter = format!(
        "{} AND t.auto = 1 \
         AND NOT EXISTS (SELECT 1 FROM feedback rf WHERE rf.task = t.id) \
         AND EXISTS (SELECT 1 FROM boards b WHERE b.slug = t.board AND b.status = 'active' AND b.autopilot = 1) \
         AND NOT EXISTS ( \
             SELECT 1 FROM runs r \
             WHERE r.task_id = t.id AND r.kind = 'worker' \
               AND r.state IN ('queued', 'leased', 'starting', 'running', 'waiting_external') \
         )",
        ready_sql("t")
    );
    let ready_tasks = load_tasks(cx, &filter, &Vec::new())?;
    if ready_tasks.is_empty() {
        return Ok(());
    }

    // Group schedulable ready tasks past `starvation_after_ms` by board, keeping the oldest.
    struct BoardWait {
        oldest_task: TaskId,
        oldest_ready_at: i64,
        waiting_count: u32,
    }
    let mut starved_candidates: BTreeMap<BoardSlug, BoardWait> = BTreeMap::new();
    for task in ready_tasks {
        let Some(board) = boards.get(&task.board) else {
            continue;
        };
        if !fresh_usages.is_empty()
            && fresh_usages.iter().all(|u| {
                u.over(board.usage_stop_at, cx.now).is_some()
                    || u.pace_limit(board.parallel_tasks, cx.now) == 0
            })
        {
            continue;
        }
        let occupied = board_occupied_tasks
            .get(&task.board)
            .map_or(0, HashSet::len) as u32
            + board_taskless_workers
                .get(&task.board)
                .copied()
                .unwrap_or(0);
        if occupied >= board.parallel_tasks {
            continue;
        }
        let review_count = review_count_by_board.get(&task.board).copied().unwrap_or(0);
        if review_count >= crate::ops::dispatch::review_backlog_limit(board.parallel_tasks) {
            continue;
        }
        let repo = task
            .repo
            .as_deref()
            .or_else(|| board.repos.first().map(String::as_str));
        let (_, cost_units) = task_weight_and_units(&task);
        let any_match = online_devices
            .iter()
            .any(|dev| device_matches_worker_task(dev, &task, repo, cost_units));
        if !any_match {
            continue;
        }
        let ready_at = last_ready_ms_for_run(cx, &task.id, RunKind::Worker, now, None)?
            .unwrap_or_else(|| task.created_at.timestamp_millis());
        if now.saturating_sub(ready_at) < window_ms {
            continue;
        }
        let entry = starved_candidates
            .entry(task.board.clone())
            .or_insert_with(|| BoardWait {
                oldest_task: task.id.clone(),
                oldest_ready_at: ready_at,
                waiting_count: 0,
            });
        entry.waiting_count += 1;
        if ready_at < entry.oldest_ready_at {
            entry.oldest_ready_at = ready_at;
            entry.oldest_task = task.id;
        }
    }
    if starved_candidates.is_empty() {
        return Ok(());
    }

    // Count recent managed `worker` runs per board in `[from, now]` (excluding non-worker
    // judge/curate/triage/merge/housekeeping runs so brief maintenance runs on other boards
    // never trigger false worker-starvation alerts).
    let mut runs_by_board: HashMap<BoardSlug, u32> = HashMap::new();
    let mut total_recent_runs: u32 = 0;
    let mut stmt = cx.db.prepare_cached(
        "SELECT board, COUNT(*) FROM runs \
         WHERE board IS NOT NULL AND origin = 'managed' \
           AND kind = 'worker' \
           AND created_at >= ?1 \
         GROUP BY board",
    )?;
    let mut rows = stmt.query([from])?;
    while let Some(r) = rows.next()? {
        let slug: BoardSlug = parse_id(&r.get::<_, String>(0)?)?;
        let count: u32 = r.get(1)?;
        runs_by_board.insert(slug, count);
        total_recent_runs = total_recent_runs.saturating_add(count);
    }
    drop(rows);

    // Skip any board that was held by quota within `[from, now]` so lifting weekly pace
    // control never fires spurious wake-up starvation alerts before the board has had a
    // full unheld starvation window to claim work.
    let mut quota_held_boards: HashSet<BoardSlug> = HashSet::new();
    {
        let mut stmt = cx.db.prepare_cached(
            "SELECT DISTINCT board FROM scheduler_holds \
             WHERE board IS NOT NULL AND reason = 'quota' AND at >= ?1",
        )?;
        let mut rows = stmt.query([from])?;
        while let Some(r) = rows.next()? {
            quota_held_boards.insert(parse_id(&r.get::<_, String>(0)?)?);
        }
    }

    for (board, wait) in starved_candidates {
        if quota_held_boards.contains(&board) {
            continue;
        }
        let board_runs = runs_by_board.get(&board).copied().unwrap_or(0);
        let other_runs = total_recent_runs.saturating_sub(board_runs);
        if board_runs > 0 || other_runs == 0 {
            continue;
        }
        let waited_hours = (now.saturating_sub(wait.oldest_ready_at) as f64) / 3_600_000.0;
        let signature = bounded_signature("signal:scheduler:starvation", board.as_str());
        let title = format!(
            "scheduler: board {board} starved for {waited_hours:.1}h ({other_runs} worker runs on other boards)"
        );
        let body = format!(
            "{title}\n\nBoard {board} has {} ready auto task(s) (oldest: {}, waiting \
             {waited_hours:.1}h >= threshold {}s) with 0 managed worker runs claimed in the window \
             while other boards claimed {other_runs} managed worker run(s).\n\n\
             Inspect `GET /v1/stats/scheduler` and consider raising `w_age` or `w_fair` via \
             `board admin replay-schedule`.",
            wait.waiting_count, wait.oldest_task, cx.s.starvation_after_secs,
        );
        let summary = format!(
            "board {board} starved {waited_hours:.1}h ({} ready, 0 worker runs vs {other_runs} on other boards)",
            wait.waiting_count
        );
        emit_with_cooldown(
            cx,
            SignalEmission {
                kind: "starvation",
                about: "scheduler",
                signature,
                title,
                body,
                summary,
                reporter: server_reporter(),
                first_seen_at: wait.oldest_ready_at,
            },
            out,
        )?;
    }
    Ok(())
}

/// 3. `signal:scheduler:orphan_storm` — a device orphaned >= `orphan_storm_count` (default 3)
///    runs within a rolling 1-hour window.
fn detect_orphan_storm(cx: &mut Ctx<'_>, out: &mut PathologySweepSummary) -> Result<()> {
    let now = cx.now_ms();
    let from = now.saturating_sub(ROLLING_HOUR_MS);
    let threshold = cx.s.orphan_storm_count;

    let rows: Vec<(DeviceName, u32, i64)> = cx
        .db
        .prepare_cached(
            "SELECT device, COUNT(*), MIN(COALESCE(ended_at, updated_at)) FROM runs \
             WHERE state = 'orphaned' AND COALESCE(ended_at, updated_at) >= ?1 \
             GROUP BY device HAVING COUNT(*) >= ?2 ORDER BY device",
        )?
        .query_map(params![from, threshold], |r| {
            Ok((parse_id(&r.get::<_, String>(0)?)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    for (device, count, first_at) in rows {
        let run_ids: Vec<String> = cx
            .db
            .prepare_cached(
                "SELECT id FROM runs \
                 WHERE device = ?1 AND state = 'orphaned' AND COALESCE(ended_at, updated_at) >= ?2 \
                 ORDER BY COALESCE(ended_at, updated_at) DESC LIMIT 10",
            )?
            .query_map(params![device.as_str(), from], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;

        let signature = bounded_signature("signal:scheduler:orphan_storm", device.as_str());
        let title = format!("scheduler: orphan storm on {device} ({count} runs in 1h)");
        let body = format!(
            "{title}\n\nDevice {device} orphaned {count} run(s) within the last 1 hour \
             (threshold: {threshold}).\n\n\
             Recent orphaned runs: {}\n\
             Check device sleep/network stability, daemon lease renewal heartbeats, or \
             `lease_ttl_secs`.",
            run_ids.join(", "),
        );
        let summary = format!("device {device} orphaned {count} runs in rolling 1h window");
        emit_with_cooldown(
            cx,
            SignalEmission {
                kind: "orphan_storm",
                about: "scheduler",
                signature,
                title,
                body,
                summary,
                reporter: device_reporter(&device),
                first_seen_at: first_at,
            },
            out,
        )?;
    }
    Ok(())
}

/// 4. `signal:scheduler:conflict_hot` — a `conflict_key` held claims for >= `conflict_hot_percent`
///    (default 30%) of active hours over a rolling 24h window (with at least 3 holds).
fn detect_conflict_hot(cx: &mut Ctx<'_>, out: &mut PathologySweepSummary) -> Result<()> {
    let now = cx.now_ms();
    let from = now.saturating_sub(ROLLING_DAY_MS);
    let pct_threshold = f64::from(cx.s.conflict_hot_percent);

    let active_hours: u32 = cx.db.query_row(
        "SELECT COUNT(*) FROM ( \
             SELECT created_at / 3600000 AS hr FROM runs WHERE created_at >= ?1 \
             UNION \
             SELECT at / 3600000 AS hr FROM scheduler_holds WHERE at >= ?1 \
         )",
        [from],
        |r| r.get(0),
    )?;
    if active_hours == 0 {
        return Ok(());
    }

    let rows: Vec<(String, u32, u32, i64)> = cx
        .db
        .prepare_cached(
            "SELECT conflict_key, COUNT(*), COUNT(DISTINCT at / 3600000), MIN(at) \
             FROM scheduler_holds \
             WHERE reason = 'conflict_key' AND conflict_key IS NOT NULL AND at >= ?1 \
             GROUP BY conflict_key \
             HAVING COUNT(*) >= ?2 AND COUNT(DISTINCT at / 3600000) >= ?3 \
             ORDER BY COUNT(*) DESC, conflict_key",
        )?
        .query_map(
            params![from, CONFLICT_HOT_MIN_HOLDS, CONFLICT_HOT_MIN_HELD_HOURS],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;

    for (key, holds, held_hours, first_at) in rows {
        let ratio_pct = (f64::from(held_hours) / f64::from(active_hours)) * 100.0;
        if ratio_pct < pct_threshold {
            continue;
        }
        let signature = bounded_signature("signal:scheduler:conflict_hot", &key);
        let title = format!(
            "scheduler: conflict key {key} hot ({ratio_pct:.0}% of active hours, {holds} holds)"
        );
        let body = format!(
            "{title}\n\nConflict key `{key}` blocked claims in {held_hours}/{active_hours} \
             active hours ({ratio_pct:.1}% >= {pct_threshold:.0}%) with {holds} recorded hold(s) \
             over the last 24 hours.\n\n\
             Consider shortening merge/write lock hold times or tuning concurrency caps."
        );
        let summary = format!(
            "conflict key {key} held {held_hours}/{active_hours} active hours ({ratio_pct:.1}%, {holds} holds)"
        );
        emit_with_cooldown(
            cx,
            SignalEmission {
                kind: "conflict_hot",
                about: "scheduler",
                signature,
                title,
                body,
                summary,
                reporter: server_reporter(),
                first_seen_at: first_at,
            },
            out,
        )?;
    }
    Ok(())
}

/// 5. `signal:runner:spawn_fail` — a `(device, runner)` pair had >= `spawn_fail_count`
///    (default 3) fast `failed` runs (`running_secs <= 30` or never started) within 1 hour.
fn detect_spawn_fail(cx: &mut Ctx<'_>, out: &mut PathologySweepSummary) -> Result<()> {
    let now = cx.now_ms();
    let from = now.saturating_sub(ROLLING_HOUR_MS);
    let threshold = cx.s.spawn_fail_count;

    let rows: Vec<(DeviceName, String, u32, i64)> = cx
        .db
        .prepare_cached(
            "SELECT device, runner, COUNT(*), MIN(COALESCE(ended_at, updated_at)) FROM runs \
             WHERE state = 'failed' \
               AND COALESCE(ended_at, updated_at) >= ?1 \
               AND (started_at IS NULL OR (COALESCE(ended_at, updated_at) - started_at) <= ?2) \
             GROUP BY device, runner \
             HAVING COUNT(*) >= ?3 \
             ORDER BY device, runner",
        )?
        .query_map(params![from, FAST_SPAWN_FAIL_MS, threshold], |r| {
            Ok((
                parse_id(&r.get::<_, String>(0)?)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    for (device, runner, count, first_at) in rows {
        let run_ids: Vec<String> = cx
            .db
            .prepare_cached(
                "SELECT id FROM runs \
                 WHERE device = ?1 AND runner = ?2 AND state = 'failed' \
                   AND COALESCE(ended_at, updated_at) >= ?3 \
                   AND (started_at IS NULL OR (COALESCE(ended_at, updated_at) - started_at) <= ?4) \
                 ORDER BY COALESCE(ended_at, updated_at) DESC LIMIT 10",
            )?
            .query_map(
                params![device.as_str(), &runner, from, FAST_SPAWN_FAIL_MS],
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?;

        let signature =
            bounded_signature("signal:runner:spawn_fail", &format!("{device}:{runner}"));
        let title = format!("runner: {runner} on {device} failed fast {count} times in 1h");
        let body = format!(
            "{title}\n\nRunner `{runner}` on device `{device}` had {count} fast failed run(s) \
             (<= 30s or never started) within the last 1 hour (threshold: {threshold}).\n\n\
             Recent failed runs: {}\n\
             Verify binary availability, auth/env, and local worktree state on `{device}`.",
            run_ids.join(", "),
        );
        let summary = format!("runner {runner} on {device} had {count} fast spawn failures in 1h");
        emit_with_cooldown(
            cx,
            SignalEmission {
                kind: "spawn_fail",
                about: "runner",
                signature,
                title,
                body,
                summary,
                reporter: device_reporter(&device),
                first_seen_at: first_at,
            },
            out,
        )?;
    }
    Ok(())
}

/// 6. `signal:board:drop_burst` — a board saw >= `DROP_BURST_MIN_DROPS` (5) non-duplicate /
///    non-shipped agent drops OR >= `DROP_BURST_MIN_CUTS` (3) `cut: ..., dropped:` verdict
///    clauses in the last 24 hours. Surfaces potential information loss to `board-feedback`
///    for automated healing audit.
fn detect_drop_burst(cx: &mut Ctx<'_>, out: &mut PathologySweepSummary) -> Result<()> {
    let now = cx.now_ms();
    let from = now.saturating_sub(ROLLING_DAY_MS);

    #[derive(Default)]
    struct BoardDropActivity {
        dropped_tasks: Vec<TaskId>,
        cut_clause_count: usize,
        cut_refs: Vec<String>,
        first_seen_at: Option<i64>,
    }

    let mut by_board: BTreeMap<BoardSlug, BoardDropActivity> = BTreeMap::new();

    let dropped_rows: Vec<(TaskId, BoardSlug, i64)> = cx
        .db
        .prepare_cached(
            "SELECT id, board, updated_at FROM tasks \
             WHERE status = 'dropped' AND updated_at >= ?1 \
               AND NOT EXISTS (SELECT 1 FROM feedback f \
                               WHERE f.task = tasks.id AND f.resolution IS NOT NULL) \
             ORDER BY updated_at DESC, number DESC",
        )?
        .query_map([from], |r| {
            Ok((
                parse_id(&r.get::<_, String>(0)?)?,
                parse_id(&r.get::<_, String>(1)?)?,
                r.get(2)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    for (task_id, board, updated_at) in dropped_rows {
        let decision: Option<String> = cx
            .db
            .prepare_cached(
                "SELECT text FROM notes \
                 WHERE task = ?1 AND kind = 'decision' \
                 ORDER BY at DESC, id DESC LIMIT 1",
            )?
            .query_row([task_id.as_str()], |r| r.get(0))
            .optional()?;
        // Direct user drops record no agent decision note; skip them.
        let Some(reason) = decision else {
            continue;
        };
        let lower = reason.to_ascii_lowercase();
        if lower.contains("already shipped")
            || lower.contains("duplicate")
            || lower.starts_with("verdict: merge")
        {
            continue;
        }
        let entry = by_board.entry(board).or_default();
        entry.dropped_tasks.push(task_id);
        entry.first_seen_at = Some(
            entry
                .first_seen_at
                .map_or(updated_at, |f| f.min(updated_at)),
        );
    }

    let cut_rows: Vec<(i64, TaskId, BoardSlug, i64, String)> = cx
        .db
        .prepare_cached(
            "SELECT n.id, n.task, t.board, n.at, n.text \
             FROM notes n \
             JOIN tasks t ON t.id = n.task \
             WHERE n.kind = 'decision' AND n.at >= ?1 \
               AND lower(ltrim(n.text)) LIKE 'verdict:%' \
               AND lower(n.text) LIKE '%cut:%' \
             ORDER BY n.at DESC, n.id DESC",
        )?
        .query_map([from], |r| {
            Ok((
                r.get(0)?,
                parse_id(&r.get::<_, String>(1)?)?,
                parse_id(&r.get::<_, String>(2)?)?,
                r.get(3)?,
                r.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    for (note_id, task_id, board, at, text) in cut_rows {
        let cut_lines = board_proto::extract_dropped_verdict_cuts(&text);
        if cut_lines.is_empty() {
            continue;
        }
        let entry = by_board.entry(board).or_default();
        entry.cut_clause_count += cut_lines.len();
        entry.cut_refs.push(format!("{task_id}#{note_id}"));
        entry.first_seen_at = Some(entry.first_seen_at.map_or(at, |f| f.min(at)));
    }

    for (board, activity) in by_board {
        let drop_count = activity.dropped_tasks.len();
        let cut_count = activity.cut_clause_count;
        if drop_count < DROP_BURST_MIN_DROPS && cut_count < DROP_BURST_MIN_CUTS {
            continue;
        }
        let dropped_preview: Vec<&str> = activity
            .dropped_tasks
            .iter()
            .take(10)
            .map(TaskId::as_str)
            .collect();
        let cuts_preview: Vec<&str> = activity
            .cut_refs
            .iter()
            .take(10)
            .map(String::as_str)
            .collect();
        let signature = bounded_signature("signal:board:drop_burst", board.as_str());
        let title = format!(
            "board: {board} had {drop_count} agent drops and {cut_count} dropped verdict cuts in 24h"
        );
        let body = format!(
            "{title}\n\nBoard `{board}` exceeded the 24h drop/cut audit threshold \
             (drops: {drop_count}/{DROP_BURST_MIN_DROPS}, dropped verdict cut clauses: \
             {cut_count}/{DROP_BURST_MIN_CUTS}).\n\n\
             Dropped tasks: {}\n\
             Verdict cut notes: {}\n\n\
             Audit `GET /v1/updates?board={board}` and re-file or cool to Icebox any valid \
             defect that was dropped due to bundle crossfire or a judge misread.",
            if dropped_preview.is_empty() {
                "none".to_owned()
            } else {
                dropped_preview.join(", ")
            },
            if cuts_preview.is_empty() {
                "none".to_owned()
            } else {
                cuts_preview.join(", ")
            },
        );
        let summary = format!(
            "board {board}: {drop_count} agent drops + {cut_count} dropped verdict cuts in 24h"
        );
        emit_with_cooldown(
            cx,
            SignalEmission {
                kind: "drop_burst",
                about: "board",
                signature,
                title,
                body,
                summary,
                reporter: server_reporter(),
                first_seen_at: activity.first_seen_at.unwrap_or(now),
            },
            out,
        )?;
    }
    Ok(())
}
