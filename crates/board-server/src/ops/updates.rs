//! What the user reads apart from the threads (`docs/API.md`, "Updates"): done tasks with
//! their summaries, coordinators' turn endings, and the user's seen cursor.

use board_proto::{
    AgentId, BoardSlug, DoneUpdate, MarkSeen, SAID_CHARS_MAX, Said, SeenCursor, Timestamp,
    UPDATES_MAX, Updates,
};
use rusqlite::{OptionalExtension, Row, params};

use crate::db::{Ctx, parse_id, ts};
use crate::error::{Result, invalid};
use crate::store::load_evidence;
use crate::text::check_text;

/// Turn endings kept per agent and board.
pub(crate) const SAID_KEPT: usize = 50;
/// Default `limit` of `GET /v1/agents/{id}/said`; at most `SAID_KEPT`.
pub(crate) const SAID_LIST_DEFAULT: usize = 20;

/// Checks `AgentStateUpdate.said`.
pub(crate) fn validate_said(text: &str) -> Result<()> {
    if text.trim().is_empty() {
        return Err(invalid(
            "said is empty",
            "send the last message of the turn, or leave said out when there was none",
        ));
    }
    check_text("said", text, SAID_CHARS_MAX)
}

/// Stores `text` once per board `agent` coordinates now, keeping the newest `SAID_KEPT`
/// per agent and board. An agent that coordinates no board stores nothing.
pub(crate) fn record_said(cx: &mut Ctx<'_>, agent: &AgentId, text: &str) -> Result<Vec<Said>> {
    let boards: Vec<String> = cx
        .db
        .prepare_cached("SELECT slug FROM boards WHERE coordinator = ?1 ORDER BY slug")?
        .query_map([agent.as_str()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut stored = Vec::with_capacity(boards.len());
    for board in boards {
        cx.db.execute(
            "INSERT INTO said (agent, board, text, at) VALUES (?1, ?2, ?3, ?4)",
            params![agent.as_str(), board, text, cx.now_ms()],
        )?;
        let id = cx.db.last_insert_rowid();
        cx.db.execute(
            "DELETE FROM said WHERE agent = ?1 AND board = ?2 AND id NOT IN \
             (SELECT id FROM said WHERE agent = ?1 AND board = ?2 ORDER BY id DESC LIMIT ?3)",
            params![agent.as_str(), board, SAID_KEPT as i64],
        )?;
        stored.push(Said {
            id,
            agent: agent.clone(),
            board: parse_id(&board)?,
            text: text.to_owned(),
            at: cx.now,
        });
    }
    Ok(stored)
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
    let more = done.len() > UPDATES_MAX || said.len() > UPDATES_MAX;
    done.truncate(UPDATES_MAX);
    said.truncate(UPDATES_MAX);
    for item in &mut done {
        item.evidence = load_evidence(cx, item.task.as_str())?;
    }
    Ok(Updates {
        seen_at,
        since,
        done,
        said,
        more,
    })
}

/// Moves the seen cursor forward to `req.at`, never past now and never back.
pub(crate) fn mark_seen(cx: &mut Ctx<'_>, req: MarkSeen) -> Result<SeenCursor> {
    let at = req.at.timestamp_millis().min(cx.now_ms());
    cx.db.execute(
        "INSERT INTO user_seen (id, seen_at) VALUES (1, ?1) \
         ON CONFLICT (id) DO UPDATE SET seen_at = MAX(seen_at, excluded.seen_at)",
        [at],
    )?;
    Ok(SeenCursor {
        seen_at: seen_at(cx)?.unwrap_or_else(|| ts(at)),
    })
}
