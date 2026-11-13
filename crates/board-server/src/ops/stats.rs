use std::collections::HashMap;

use board_proto::{AgentState, BoardStats, EvidenceKind, NewEvidence, Stats, Timestamp};
use rusqlite::types::ValueRef;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;

use crate::db::{Ctx, parse_id, parse_json, ts};
use crate::error::{Result, invalid};

/// The longest window stats read, in days. The server keeps the history they read
/// (events and refused claims) this long; see [`prune_history`].
pub(crate) const MAX_DAYS: u32 = 365;

/// The most rows of each table that one [`prune_history`] transaction deletes, so pruning
/// never holds the write lock for long.
pub(crate) const PRUNE_BATCH: usize = 1_000;

const DAY_MS: i64 = 86_400_000;

/// Deletes up to [`PRUNE_BATCH`] events and as many refused claims older than
/// [`MAX_DAYS`], oldest first. The newest event always stays, so the feed's position
/// (`max_seq`) holds. Returns how many events and refused claims went.
pub(crate) fn prune_history(cx: &Ctx<'_>) -> Result<(usize, usize)> {
    let before = cx.now_ms() - i64::from(MAX_DAYS) * 86_400_000;
    // Events are appended in time order, so the oldest by sequence are the old ones; taking
    // them by key reads at most one batch even when nothing is due.
    let events = cx.db.execute(
        "DELETE FROM events WHERE seq IN (SELECT seq FROM events ORDER BY seq LIMIT ?2) \
         AND at < ?1 AND seq < (SELECT MAX(seq) FROM events)",
        params![before, PRUNE_BATCH as i64],
    )?;
    let refusals = cx.db.execute(
        "DELETE FROM claim_refusals WHERE id IN \
         (SELECT id FROM claim_refusals WHERE at < ?1 ORDER BY at LIMIT ?2)",
        params![before, PRUNE_BATCH as i64],
    )?;
    Ok((events, refusals))
}

pub(crate) fn validate_days(days: Option<u32>) -> Result<u32> {
    match days.unwrap_or(7) {
        d @ 1..=MAX_DAYS => Ok(d),
        d => Err(invalid(
            format!("days={d} is out of range"),
            format!("send days between 1 and {MAX_DAYS}"),
        )),
    }
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    })
}

/// The value below which `share` of the values fall, by nearest rank.
fn percentile(mut values: Vec<f64>, share: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let rank = (share * values.len() as f64).ceil() as usize;
    Some(values[rank.clamp(1, values.len()) - 1])
}

/// The fields of a `task_done` payload that stats read.
#[derive(Deserialize)]
struct DonePayload {
    evidence: Vec<NewEvidence>,
    /// Absent when the task never was in `doing`.
    doing_ms: Option<i64>,
    /// Set when a feedback item was resolved: its evidence is the resolution.
    #[serde(default)]
    resolution: Option<serde::de::IgnoredAny>,
}

