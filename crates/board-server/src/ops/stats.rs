use board_proto::{EvidenceKind, NewEvidence, Stats};
use rusqlite::params;
use serde::Deserialize;

use crate::db::{Ctx, parse_json};
use crate::error::{Result, invalid};

/// The longest window stats read, in days. The server keeps the history they read
/// (events and refused claims) this long; see [`prune_history`].
pub(crate) const MAX_DAYS: u32 = 365;

/// The most rows of each table that one [`prune_history`] transaction deletes, so pruning
/// never holds the write lock for long.
pub(crate) const PRUNE_BATCH: usize = 1_000;

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
    let from = cx.now_ms() - i64::from(days) * 86_400_000;
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
    })
}

#[cfg(test)]
mod tests {
    use super::median;

    #[test]
    fn medians() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
    }
}
