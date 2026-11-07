use board_proto::{
    AgentId, AnswerAsk, Ask, AskStatus, BoardSlug, CreateAsk, CreatedAsk, EventType,
};
use rusqlite::params;

use crate::config::Settings;
use crate::db::{Ctx, enum_str};
use crate::error::{Result, bad_transition, forbidden, invalid};
use crate::ops::touch_agent;
use crate::store::{NamedParams, get_ask, get_board, get_task, load_asks};
use crate::text::{ago, check_line, check_text};

pub(crate) fn validate_create(req: &CreateAsk, s: &Settings) -> Result<()> {
    if req.question.trim().is_empty() {
        return Err(invalid(
            "question is empty",
            "say what the user must decide or do",
        ));
    }
    check_text("question", &req.question, s.question_chars)?;
    if req.options.len() > 8 {
        return Err(invalid(
            "at most 8 options",
            "offer the few answers that matter, recommended first",
        ));
    }
    for option in &req.options {
        check_line("option", option, board_proto::ASK_OPTION_CHARS_MAX)?;
    }
    Ok(())
}

pub(crate) fn validate_answer(req: &AnswerAsk, s: &Settings) -> Result<()> {
    if req.answer.trim().is_empty() {
        return Err(invalid("answer is empty", "send the answer text"));
    }
    check_text("answer", &req.answer, s.question_chars)
}

/// Creates the ask and warns of the board's other recent open asks, which it may repeat:
/// the ask is made either way.
pub(crate) fn create(cx: &mut Ctx<'_>, req: CreateAsk) -> Result<CreatedAsk> {
    get_board(cx, &req.board)?;
    touch_agent(cx, &req.agent)?;
    if let Some(task) = &req.task {
        let task = get_task(cx, task)?;
        if task.board != req.board {
            return Err(invalid(
                format!(
                    "task {} is on board {}, not {}",
                    task.id, task.board, req.board
                ),
                "ask on the task's board",
            ));
        }
    }
    cx.db.execute(
        "INSERT INTO asks (board, task, agent, kind, question, options, status, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7)",
        params![
            req.board.as_str(),
            req.task.as_ref().map(|t| t.as_str()),
            req.agent.as_str(),
            enum_str(&req.kind),
            req.question,
            serde_json::to_string(&req.options).expect("strings serialize"),
            cx.now_ms(),
        ],
    )?;
    let id = cx.db.last_insert_rowid();
    cx.emit(
        Some(&req.board),
        req.agent.as_str(),
        EventType::AskCreated,
        req.task.as_ref(),
        serde_json::json!({ "id": id, "kind": req.kind }),
    )?;
    let ask = get_ask(cx, id)?;
    let warning = recent_open_asks_warning(cx, &ask)?;
    Ok(CreatedAsk { ask, warning })
}

/// Names the board's other open asks younger than `duplicate_ask_window_secs`, newest first.
fn recent_open_asks_warning(cx: &Ctx<'_>, ask: &Ask) -> Result<Option<String>> {
    let since = cx.now_ms() - cx.s.duplicate_ask_ms();
    let board = ask.board.as_str();
    let mut recent = load_asks(
        cx,
        "board = :board AND status = 'open' AND id != :id AND created_at > :since",
        &vec![(":board", &board), (":id", &ask.id), (":since", &since)],
    )?;
    if recent.is_empty() {
        return Ok(None);
    }
    recent.sort_by_key(|other| std::cmp::Reverse((other.created_at, other.id)));
    let named: Vec<String> = recent
        .iter()
        .map(|other| {
            let task = other
                .task
                .as_ref()
                .map_or("no task".to_owned(), ToString::to_string);
            let by = if other.agent == ask.agent {
                "you".to_owned()
            } else {
                other.agent.to_string()
            };
            let age = ago(cx.now.signed_duration_since(other.created_at));
            format!("#{} on {task} by {by}, {age} ago", other.id)
        })
        .collect();
    let window = cx.s.duplicate_ask_window_secs;
    let (count, unit) = if window.is_multiple_of(60) {
        (window / 60, "minute")
    } else {
        (window, "second")
    };
    Ok(Some(format!(
        "board {board} has {} other open {} under {count} {unit}{} old: {}. If ask #{} repeats \
         one of them, keep one and withdraw the other (ask_withdraw; CLI: board withdraw <id>).",
        recent.len(),
        if recent.len() == 1 { "ask" } else { "asks" },
        if count == 1 { "" } else { "s" },
        named.join("; "),
        ask.id,
    )))
}