pub(crate) fn stats(cx: &Ctx<'_>, days: u32) -> Result<Stats> {
    let from = cx.now_ms() - i64::from(days) * DAY_MS;
    // Events are appended in time order: the first by key is the oldest.
    let oldest: Option<i64> = cx
        .db
        .query_row("SELECT at FROM events ORDER BY seq LIMIT 1", [], |r| {
            r.get(0)
        })
        .optional()?;
    let since = oldest.map_or(from, |oldest| oldest.max(from));
    let count_events = |kind: &str| -> Result<u32> {
        Ok(cx.db.query_row(
            "SELECT COUNT(*) FROM events WHERE type = ?1 AND at >= ?2",
            params![kind, from],
            |r| r.get(0),
        )?)
    };
    let conflicts = conflicts_by_repo(cx, from)?;
    let done_payloads = cx
        .db
        .prepare_cached(
            "SELECT seq, payload, task, board, at FROM events \
             WHERE type = 'task_done' AND at >= ?1",
        )?
        .query_map([from], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut doing_minutes = Vec::new();
    let mut without_check = 0;
    let mut under_a_minute = 0;
    let mut done_by_board: HashMap<String, Vec<DoneRow>> = HashMap::new();
    for (seq, payload, task, board, done_at) in done_payloads {
        let done: DonePayload = parse_json(&payload)?;
        // Feedback items (with a resolution) are routed, not worked: neither count applies.
        if done.resolution.is_none() && !done.evidence.iter().any(|e| e.kind == EvidenceKind::Check)
        {
            without_check += 1;
        }
        if let Some(ms) = done.doing_ms.filter(|ms| *ms < 60_000)
            && done.resolution.is_none()
        {
            // A takeover restarts `doing`: finishing a stale owner's work quickly is not
            // work done off the board.
            let taken: bool = cx.db.query_row(
                "SELECT EXISTS (SELECT 1 FROM events WHERE type = 'task_taken' AND task = ?1 \
                 AND at BETWEEN ?2 AND ?3)",
                params![task, done_at - ms, done_at],
                |r| r.get(0),
            )?;
            if !taken {
                under_a_minute += 1;
            }
        }
        if let Some(ms) = done.doing_ms {
            doing_minutes.push(ms as f64 / 60_000.0);
        }
        if let (Some(board), Some(task)) = (board, task) {
            let prs = done
                .evidence
                .iter()
                .filter(|e| e.kind == EvidenceKind::Pr)
                .count() as u32;
            done_by_board
                .entry(board)
                .or_default()
                .push(DoneRow { seq, task, prs });
        }
    }
    let answer_minutes: Vec<f64> = cx
        .db
        .prepare_cached(
            "SELECT answered_at - created_at FROM asks WHERE answered_at IS NOT NULL \
             AND answered_at >= ?1",
        )?
        .query_map([from], |r| r.get::<_, i64>(0))?
        .map(|ms| ms.map(|ms| ms as f64 / 60_000.0))
        .collect::<rusqlite::Result<_>>()?;
    Ok(Stats {
        days,
        tasks_created: count_events("task_created")?,
        tasks_done: count_events("task_done")?,
        claims_refused: cx.db.query_row(
            "SELECT COUNT(*) FROM claim_refusals WHERE at >= ?1",
            [from],
            |r| r.get(0),
        )?,
        // A stand-in for an ended owner takes a task that is not stale.
        tasks_taken_stale: cx.db.query_row(
            "SELECT COUNT(*) FROM events WHERE type = 'task_taken' AND at >= ?1 \
             AND json_extract(payload, '$.stand_in') IS NULL",
            [from],
            |r| r.get(0),
        )?,
        lease_conflicts: conflicts.iter().map(|(_, n)| n).sum(),
        asks_created: cx.db.query_row(
            "SELECT COUNT(*) FROM asks WHERE created_at >= ?1",
            [from],
            |r| r.get(0),
        )?,
        asks_answered: answer_minutes.len() as u32,
        ask_answer_minutes_median: median(answer_minutes),
        doing_minutes_median: median(doing_minutes),
        done_without_check_evidence: without_check,
        done_under_a_minute: Some(under_a_minute),
        feedback: crate::ops::feedback::stats(cx, from)?,
        since: Some(ts(since)),
        boards: board_stats(cx, from, since, done_by_board, &conflicts)?,
    })
}

/// Lease conflicts and touch overlaps since `?1`, summed per repository: the scan that
/// [`conflicts_by_repo`] runs once for the total and for every board.
pub(crate) const CONFLICTS_BY_REPO: &str = "SELECT json_extract(payload, '$.repo'), \
     COALESCE(SUM(json_extract(payload, CASE type WHEN 'lease_created' THEN '$.conflicts' \
                                         ELSE '$.overlaps' END)), 0) \
     FROM events WHERE type IN ('lease_created', 'files_touched') AND at >= ?1 GROUP BY 1";

/// Lease conflicts and touch overlaps since `from`, by the repository their event names
/// (`None` for an event that names none, which counts for no board).
fn conflicts_by_repo(cx: &Ctx<'_>, from: i64) -> Result<Vec<(Option<String>, u32)>> {
    Ok(cx
        .db
        .prepare_cached(CONFLICTS_BY_REPO)?
        .query_map([from], |r| {
            // A board's repositories are strings, so a `repo` of another type matches none.
            let repo = match r.get_ref(0)? {
                ValueRef::Text(repo) => Some(String::from_utf8_lossy(repo).into_owned()),
                _ => None,
            };
            Ok((repo, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// A `task_done` event of a board's task, as its board's stats read it.
struct DoneRow {
    seq: i64,
    task: String,
    /// PR links in its evidence.
    prs: u32,
}

/// Per-board numbers for each board with events since `from`; rates are per day since
/// `since`, the oldest time the window covers. `done` holds the window's `task_done`
/// events by board.
fn board_stats(
    cx: &Ctx<'_>,
    from: i64,
    since: i64,
    mut done: HashMap<String, Vec<DoneRow>>,
    conflicts: &[(Option<String>, u32)],
) -> Result<Vec<BoardStats>> {
    let days = ((cx.now_ms() - since) as f64 / DAY_MS as f64).max(1.0 / 1440.0);
    // A board's newest event, by key, says whether it has any since `from`: one index
    // lookup per board instead of a scan of every event.
    let boards: Vec<(String, String)> = cx
        .db
        .prepare_cached(
            "SELECT slug, repos FROM boards b WHERE (SELECT e.at FROM events e \
             WHERE e.board = b.slug ORDER BY e.seq DESC LIMIT 1) >= ?1 ORDER BY slug",
        )?
        .query_map([from], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(boards.len());
    for (slug, repos) in boards {
        let repos: Vec<String> = parse_json(&repos)?;
        let waits = waits(cx, &slug, from)?;
        let done = done.remove(&slug).unwrap_or_default();
        let doing: Vec<f64> = doing_minutes(cx, &done)?.into_iter().flatten().collect();
        let pauses = pauses(cx, &slug, from)?;
        let tasks_done = done.len() as u32;
        let limit_pauses = pauses.len() as u32;
        out.push(BoardStats {
            board: parse_id(&slug)?,
            claims: waits.len() as u32,
            wait_minutes_median: median(waits.clone()),
            wait_minutes_p90: percentile(waits, 0.9),
            tasks_done,
            done_per_day: f64::from(tasks_done) / days,
            doing_minutes_median: median(doing),
            // On the board's repos, whichever board's task (if any) the lease names.
            lease_conflicts: conflicts
                .iter()
                .filter(|(repo, _)| repo.as_ref().is_some_and(|repo| repos.contains(repo)))
                .map(|(_, n)| n)
                .sum(),
            reopened_after_review: cx.db.query_row(
                "SELECT COUNT(*) FROM events \
                 WHERE type IN ('task_released', 'task_updated') AND at >= ?2 AND board = ?1 \
                 AND json_extract(payload, '$.from') = 'review' \
                 AND (type = 'task_released' \
                      OR json_extract(payload, '$.to') IN ('open', 'doing'))",
                params![slug, from],
                |r| r.get(0),
            )?,
            pr_links: done.iter().map(|d| d.prs).sum(),
            limit_pauses,
            limit_pauses_per_day: f64::from(limit_pauses) / days,
            paused_hours: pauses.iter().fold(0.0, |hours, p| hours + p.hours),
            peak_running_at_pause: pauses.iter().map(|p| p.running).max(),
        });
    }
    Ok(out)
}

/// Minutes each auto task of `board` claimed since `from` waited from ready to claimed.
/// Ready at the latest of: its latest creation, release, move to `open`, or change of
/// `auto`, blockers or `not_before`; the `not_before` it then has; and its last blocker
/// done. A task created claimed never waited, unless it was released since.
fn waits(cx: &Ctx<'_>, board: &str, from: i64) -> Result<Vec<f64>> {
    let claims: Vec<(i64, i64, String, Option<i64>)> = cx
        .db
        .prepare_cached(
            "SELECT e.seq, e.at, e.task, t.not_before FROM events e JOIN tasks t \
             ON t.id = e.task \
             WHERE e.type = 'task_claimed' AND e.board = ?1 AND e.at >= ?2 AND t.auto = 1",
        )?
        .query_map(params![board, from], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut waits = Vec::with_capacity(claims.len());
    for (seq, at, task, row_not_before) in claims {
        // Each event that makes the task ready again replaces the last: a `not_before`
        // cleared or moved earlier makes it ready sooner than one still ahead.
        let mut reset: Option<i64> = None;
        let mut not_before: Option<i64> = None;
        // Whether `not_before` is known: set by a change or named by its passing. Until
        // then it is the one the task was created with.
        let mut not_before_known = false;
        let mut created_claimed = false;
        let mut stmt = cx.db.prepare_cached(
            "SELECT type, at, payload FROM events WHERE task = ?1 AND seq < ?2 \
             AND type IN ('task_created', 'task_released', 'task_updated') ORDER BY seq",
        )?;
        let mut rows = stmt.query(params![task, seq])?;
        while let Some(r) = rows.next()? {
            let kind: String = r.get(0)?;
            let event_at: i64 = r.get(1)?;
            let payload: serde_json::Value = parse_json(&r.get::<_, String>(2)?)?;
            let time = |field: &str| {
                payload[field]
                    .as_str()
                    .and_then(|t| t.parse::<Timestamp>().ok())
                    .map(|t| t.timestamp_millis())
            };
            match kind.as_str() {
                "task_created" => {
                    created_claimed = payload["claimed"] == true;
                    reset = Some(event_at);
                }
                "task_released" => {
                    created_claimed = false;
                    reset = Some(event_at);
                }
                _ => {
                    let fields = payload["fields"].as_array();
                    let changed = |f: &str| fields.is_some_and(|fs| fs.iter().any(|v| v == f));
                    if changed("not_before") {
                        not_before = time("not_before");
                        not_before_known = true;
                    }
                    if let Some(passed) = time("not_before_passed") {
                        not_before = Some(passed);
                        not_before_known = true;
                    }
                    if payload["to"] == "open" {
                        created_claimed = false;
                    }
                    if payload["to"] == "open"
                        || changed("not_before")
                        || changed("auto")
                        || changed("blockers")
                    {
                        reset = Some(event_at);
                    }
                }
            }
        }
        drop(rows);
        if created_claimed {
            continue;
        }
        // The task's row holds the `not_before` it was created with until a change: a
        // claim before the sweeper announced it passing still waited for it.
        if !not_before_known
            && row_not_before.is_some()
            && !cx.exists(
                "SELECT 1 FROM events WHERE task = ?1 AND type = 'task_updated' \
                 AND EXISTS (SELECT 1 FROM json_each(payload, '$.fields') \
                             WHERE value = 'not_before')",
                [&task],
            )?
        {
            not_before = row_not_before;
        }
        let blockers_done: Option<i64> = cx.db.query_row(
            "SELECT MAX(e.at) FROM task_blockers b JOIN events e \
             ON e.task = b.blocker AND e.type = 'task_done' AND e.seq < ?2 WHERE b.task = ?1",
            params![task, seq],
            |r| r.get(0),
        )?;
        if let Some(ready) = [reset, not_before, blockers_done]
            .into_iter()
            .flatten()
            .max()
        {
            waits.push((at - ready).max(0) as f64 / 60_000.0);
        }
    }
    Ok(waits)
}

/// Minutes each of `done` spent in `doing` since the task's previous `task_done`, time in
/// `review` excluded; `None` when it was not in `doing`.
fn doing_minutes(cx: &Ctx<'_>, done: &[DoneRow]) -> Result<Vec<Option<f64>>> {
    let mut out = Vec::with_capacity(done.len());
    for DoneRow { seq, task, .. } in done {
        let mut doing_since: Option<i64> = None;
        let mut doing_ms: Option<i64> = None;
        let mut stmt = cx.db.prepare_cached(
            "SELECT type, at, payload FROM events WHERE task = ?1 AND seq <= ?2 \
             AND seq > COALESCE((SELECT MAX(seq) FROM events WHERE task = ?1 \
                                 AND type = 'task_done' AND seq < ?2), 0) \
             AND type IN ('task_created', 'task_claimed', 'task_taken', 'task_updated', \
                          'task_released', 'task_done') ORDER BY seq",
        )?;
        let mut events = stmt.query(params![task, seq])?;
        while let Some(r) = events.next()? {
            let kind: String = r.get(0)?;
            let at: i64 = r.get(1)?;
            let payload: serde_json::Value = parse_json(&r.get::<_, String>(2)?)?;
            let to = match kind.as_str() {
                "task_created" if payload["claimed"] == true => "doing",
                "task_created" => continue,
                "task_claimed" => "doing",
                "task_taken" => payload["status"].as_str().unwrap_or("doing"),
                "task_updated" => match payload["to"].as_str() {
                    Some(to) => to,
                    None => continue,
                },
                "task_released" => "open",
                _ => "done",
            };
            match (doing_since, to == "doing") {
                (Some(start), false) => {
                    *doing_ms.get_or_insert(0) += at - start;
                    doing_since = None;
                }
                (None, true) => doing_since = Some(at),
                _ => {}
            }
        }
        out.push(doing_ms.map(|ms| ms as f64 / 60_000.0));
    }
    Ok(out)
}

/// A usage-limit pause of a member agent of a board.
struct Pause {
    /// Hours paused: until its reset (extended by a repeat naming a later one), an earlier
    /// move to a working state, an earlier end of the session when it named no reset, or
    /// now.
    hours: f64,
    /// Member agents running just before it paused, itself included.
    running: u32,
}

/// A pause still being replayed.
struct OpenPause {
    start: i64,
    until: Option<i64>,
    running: u32,
}

impl OpenPause {
    fn close(self, at: i64) -> Pause {
        let end = self.until.map_or(at, |until| until.min(at));
        Pause {
            hours: (end - self.start).max(0) as f64 / 3_600_000.0,
            running: self.running,
        }
    }
}

/// The pauses of `board`'s member agents since `from`, each after the agent joined.
fn pauses(cx: &Ctx<'_>, board: &str, from: i64) -> Result<Vec<Pause>> {
    #[derive(Deserialize)]
    struct Change {
        from: AgentState,
        to: AgentState,
        paused_until: Option<Timestamp>,
    }
    let rows: Vec<(i64, i64, String, String)> = cx
        .db
        .prepare_cached(
            "SELECT e.seq, e.at, e.actor, e.payload FROM events e \
             JOIN board_members m ON m.agent = e.actor AND m.board = ?1 \
             WHERE e.type = 'agent_state_changed' AND e.at >= ?2 AND e.at >= m.joined_at \
             ORDER BY e.actor, e.seq",
        )?
        .query_map(params![board, from], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let now = cx.now_ms();
    let mut pauses = Vec::new();
    let mut agent: Option<String> = None;
    let mut open: Option<OpenPause> = None;
    for (seq, at, actor, payload) in rows {
        if agent.as_deref() != Some(actor.as_str()) {
            pauses.extend(open.take().map(|p| p.close(now)));
            agent = Some(actor);
        }
        let change: Change = parse_json(&payload)?;
        let until = change.paused_until.map(|t| t.timestamp_millis());
        match change.to {
            AgentState::Paused => {
                if change.from == AgentState::Paused {
                    match open.as_mut() {
                        // A repeat before the reset: a later reset extends the pause.
                        Some(p) if p.until.is_none_or(|u| at < u) => {
                            p.until = p.until.max(until);
                            continue;
                        }
                        // The reset passed while the agent stayed paused: a new limit.
                        Some(_) => {}
                        // Paused before the window or before it joined.
                        None => continue,
                    }
                }
                pauses.extend(open.take().map(|p| p.close(at)));
                open = Some(OpenPause {
                    start: at,
                    until,
                    running: running_at(cx, board, seq, at)?,
                });
            }
            // A session that ends keeps a reset it waits for, so the pause holds until
            // then; without one, nothing is left waiting.
            AgentState::Ended if open.as_ref().is_some_and(|p| p.until.is_some()) => {}
            _ => pauses.extend(open.take().map(|p| p.close(at))),
        }
    }
    pauses.extend(open.take().map(|p| p.close(now)));
    Ok(pauses)
}

/// Member agents of `board` running when the event `seq` at `at` paused one of them:
/// running by their latest registration or state change before it, and live, with an
/// event of their own within the staleness threshold.
fn running_at(cx: &Ctx<'_>, board: &str, seq: i64, at: i64) -> Result<u32> {
    Ok(cx.db.query_row(
        "SELECT COUNT(*) FROM board_members m \
         WHERE m.board = ?1 AND m.joined_at <= ?2 \
         AND (SELECT CASE e.type WHEN 'agent_registered' THEN 'running' \
                     ELSE json_extract(e.payload, '$.to') END \
              FROM events e WHERE e.actor = m.agent \
              AND e.type IN ('agent_state_changed', 'agent_registered') AND e.seq < ?3 \
              ORDER BY e.seq DESC LIMIT 1) = 'running' \
         AND (SELECT e.at FROM events e WHERE e.actor = m.agent AND e.seq <= ?3 \
              ORDER BY e.seq DESC LIMIT 1) > ?4",
        params![board, at, seq, at - cx.s.stale_ms()],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::{median, percentile};

    #[test]
    fn percentiles_by_nearest_rank() {
        assert_eq!(percentile(vec![], 0.9), None);
        assert_eq!(percentile(vec![5.0], 0.9), Some(5.0));
        let ten: Vec<f64> = (1..=10).map(f64::from).collect();
        assert_eq!(percentile(ten.clone(), 0.9), Some(9.0));
        assert_eq!(percentile(ten, 0.5), Some(5.0));
    }

    #[test]
    fn medians() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
    }
}
