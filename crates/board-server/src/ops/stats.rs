use board_proto::{BoardStats, EvidenceKind, NewEvidence, Stats, Timestamp};
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
    let oldest: Option<i64> = cx
        .db
        .query_row("SELECT MIN(at) FROM events", [], |r| r.get(0))?;
    let since = oldest.map_or(from, |oldest| oldest.max(from));
    let count_events = |kind: &str| -> Result<u32> {
        Ok(cx.db.query_row(
            "SELECT COUNT(*) FROM events WHERE type = ?1 AND at >= ?2",
            params![kind, from],
            |r| r.get(0),
        )?)
    };
    let sum_payload = |kind: &str, field: &str| -> Result<u32> {
        Ok(cx.db.query_row(
            "SELECT COALESCE(SUM(json_extract(payload, ?3)), 0) FROM events \
             WHERE type = ?1 AND at >= ?2",
            params![kind, from, format!("$.{field}")],
            |r| r.get(0),
        )?)
    };
    let done_payloads: Vec<(String, Option<String>, i64)> = cx
        .db
        .prepare_cached(
            "SELECT payload, task, at FROM events WHERE type = 'task_done' AND at >= ?1",
        )?
        .query_map([from], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut doing_minutes = Vec::new();
    let mut without_check = 0;
    let mut under_a_minute = 0;
    for (payload, task, done_at) in done_payloads {
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
        tasks_taken_stale: count_events("task_taken")?,
        lease_conflicts: sum_payload("lease_created", "conflicts")?
            + sum_payload("files_touched", "overlaps")?,
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
        boards: board_stats(cx, from, since)?,
    })
}

