use board_proto::{EvidenceKind, NewEvidence, Stats};
use rusqlite::params;
use serde::Deserialize;

use crate::db::{Ctx, parse_json};
use crate::error::{Result, invalid};

pub(crate) fn validate_days(days: Option<u32>) -> Result<u32> {
    match days.unwrap_or(7) {
        d @ 1..=365 => Ok(d),
        d => Err(invalid(
            format!("days={d} is out of range"),
            "send days between 1 and 365",
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
    let done_payloads: Vec<String> = cx
        .db
        .prepare_cached("SELECT payload FROM events WHERE type = 'task_done' AND at >= ?1")?
        .query_map([from], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut doing_minutes = Vec::new();
    let mut without_check = 0;
    for payload in done_payloads {
        let done: DonePayload = parse_json(&payload)?;
        if !done.evidence.iter().any(|e| e.kind == EvidenceKind::Check) {
            without_check += 1;
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
