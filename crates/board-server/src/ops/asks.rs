use board_proto::{AgentId, AnswerAsk, Ask, AskStatus, BoardSlug, CreateAsk, EventType};
use rusqlite::params;

use crate::config::Settings;
use crate::db::{Ctx, enum_str};
use crate::error::{Result, bad_transition, forbidden, invalid};
use crate::ops::touch_agent;
use crate::store::{NamedParams, get_ask, get_board, get_task, load_asks};
use crate::text::{check_line, check_text};

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

pub(crate) fn create(cx: &mut Ctx<'_>, req: CreateAsk) -> Result<Ask> {
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
    get_ask(cx, id)
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

/// Records the Telegram message an ask was sent as, so replies to it can be matched.
/// Returns the ask's status now, which may have changed while it was being sent.
pub(crate) fn set_telegram_message(
    cx: &mut Ctx<'_>,
    id: i64,
    chat: i64,
    message: i64,
) -> Result<AskStatus> {
    cx.db.execute(
        "UPDATE asks SET telegram_chat_id = ?2, telegram_message_id = ?3 WHERE id = ?1",
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