/// Per-board numbers for each board with events since `from`; rates are per day since
/// `since`, the oldest time the window covers.
fn board_stats(cx: &Ctx<'_>, from: i64, since: i64) -> Result<Vec<BoardStats>> {
    let days = ((cx.now_ms() - since) as f64 / DAY_MS as f64).max(1.0 / 1440.0);
    let boards: Vec<(String, String)> = cx
        .db
        .prepare_cached(
            "SELECT slug, repos FROM boards WHERE slug IN \
             (SELECT DISTINCT board FROM events WHERE at >= ?1 AND board IS NOT NULL) \
             ORDER BY slug",
        )?
        .query_map([from], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(boards.len());
    for (slug, repos) in boards {
        let waits = waits(cx, &slug, from)?;
        let done = done(cx, &slug, from)?;
        let pauses = pauses(cx, &slug, from)?;
        let tasks_done = done.len() as u32;
        let pr_links = done.iter().map(|d| d.prs).sum();
        let doing: Vec<f64> = done.iter().filter_map(|d| d.doing_minutes).collect();
        let limit_pauses = pauses.len() as u32;
        out.push(BoardStats {
            board: parse_id(&slug)?,
            claims: waits.len() as u32,
            wait_minutes_median: median(waits.clone()),
            wait_minutes_p90: percentile(waits, 0.9),
            tasks_done,
            done_per_day: f64::from(tasks_done) / days,
            doing_minutes_median: median(doing),
            lease_conflicts: cx.db.query_row(
                "SELECT COALESCE(SUM(json_extract(payload, '$.conflicts')), 0) FROM events \
                 WHERE type = 'lease_created' AND board = ?1 AND at >= ?2",
                params![slug, from],
                |r| r.get::<_, u32>(0),
            )? + cx.db.query_row(
                "SELECT COALESCE(SUM(json_extract(payload, '$.overlaps')), 0) FROM events \
                 WHERE type = 'files_touched' AND at >= ?1 \
                 AND json_extract(payload, '$.repo') IN (SELECT value FROM json_each(?2))",
                params![from, repos],
                |r| r.get::<_, u32>(0),
            )?,
            reopened_after_review: cx.db.query_row(
                "SELECT COUNT(*) FROM events WHERE board = ?1 AND at >= ?2 \
                 AND json_extract(payload, '$.from') = 'review' \
                 AND (type = 'task_released' OR (type = 'task_updated' \
                      AND json_extract(payload, '$.to') IN ('open', 'doing', 'blocked')))",
                params![slug, from],
                |r| r.get(0),
            )?,
            pr_links,
            limit_pauses,
            limit_pauses_per_day: f64::from(limit_pauses) / days,
            paused_hours: pauses.iter().map(|p| p.hours).sum(),
            peak_running_at_pause: pauses.iter().map(|p| p.running).max(),
        });
    }
    Ok(out)
}

/// Minutes each auto task of `board` claimed since `from` waited from ready to claimed.
/// Ready from the latest of: its creation, a release, a move to `open`, a change of `auto`,
/// blockers or `not_before` (or the time it named), `not_before` passing, and its last
/// blocker done. A task created claimed never waited, unless it was released since.
fn waits(cx: &Ctx<'_>, board: &str, from: i64) -> Result<Vec<f64>> {
    let claims: Vec<(i64, i64, String)> = cx
        .db
        .prepare_cached(
            "SELECT e.seq, e.at, e.task FROM events e JOIN tasks t ON t.id = e.task \
             WHERE e.type = 'task_claimed' AND e.board = ?1 AND e.at >= ?2 AND t.auto = 1",
        )?
        .query_map(params![board, from], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut waits = Vec::with_capacity(claims.len());
    for (seq, at, task) in claims {
        let mut ready: Option<i64> = None;
        let mut created_claimed = false;
        let mut stmt = cx.db.prepare_cached(
            "SELECT type, at, payload FROM events WHERE task = ?1 AND seq < ?2 ORDER BY seq",
        )?;
        let mut rows = stmt.query(params![task, seq])?;
        while let Some(r) = rows.next()? {
            let kind: String = r.get(0)?;
            let event_at: i64 = r.get(1)?;
            let payload: serde_json::Value = parse_json(&r.get::<_, String>(2)?)?;
            let became = match kind.as_str() {
                "task_created" => {
                    created_claimed = payload["claimed"] == true;
                    Some(event_at)
                }
                "task_released" => {
                    created_claimed = false;
                    Some(event_at)
                }
                "task_updated" => {
                    let fields = payload["fields"].as_array();
                    let changed = |f: &str| fields.is_some_and(|fs| fs.iter().any(|v| v == f));
                    let not_before = payload["not_before"]
                        .as_str()
                        .and_then(|t| t.parse::<Timestamp>().ok())
                        .map(|t| t.timestamp_millis());
                    if payload["to"] == "open" {
                        created_claimed = false;
                    }
                    if changed("not_before") {
                        Some(not_before.map_or(event_at, |t| t.max(event_at)))
                    } else if payload["to"] == "open"
                        || changed("auto")
                        || changed("blockers")
                        || payload.get("not_before_passed").is_some()
                    {
                        Some(event_at)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if became.is_some() {
                ready = ready.max(became);
            }
        }
        drop(rows);
        let blockers_done: Option<i64> = cx.db.query_row(
            "SELECT MAX(e.at) FROM task_blockers b JOIN events e \
             ON e.task = b.blocker AND e.type = 'task_done' AND e.seq < ?2 WHERE b.task = ?1",
            params![task, seq],
            |r| r.get(0),
        )?;
        if created_claimed {
            continue;
        }
        if let Some(ready) = ready.max(blockers_done) {
            waits.push((at - ready).max(0) as f64 / 60_000.0);
        }
    }
    Ok(waits)
}

/// A task done on a board, as its stats read it.
struct Done {
    prs: u32,
    /// Minutes in `doing` before it was done, `review` excluded; `None` when never in it.
    doing_minutes: Option<f64>,
}

/// The tasks of `board` done since `from`, each time in `doing` replayed from its events.
fn done(cx: &Ctx<'_>, board: &str, from: i64) -> Result<Vec<Done>> {
    let rows: Vec<(i64, String, String)> = cx
        .db
        .prepare_cached(
            "SELECT seq, task, payload FROM events \
             WHERE type = 'task_done' AND board = ?1 AND at >= ?2",
        )?
        .query_map(params![board, from], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut done = Vec::with_capacity(rows.len());
    for (seq, task, payload) in rows {
        let payload: DonePayload = parse_json(&payload)?;
        let prs = payload
            .evidence
            .iter()
            .filter(|e| e.kind == EvidenceKind::Pr)
            .count() as u32;
        let mut doing_since: Option<i64> = None;
        let mut doing_ms: Option<i64> = None;
        let mut stmt = cx.db.prepare_cached(
            "SELECT type, at, payload FROM events WHERE task = ?1 AND seq <= ?2 \
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
        done.push(Done {
            prs,
            doing_minutes: doing_ms.map(|ms| ms as f64 / 60_000.0),
        });
    }
    Ok(done)
}

/// A usage-limit pause of a member agent of a board.
struct Pause {
    /// Hours paused: until its reset, an earlier change to a working state, or now.
    hours: f64,
    /// Member agents running just before it paused, itself included.
    running: u32,
}

/// The pauses of `board`'s member agents since `from`.
fn pauses(cx: &Ctx<'_>, board: &str, from: i64) -> Result<Vec<Pause>> {
    let rows: Vec<(i64, i64, String, String)> = cx
        .db
        .prepare_cached(
            "SELECT e.seq, e.at, e.actor, e.payload FROM events e \
             JOIN board_members m ON m.agent = e.actor AND m.board = ?1 \
             WHERE e.type = 'agent_state_changed' AND e.at >= ?2 \
             AND json_extract(e.payload, '$.to') = 'paused' \
             AND json_extract(e.payload, '$.from') != 'paused'",
        )?
        .query_map(params![board, from], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let now = cx.now_ms();
    let mut pauses = Vec::with_capacity(rows.len());
    for (seq, at, agent, payload) in rows {
        #[derive(Deserialize)]
        struct Paused {
            paused_until: Option<Timestamp>,
        }
        let until = parse_json::<Paused>(&payload)?
            .paused_until
            .map(|t| t.timestamp_millis());
        let resumed: Option<i64> = cx
            .db
            .query_row(
                "SELECT at FROM events WHERE actor = ?1 AND type = 'agent_state_changed' \
                 AND seq > ?2 AND json_extract(payload, '$.to') NOT IN ('paused', 'ended') \
                 ORDER BY seq LIMIT 1",
                params![agent, seq],
                |r| r.get(0),
            )
            .optional()?;
        let end = [until, resumed, Some(now)]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(now);
        let running = cx.db.query_row(
            "SELECT COUNT(*) FROM board_members m JOIN agents a ON a.id = m.agent \
             WHERE m.board = ?1 AND m.joined_at <= ?2 AND a.created_at <= ?2 \
             AND COALESCE((SELECT json_extract(e.payload, '$.to') FROM events e \
                 WHERE e.actor = m.agent AND e.type = 'agent_state_changed' AND e.seq < ?3 \
                 ORDER BY e.seq DESC LIMIT 1), 'running') = 'running'",
            params![board, at, seq],
            |r| r.get(0),
        )?;
        pauses.push(Pause {
            hours: (end - at).max(0) as f64 / 3_600_000.0,
            running,
        });
    }
    Ok(pauses)
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