pub(crate) fn list(
    cx: &Ctx<'_>,
    status: Option<AskStatus>,
    board: Option<&BoardSlug>,
) -> Result<Vec<Ask>> {
    let status = status.map(|s| enum_str(&s));
    let board = board.map(|b| b.as_str().to_owned());
    let mut clauses = vec!["1 = 1"];
    let mut params: NamedParams<'_> = Vec::new();
    if let Some(status) = &status {
        clauses.push("status = :status");
        params.push((":status", status));
    }
    if let Some(board) = &board {
        clauses.push("board = :board");
        params.push((":board", board));
    }
    load_asks(cx, &clauses.join(" AND "), &params)
}

fn require_open(ask: &Ask) -> Result<()> {
    if ask.status != AskStatus::Open {
        return Err(bad_transition(
            format!("ask {} is already {}", ask.id, enum_str(&ask.status)),
            "nothing to do; open asks are listed by GET /v1/asks?status=open",
        ));
    }
    Ok(())
}

/// Answers an open ask (the user only; checked by the caller).
pub(crate) fn answer(cx: &mut Ctx<'_>, id: i64, answer: &str, via: &str) -> Result<Ask> {
    let ask = get_ask(cx, id)?;
    require_open(&ask)?;
    cx.db.execute(
        "UPDATE asks SET status = 'answered', answer = ?2, answered_via = ?3, answered_at = ?4 \
         WHERE id = ?1",
        params![id, answer, via, cx.now_ms()],
    )?;
    cx.emit(
        Some(&ask.board),
        "user",
        EventType::AskAnswered,
        ask.task.as_ref(),
        serde_json::json!({ "id": id, "agent": ask.agent, "via": via }),
    )?;
    get_ask(cx, id)
}

pub(crate) fn withdraw(cx: &mut Ctx<'_>, id: i64, agent: &AgentId) -> Result<Ask> {
    let ask = get_ask(cx, id)?;
    if ask.agent != *agent {
        return Err(forbidden(
            format!("ask {id} was asked by {}", ask.agent),
            "only the asking agent may withdraw an ask",
        ));
    }
    require_open(&ask)?;
    touch_agent(cx, agent)?;
    cx.db
        .execute("UPDATE asks SET status = 'withdrawn' WHERE id = ?1", [id])?;
    cx.emit(
        Some(&ask.board),
        agent.as_str(),
        EventType::AskWithdrawn,
        ask.task.as_ref(),
        serde_json::json!({ "id": id }),
    )?;
    get_ask(cx, id)
}

/// Claims sending ask `id` to `chat`: true unless it was already sent, or is being sent,
/// to a chat, or `chat` is no longer the paired one (`paired` is false for a configured
/// chat). The notifier sends an ask both when it is created and when a chat pairs, so this
/// makes exactly one of the two send it, to the current chat. Pairing clears every claim,
/// so the new chat gets the asks again, also those whose send failed.
pub(crate) fn claim_telegram_send(
    cx: &mut Ctx<'_>,
    id: i64,
    chat: i64,
    paired: bool,
) -> Result<bool> {
    let claimed = cx.db.execute(
        "UPDATE asks SET telegram_chat_id = ?2 WHERE id = ?1 AND telegram_chat_id IS NULL \
         AND (NOT ?3 OR ?2 = (SELECT chat_id FROM telegram_chat WHERE id = 1))",
        params![id, chat, paired],
    )?;
    Ok(claimed == 1)
}

/// Records the Telegram message an ask was sent as, so replies to it can be matched; not
/// when pairing another chat dropped the claim meanwhile. Returns the ask's status now,
/// which may have changed while it was being sent.
pub(crate) fn set_telegram_message(
    cx: &mut Ctx<'_>,
    id: i64,
    chat: i64,
    message: i64,
) -> Result<AskStatus> {
    cx.db.execute(
        "UPDATE asks SET telegram_message_id = ?3 WHERE id = ?1 AND telegram_chat_id = ?2",
        params![id, chat, message],
    )?;
    Ok(get_ask(cx, id)?.status)
}

/// The chat and message ask `id` was sent as, if it was sent.
pub(crate) fn telegram_message(cx: &Ctx<'_>, id: i64) -> Result<Option<(i64, i64)>> {
    let (chat, message): (Option<i64>, Option<i64>) = cx.db.query_row(
        "SELECT telegram_chat_id, telegram_message_id FROM asks WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(chat.zip(message))
}

pub(crate) fn by_telegram_message(cx: &Ctx<'_>, message: i64) -> Result<Option<i64>> {
    use rusqlite::OptionalExtension;
    Ok(cx
        .db
        .query_row(
            "SELECT id FROM asks WHERE telegram_message_id = ?1",
            [message],
            |r| r.get(0),
        )
        .optional()?)
}
