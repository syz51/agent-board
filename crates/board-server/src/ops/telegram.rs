//! Telegram state: pairing (one-time codes created on the host, redeemed by `/start <code>`
//! from a private chat, which then becomes the notifier's chat) and the scheduled alerts.

use std::collections::BTreeMap;

use board_proto::{Ask, AskStatus, TaskId, Timestamp};
use rusqlite::{OptionalExtension, named_params, params};

use crate::auth::hash_token;
use crate::db::{Ctx, parse_id, ready_sql, ts};
use crate::error::{Result, invalid};

/// How long a pairing code stays valid.
pub(crate) const CODE_TTL_MS: i64 = 15 * 60 * 1000;
/// Letters and digits that cannot be misread for one another.
const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
/// 12 characters from 31: about 59 bits.
const CODE_LEN: usize = 12;

/// The paired chat, if any.
pub(crate) fn paired_chat(cx: &Ctx<'_>) -> Result<Option<i64>> {
    Ok(cx
        .db
        .query_row("SELECT chat_id FROM telegram_chat WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()?)
}

/// Creates a pairing code and returns it; only its hash is stored. Refused while a chat is
/// paired, unless `replace`: the paired chat then stays until the code is redeemed.
pub(crate) fn create_code(cx: &mut Ctx<'_>, replace: bool) -> Result<String> {
    if !replace && paired_chat(cx)?.is_some() {
        return Err(invalid(
            "a Telegram chat is already paired",
            "pass --replace to pair another chat in its place",
        ));
    }
    cx.db.execute(
        "DELETE FROM telegram_pairing_codes WHERE expires_at <= ?1",
        [cx.now_ms()],
    )?;
    let code = new_code();
    cx.db.execute(
        "INSERT INTO telegram_pairing_codes (code_hash, replaces, created_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            hash_token(&code),
            replace,
            cx.now_ms(),
            cx.now_ms() + CODE_TTL_MS
        ],
    )?;
    Ok(code)
}

fn new_code() -> String {
    let mut bytes = [0u8; CODE_LEN];
    rand::fill(&mut bytes);
    // 256 is not a multiple of 31, so reject the top values to stay uniform.
    let mut code = String::with_capacity(CODE_LEN);
    let limit = 256 - 256 % ALPHABET.len();
    while code.len() < CODE_LEN {
        for &b in &bytes {
            if (b as usize) < limit && code.len() < CODE_LEN {
                code.push(ALPHABET[b as usize % ALPHABET.len()] as char);
            }
        }
        rand::fill(&mut bytes);
    }
    code
}

/// Pairs `chat_id` when `code` is a live pairing code, and either no chat is paired or the
/// code replaces the paired chat; every code is used up by a successful pairing. Message
/// ids from earlier chats are forgotten, since Telegram numbers messages per chat, and so
/// are the stuck and stalled alerts and the digest sent there, so the new chat gets them
/// too. Returns whether the chat was paired.
pub(crate) fn redeem(cx: &mut Ctx<'_>, code: &str, chat_id: i64) -> Result<bool> {
    let code = code.trim().to_ascii_lowercase();
    let replaces: Option<bool> = cx
        .db
        .query_row(
            "SELECT replaces FROM telegram_pairing_codes \
             WHERE code_hash = ?1 AND expires_at > ?2",
            params![hash_token(&code), cx.now_ms()],
            |r| r.get(0),
        )
        .optional()?;
    match replaces {
        None => return Ok(false),
        Some(false) if paired_chat(cx)?.is_some() => return Ok(false),
        Some(_) => {}
    }
    cx.db.execute("DELETE FROM telegram_pairing_codes", [])?;
    cx.db.execute(
        "INSERT INTO telegram_chat (id, chat_id, paired_at) VALUES (1, ?1, ?2) \
         ON CONFLICT (id) DO UPDATE SET chat_id = ?1, paired_at = ?2",
        params![chat_id, cx.now_ms()],
    )?;
    cx.db.execute(
        "UPDATE asks SET telegram_message_id = NULL, telegram_chat_id = NULL \
         WHERE telegram_chat_id IS NOT NULL OR telegram_message_id IS NOT NULL",
        [],
    )?;
    cx.db.execute("DELETE FROM telegram_stuck", [])?;
    cx.db.execute("DELETE FROM telegram_digest", [])?;
    cx.db
        .execute("UPDATE telegram_stalled SET sent_at = NULL", [])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_use_the_alphabet() {
        for _ in 0..50 {
            let code = new_code();
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.bytes().all(|b| ALPHABET.contains(&b)), "{code}");
        }
        assert_ne!(new_code(), new_code());
    }
}

