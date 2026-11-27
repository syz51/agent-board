//! What the user reads apart from the threads (`docs/API.md`, "Updates"): done tasks with
//! their summaries, dropped tasks and verdict cut-drops (so no scope is lost silently),
//! coordinators' turn endings, and the user's seen cursor.

use board_proto::{
    AgentId, BoardSlug, DoneUpdate, DroppedUpdate, MarkSeen, Said, SeenCursor, TaskOrigin,
    Timestamp, UPDATES_MAX, Updates, VerdictCutUpdate, extract_dropped_verdict_cuts,
};
use rusqlite::{OptionalExtension, Row, params};

use crate::db::{Ctx, parse_enum, parse_id, ts};
use crate::error::Result;
use crate::store::load_evidence;

/// Turn endings kept per agent and board.
pub(crate) const SAID_KEPT: usize = 50;
/// Default `limit` of `GET /v1/agents/{id}/said`; at most `SAID_KEPT`.
pub(crate) const SAID_LIST_DEFAULT: usize = 20;

/// Stores `text` once per board whose live coordinator or live judge `agent` is, keeping
/// the newest `SAID_KEPT` per agent and board. Whether anything was stored.
pub(crate) fn record_said(cx: &mut Ctx<'_>, agent: &AgentId, text: &str) -> Result<bool> {
    let boards = crate::ops::judges::boards_with_rights(cx, agent)?;
    for board in &boards {
        cx.db.execute(
            "INSERT INTO said (agent, board, text, at) VALUES (?1, ?2, ?3, ?4)",
            params![agent.as_str(), board, text, cx.now_ms()],
        )?;
        cx.db.execute(
            "DELETE FROM said WHERE agent = ?1 AND board = ?2 AND id NOT IN \
             (SELECT id FROM said WHERE agent = ?1 AND board = ?2 ORDER BY id DESC LIMIT ?3)",
            params![agent.as_str(), board, SAID_KEPT as i64],
        )?;
    }
    Ok(!boards.is_empty())
}

fn said_row(r: &Row<'_>) -> rusqlite::Result<Said> {
    Ok(Said {
        id: r.get(0)?,
        agent: parse_id(&r.get::<_, String>(1)?)?,
        board: parse_id(&r.get::<_, String>(2)?)?,
        text: r.get(3)?,
        at: ts(r.get(4)?),
    })
}

/// The agent's newest turn endings, newest first.
pub(crate) fn list_said(cx: &Ctx<'_>, agent: &AgentId, limit: usize) -> Result<Vec<Said>> {
    Ok(cx
        .db
        .prepare_cached(
            "SELECT id, agent, board, text, at FROM said WHERE agent = ?1 \
             ORDER BY at DESC, id DESC LIMIT ?2",
        )?
        .query_map(params![agent.as_str(), limit as i64], said_row)?
        .collect::<rusqlite::Result<_>>()?)
}