/// A task in doing or review that has gone quiet, as the stuck alert reports it.
#[derive(Debug)]
pub(crate) struct Stuck {
    pub id: TaskId,
    pub title: String,
    pub status: String,
    /// The owner's handle, if the task has an owner.
    pub owner: Option<String>,
    /// The newest event not by the server, or 0 when none is left (events are pruned
    /// after a year); an alert is recorded against it.
    pub seq: i64,
    /// That event's type and actor (a handle for an agent), when there is one.
    pub last: Option<(String, String)>,
    /// That event's time, else the task's last change.
    pub quiet_since: Timestamp,
}

/// Every task in doing or review, on a board that is not closed, whose newest event not by
/// the server is at least `quiet_ms` old and not yet reported. Left out, since a usage
/// limit has its own message: tasks whose owner stopped on a limit and neither resumed nor
/// saw it reset at least `quiet_ms` ago (the quiet time restarts when the owner resumes, or
/// at the reset), and those of a coordinator paused until further notice. The owner's
/// events are read rather than its state, since a resumed owner no longer shows its reset.
pub(crate) fn stuck(cx: &Ctx<'_>, quiet_ms: i64) -> Result<Vec<Stuck>> {
    let now = cx.now_ms();
    Ok(cx
        .db
        .prepare_cached(
            "SELECT t.id, t.title, t.status, a.handle, COALESCE(e.seq, 0), e.type, \
               COALESCE(la.handle, e.actor), COALESCE(e.at, t.updated_at) \
             FROM tasks t \
             JOIN boards b ON b.slug = t.board \
             LEFT JOIN events e ON e.seq = (SELECT MAX(x.seq) FROM events x \
               WHERE x.task = t.id AND x.actor != 'server') \
             LEFT JOIN agents a ON a.id = t.owner \
             LEFT JOIN agents la ON la.id = e.actor \
             WHERE t.status IN ('doing', 'review') AND b.status != 'closed' \
               AND COALESCE(e.at, t.updated_at) <= ?1 \
               AND NOT (COALESCE(a.state, '') = 'paused' AND a.role = 'coordinator' \
                 AND a.paused_until IS NULL) \
               AND NOT EXISTS (SELECT 1 FROM telegram_stuck s \
                 WHERE s.task = t.id AND s.seq = COALESCE(e.seq, 0)) \
               AND NOT EXISTS (SELECT 1 FROM events p WHERE p.actor = t.owner \
                 AND p.type = 'agent_state_changed' \
                 AND json_extract(p.payload, '$.to') = 'paused' \
                 AND unixepoch(json_extract(p.payload, '$.paused_until')) * 1000 > ?1 \
                 AND NOT EXISTS (SELECT 1 FROM events r WHERE r.actor = p.actor \
                   AND r.seq > p.seq AND r.at <= ?1 AND r.type = 'agent_state_changed' \
                   AND json_extract(r.payload, '$.to') IN ('running', 'idle'))) \
             ORDER BY t.board, t.number",
        )?
        .query_map(params![now - quiet_ms], |r| {
            let kind: Option<String> = r.get(5)?;
            let actor: Option<String> = r.get(6)?;
            Ok(Stuck {
                id: parse_id(&r.get::<_, String>(0)?)?,
                title: r.get(1)?,
                status: r.get(2)?,
                owner: r.get(3)?,
                seq: r.get(4)?,
                last: kind.zip(actor),
                quiet_since: ts(r.get(7)?),
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Records the stuck alerts sent, each against its task's newest event, so a task is
/// reported again only after it changes. Forgets the alerts of tasks that left doing and
/// review.
pub(crate) fn record_stuck(cx: &mut Ctx<'_>, sent: &[(TaskId, i64)]) -> Result<()> {
    cx.db.execute(
        "DELETE FROM telegram_stuck WHERE task NOT IN \
         (SELECT id FROM tasks WHERE status IN ('doing', 'review'))",
        [],
    )?;
    let now = cx.now_ms();
    for (task, seq) in sent {
        cx.db.execute(
            "INSERT INTO telegram_stuck (task, seq, sent_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (task) DO UPDATE SET seq = excluded.seq, sent_at = excluded.sent_at",
            params![task.as_str(), seq, now],
        )?;
    }
    Ok(())
}

/// An active autopilot board whose ready work sits unclaimed, as the stalled alert reports
/// it.
#[derive(Debug)]
pub(crate) struct Stalled {
    pub board: String,
    /// Ready auto tasks on the board.
    pub ready: usize,
    /// When a check first saw the board stalled; an alert is recorded against it.
    pub since: Timestamp,
    /// The coordinator that holds the board's lease: its handle, state and when it was
    /// last seen.
    pub coordinator: Option<(String, String, Timestamp)>,
}

/// Events that move a task into or out of doing, over events alias `x`.
const DOING_MOVES: &str = "(x.type IN ('task_claimed', 'task_taken', 'task_released', \
    'task_done') OR (x.type = 'task_updated' AND 'doing' IN \
    (json_extract(x.payload, '$.from'), json_extract(x.payload, '$.to'))))";

/// Tracks which boards are stalled and returns those due an alert. A board is stalled while
/// it is active, has autopilot on, has no task in doing and has a ready auto task, unless the
/// coordinator that holds its lease is paused on a usage limit (that pause has its own
/// message), or the account usage stops its new work (over its stop, or a weekly pace of
/// 0: the coordinator is woken when it eases). Each check records when it first saw a board stalled; a board that stops being
/// stalled, or where a task moved into or out of doing since then, starts over. A board is
/// due once it has stayed stalled for `quiet_ms` and was not yet reported since then. So
/// work that became claimable by any route (autopilot switched on, a board resumed, a
/// blocker done, a pause over) gives the coordinator the whole threshold to claim it.
pub(crate) fn stalled(cx: &mut Ctx<'_>, quiet_ms: i64) -> Result<Vec<Stalled>> {
    let now = cx.now_ms();
    let max_seq = cx.max_seq()?;
    let rows = cx
        .db
        .prepare_cached(&format!(
            "SELECT b.slug, a.handle, a.state, a.last_seen, \
               (SELECT COUNT(*) FROM tasks t WHERE t.board = b.slug AND t.auto = 1 \
                 AND {ready}), \
               s.since, s.sent_at, \
               EXISTS (SELECT 1 FROM events x WHERE x.board = b.slug AND x.seq > s.seq \
                 AND {DOING_MOVES}), \
               b.usage_stop_at, b.parallel_tasks \
             FROM boards b \
             LEFT JOIN agents a ON a.id = b.coordinator AND b.coordinator_expires_at > :now \
             LEFT JOIN telegram_stalled s ON s.board = b.slug \
             WHERE b.autopilot = 1 AND b.status = 'active' \
               AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.board = b.slug \
                 AND t.status = 'doing') \
               AND NOT (COALESCE(a.state, '') = 'paused' AND a.paused_until IS NULL) \
               AND NOT (COALESCE(a.state, '') IN ('paused', 'ended') \
                 AND a.paused_until > :now) \
             ORDER BY b.slug",
            ready = ready_sql("t"),
        ))?
        .query_map(
            named_params! { ":now": now, ":cutoff": cx.stale_cutoff() },
            |r| {
                let handle: Option<String> = r.get(1)?;
                let state: Option<String> = r.get(2)?;
                let seen: Option<i64> = r.get(3)?;
                Ok((
                    r.get::<_, String>(0)?,
                    handle
                        .zip(state)
                        .zip(seen)
                        .map(|((h, s), at)| (h, s, ts(at))),
                    usize::try_from(r.get::<_, i64>(4)?).unwrap_or_default(),
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<bool>>(7)?.unwrap_or(false),
                    (r.get::<_, u32>(8)?, r.get::<_, u32>(9)?),
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let usage = super::usage::latest(cx)?;
    let held = |(stop_at, parallel): (u32, u32)| {
        usage.as_ref().is_some_and(|u| {
            u.over(stop_at, cx.now).is_some() || u.pace_limit(parallel.max(1), cx.now) == 0
        })
    };
    let mut kept = Vec::new();
    let mut due = Vec::new();
    for (board, coordinator, ready, since, sent_at, moved, limits) in rows {
        if ready == 0 || held(limits) {
            continue;
        }
        match since {
            Some(since) if !moved => {
                if sent_at.is_none() && since <= now - quiet_ms {
                    due.push(Stalled {
                        board: board.clone(),
                        ready,
                        since: ts(since),
                        coordinator,
                    });
                }
            }
            _ => {
                cx.db.execute(
                    "INSERT INTO telegram_stalled (board, seq, since, sent_at) \
                     VALUES (?1, ?2, ?3, NULL) ON CONFLICT (board) DO UPDATE \
                     SET seq = excluded.seq, since = excluded.since, sent_at = NULL",
                    params![board, max_seq, now],
                )?;
            }
        }
        kept.push(board);
    }
    cx.db.execute(
        "DELETE FROM telegram_stalled WHERE board NOT IN (SELECT value FROM json_each(?1))",
        [serde_json::to_string(&kept).expect("strings serialize")],
    )?;
    Ok(due)
}

/// Records the stalled alerts sent, each against when its board was first seen stalled, so
/// a board is reported again only once it stalls anew.
pub(crate) fn record_stalled(cx: &mut Ctx<'_>, sent: &[(String, Timestamp)]) -> Result<()> {
    let now = cx.now_ms();
    for (board, since) in sent {
        cx.db.execute(
            "UPDATE telegram_stalled SET sent_at = ?1 WHERE board = ?2 AND since = ?3",
            params![now, board, since.timestamp_millis()],
        )?;
    }
    Ok(())
}

/// The local date (`YYYY-MM-DD`) of the last digest sent, if any.
pub(crate) fn digest_sent(cx: &Ctx<'_>) -> Result<Option<String>> {
    Ok(cx
        .db
        .query_row("SELECT day FROM telegram_digest WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()?)
}

/// Records the digest of local date `day` as sent.
pub(crate) fn record_digest(cx: &mut Ctx<'_>, day: &str) -> Result<()> {
    cx.db.execute(
        "INSERT INTO telegram_digest (id, day) VALUES (1, ?1) \
         ON CONFLICT (id) DO UPDATE SET day = excluded.day",
        [day],
    )?;
    Ok(())
}

/// What the daily digest lists: the web view's Needs you page, per board.
#[derive(Debug, Default)]
pub(crate) struct Digest {
    /// Boards in slug order, each with its open asks.
    pub boards: BTreeMap<String, Vec<Ask>>,
}

impl Digest {
    pub(crate) fn is_empty(&self) -> bool {
        self.boards.is_empty()
    }
}

/// The Needs you items, by the web view's rule (`web/src/screens/asks.tsx`): the open asks,
/// which are what waits on the user's decision or action.
pub(crate) fn digest(cx: &Ctx<'_>) -> Result<Digest> {
    let mut digest = Digest::default();
    for ask in crate::ops::asks::list(cx, Some(AskStatus::Open), None, None)? {
        digest
            .boards
            .entry(ask.board.to_string())
            .or_default()
            .push(ask);
    }
    Ok(digest)
}