fn seen_at(cx: &Ctx<'_>) -> Result<Option<Timestamp>> {
    Ok(cx
        .db
        .query_row("SELECT seen_at FROM user_seen WHERE id = 1", [], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?
        .map(ts))
}

/// Done tasks and turn endings after `since` (else the seen cursor), newest first.
pub(crate) fn updates(
    cx: &Ctx<'_>,
    since: Option<Timestamp>,
    board: Option<&BoardSlug>,
) -> Result<Updates> {
    let seen_at = seen_at(cx)?;
    let since = since.or(seen_at);
    // Stored times are milliseconds; a NULL bound selects everything.
    let after = since.map(|t| t.timestamp_millis());
    let board = board.map(BoardSlug::as_str);
    let fetch = UPDATES_MAX as i64 + 1;

    let mut done = cx
        .db
        .prepare_cached(
            "SELECT id, board, parent, title, summary, owner, done_at FROM tasks \
             WHERE status = 'done' AND done_at IS NOT NULL \
               AND (?1 IS NULL OR done_at > ?1) AND (?2 IS NULL OR board = ?2) \
               AND NOT EXISTS (SELECT 1 FROM feedback f \
                               WHERE f.task = tasks.id AND f.resolution IS NOT NULL) \
             ORDER BY done_at DESC, number DESC LIMIT ?3",
        )?
        .query_map(params![after, board, fetch], |r| {
            Ok(DoneUpdate {
                task: parse_id(&r.get::<_, String>(0)?)?,
                board: parse_id(&r.get::<_, String>(1)?)?,
                parent: r
                    .get::<_, Option<String>>(2)?
                    .map(|p| parse_id(&p))
                    .transpose()?,
                title: r.get(3)?,
                summary: r.get(4)?,
                owner: r
                    .get::<_, Option<String>>(5)?
                    .map(|o| parse_id(&o))
                    .transpose()?,
                done_at: ts(r.get(6)?),
                evidence: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut said = cx
        .db
        .prepare_cached(
            "SELECT id, agent, board, text, at FROM said \
             WHERE (?1 IS NULL OR at > ?1) AND (?2 IS NULL OR board = ?2) \
             ORDER BY at DESC, id DESC LIMIT ?3",
        )?
        .query_map(params![after, board, fetch], said_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut dropped = cx
        .db
        .prepare_cached(
            "SELECT id, board, parent, title, origin, updated_at FROM tasks \
             WHERE status = 'dropped' \
               AND (?1 IS NULL OR updated_at > ?1) AND (?2 IS NULL OR board = ?2) \
               AND NOT EXISTS (SELECT 1 FROM feedback f \
                               WHERE f.task = tasks.id AND f.resolution IS NOT NULL) \
             ORDER BY updated_at DESC, number DESC LIMIT ?3",
        )?
        .query_map(params![after, board, fetch], |r| {
            let origin: Option<String> = r.get(4)?;
            Ok(DroppedUpdate {
                task: parse_id(&r.get::<_, String>(0)?)?,
                board: parse_id(&r.get::<_, String>(1)?)?,
                parent: r
                    .get::<_, Option<String>>(2)?
                    .map(|p| parse_id(&p))
                    .transpose()?,
                title: r.get(3)?,
                origin: origin
                    .as_deref()
                    .map(parse_enum::<TaskOrigin>)
                    .transpose()?,
                dropped_by: None,
                reason: None,
                dropped_at: ts(r.get(5)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let cut_scan_limit = (UPDATES_MAX as i64 * 10) + 1;
    let mut cuts: Vec<VerdictCutUpdate> = Vec::new();
    let mut cut_scan_exhausted = false;
    {
        let mut stmt = cx.db.prepare_cached(
            "SELECT n.id, n.task, t.board, t.title, n.agent, n.at, n.text \
             FROM notes n \
             JOIN tasks t ON t.id = n.task \
             WHERE n.kind = 'decision' \
               AND (?1 IS NULL OR n.at > ?1) AND (?2 IS NULL OR t.board = ?2) \
               AND lower(ltrim(n.text)) LIKE 'verdict:%' \
               AND lower(n.text) LIKE '%cut:%' \
             ORDER BY n.at DESC, n.id DESC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![after, board, cut_scan_limit])?;
        let mut scanned: usize = 0;
        while let Some(r) = rows.next()? {
            scanned += 1;
            let text: String = r.get(6)?;
            let cut_lines = extract_dropped_verdict_cuts(&text);
            if cut_lines.is_empty() {
                continue;
            }
            cuts.push(VerdictCutUpdate {
                note_id: r.get(0)?,
                task: parse_id(&r.get::<_, String>(1)?)?,
                board: parse_id(&r.get::<_, String>(2)?)?,
                title: r.get(3)?,
                judge: parse_id(&r.get::<_, String>(4)?)?,
                at: ts(r.get(5)?),
                cut_lines,
            });
            if cuts.len() > UPDATES_MAX {
                break;
            }
        }
        if scanned >= cut_scan_limit as usize && cuts.len() <= UPDATES_MAX {
            cut_scan_exhausted = true;
        }
    }

    let more = done.len() > UPDATES_MAX
        || said.len() > UPDATES_MAX
        || dropped.len() > UPDATES_MAX
        || cuts.len() > UPDATES_MAX
        || cut_scan_exhausted;
    done.truncate(UPDATES_MAX);
    said.truncate(UPDATES_MAX);
    dropped.truncate(UPDATES_MAX);
    cuts.truncate(UPDATES_MAX);
    for item in &mut done {
        item.evidence = load_evidence(cx, item.task.as_str())?;
    }
    for item in &mut dropped {
        let (dropped_by, reason) = load_drop_attribution(cx, &item.task)?;
        item.dropped_by = dropped_by;
        item.reason = reason;
    }
    Ok(Updates {
        seen_at,
        since,
        done,
        said,
        dropped,
        cuts,
        more,
    })
}

/// Resolves who dropped `task_id` and why from its latest `task_updated` (`to = "dropped"`)
/// event so direct user drops never inherit an earlier judge's decision note and reasonless
/// agent drops are never disguised as user drops.
pub(crate) fn load_drop_attribution(
    cx: &Ctx<'_>,
    task_id: &board_proto::TaskId,
) -> Result<(Option<AgentId>, Option<String>)> {
    let drop_event: Option<(String, Option<String>, i64)> = cx
        .db
        .prepare_cached(
            "SELECT actor, json_extract(payload, '$.reason'), at FROM events \
             WHERE task = ?1 AND type = 'task_updated' \
               AND json_extract(payload, '$.to') = 'dropped' \
             ORDER BY seq DESC LIMIT 1",
        )?
        .query_row([task_id.as_str()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    if let Some((actor, event_reason, drop_at)) = drop_event {
        if actor == "user" {
            return Ok((None, event_reason));
        }
        let agent: AgentId = parse_id(&actor)?;
        let reason = match event_reason {
            Some(r) => Some(r),
            None => {
                let decision_note: Option<String> = cx
                    .db
                    .prepare_cached(
                        "SELECT text FROM notes \
                         WHERE task = ?1 AND kind = 'decision' AND agent = ?2 \
                         ORDER BY at DESC, id DESC LIMIT 1",
                    )?
                    .query_row(params![task_id.as_str(), agent.as_str()], |r| r.get(0))
                    .optional()?;
                match decision_note {
                    Some(text) => Some(text),
                    // Agents occasionally attach their drop rationale (e.g. duplicate/superseded)
                    // as a plain note right before or within 60s after dropping the task.
                    None => cx
                        .db
                        .prepare_cached(
                            "SELECT text FROM notes \
                             WHERE task = ?1 AND agent = ?2 \
                               AND at BETWEEN ?3 AND ?4 \
                             ORDER BY at DESC, id DESC LIMIT 1",
                        )?
                        .query_row(
                            params![
                                task_id.as_str(),
                                agent.as_str(),
                                drop_at.saturating_sub(60_000),
                                drop_at.saturating_add(60_000),
                            ],
                            |r| r.get(0),
                        )
                        .optional()?,
                }
            }
        };
        return Ok((Some(agent), reason));
    }
    // Defensive fallback if the drop event is absent (e.g., legacy or synthetic DB state).
    let note: Option<(String, String)> = cx
        .db
        .prepare_cached(
            "SELECT agent, text FROM notes \
             WHERE task = ?1 AND kind = 'decision' \
             ORDER BY at DESC, id DESC LIMIT 1",
        )?
        .query_row([task_id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    match note {
        Some((agent, text)) => Ok((Some(parse_id(&agent)?), Some(text))),
        None => Ok((None, None)),
    }
}

/// Moves the seen cursor forward to `req.at`, never past now and never back.
pub(crate) fn mark_seen(cx: &mut Ctx<'_>, req: MarkSeen) -> Result<SeenCursor> {
    let at = req.at.timestamp_millis().min(cx.now_ms());
    cx.db.execute(
        "INSERT INTO user_seen (id, seen_at) VALUES (1, ?1) \
         ON CONFLICT (id) DO UPDATE SET seen_at = MAX(seen_at, excluded.seen_at)",
        [at],
    )?;
    let seen_at: i64 = cx
        .db
        .query_row("SELECT seen_at FROM user_seen WHERE id = 1", [], |r| {
            r.get(0)
        })?;
    Ok(SeenCursor {
        seen_at: ts(seen_at),
    })
}
