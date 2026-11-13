//! Judges (`docs/API.md`, "Daemon dispatch"): short-lived threads that the holder of a
//! board's coordinator lease registers, so they have the coordinator's rights on that
//! board while they run.

use std::sync::LazyLock;

use board_proto::{
    AgentId, AgentState, BoardSlug, EventType, JUDGE_HOURS_MAX, JUDGE_TASKS_MAX, Judge, JudgeKind,
    RegisterJudge,
};
use rusqlite::{Row, named_params, params};

use crate::db::{Ctx, enum_str, parse_enum, parse_id, parse_json, ts};
use crate::error::{Result, forbidden, invalid};
use crate::store::{agent_by_id_or_handle, find_agent, get_board_uncounted, get_task};

/// The longest a registration lasts.
const JUDGE_MS: i64 = JUDGE_HOURS_MAX * 3_600_000;

/// A registration `j` that gives rights at `:now`: its agent has not ended, it has not
/// expired, and the agent that registered it still holds the board's coordinator lease.
pub(crate) const LIVE: &str = "j.ended_at IS NULL AND j.expires_at > :now \
     AND EXISTS (SELECT 1 FROM boards jb WHERE jb.slug = j.board \
                 AND jb.coordinator = j.registered_by AND jb.coordinator_expires_at > :now)";

/// Whether `:agent` has the coordinator's rights on `:board`.
static RIGHTS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT EXISTS (SELECT 1 FROM boards WHERE slug = :board AND coordinator = :agent \
                        AND coordinator_expires_at > :now) \
             OR EXISTS (SELECT 1 FROM judges j WHERE j.board = :board AND j.agent = :agent \
                        AND {LIVE})"
    )
});

/// Boards whose live coordinator or live judge `:agent` is.
static RIGHTS_BOARDS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT slug FROM boards WHERE coordinator = :agent AND coordinator_expires_at > :now \
         UNION SELECT j.board FROM judges j WHERE j.agent = :agent AND {LIVE} \
         ORDER BY 1"
    )
});

/// A registration's columns, then whether its agent is running (or not registered yet).
const COLUMNS: &str = "j.board, j.agent, j.kind, j.tasks, j.registered_by, j.created_at, \
     j.expires_at, COALESCE((SELECT a.state = 'running' FROM agents a WHERE a.id = j.agent), 1)";

/// The board's live registrations, oldest first.
static LIST: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {COLUMNS} FROM judges j WHERE j.board = :board AND {LIVE} \
         ORDER BY j.created_at, j.agent"
    )
});

/// Validates a registration at the API boundary: repeats are dropped, then the task list
/// is bounded, and a review names what it reviews.
pub(crate) fn validate_register(mut req: RegisterJudge) -> Result<RegisterJudge> {
    let mut seen = std::collections::HashSet::new();
    req.tasks.retain(|t| seen.insert(t.clone()));
    if req.tasks.len() > JUDGE_TASKS_MAX {
        return Err(invalid(
            format!("tasks names {} different tasks", req.tasks.len()),
            format!(
                "name at most {JUDGE_TASKS_MAX} tasks per judge; start another judge for the rest"
            ),
        ));
    }
    if req.kind == JudgeKind::Review && req.tasks.is_empty() {
        return Err(invalid(
            "a review judge names no task",
            "send tasks with the task (or tasks) it reviews",
        ));
    }
    Ok(req)
}

/// Whether `agent` has the coordinator's rights on `board`: it is the board's live
/// coordinator, or a live judge of the board.
pub(crate) fn has_coordinator_rights(
    cx: &Ctx<'_>,
    board: &BoardSlug,
    agent: &AgentId,
) -> Result<bool> {
    Ok(cx.db.prepare_cached(&RIGHTS)?.query_row(
        named_params! {
            ":board": board.as_str(),
            ":agent": agent.as_str(),
            ":now": cx.now_ms(),
        },
        |r| r.get(0),
    )?)
}

/// Boards on which `agent` has the coordinator's rights (as coordinator or judge), by slug.
pub(crate) fn boards_with_rights(cx: &Ctx<'_>, agent: &AgentId) -> Result<Vec<String>> {
    Ok(cx
        .db
        .prepare_cached(&RIGHTS_BOARDS)?
        .query_map(
            named_params! { ":agent": agent.as_str(), ":now": cx.now_ms() },
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?)
}

fn judge_row(r: &Row<'_>) -> rusqlite::Result<Judge> {
    Ok(Judge {
        board: parse_id(&r.get::<_, String>(0)?)?,
        agent: parse_id(&r.get::<_, String>(1)?)?,
        kind: parse_enum(&r.get::<_, String>(2)?)?,
        tasks: parse_json(&r.get::<_, String>(3)?)?,
        registered_by: parse_id(&r.get::<_, String>(4)?)?,
        created_at: ts(r.get(5)?),
        expires_at: ts(r.get(6)?),
        running: r.get(7)?,
    })
}

/// The board's live judges, oldest first.
pub(crate) fn list(cx: &Ctx<'_>, board: &BoardSlug) -> Result<Vec<Judge>> {
    get_board_uncounted(cx, board)?;
    Ok(cx
        .db
        .prepare_cached(&LIST)?
        .query_map(
            named_params! { ":board": board.as_str(), ":now": cx.now_ms() },
            judge_row,
        )?
        .collect::<rusqlite::Result<_>>()?)
}

/// Registers `req.agent` (an id or handle) as a judge of `board`. Only the board's live
/// coordinator may. A live registration of the same agent keeps its expiry, so registering
/// again never extends the rights; one that is over is replaced.
pub(crate) fn register(
    cx: &mut Ctx<'_>,
    caller: &AgentId,
    board: &BoardSlug,
    req: RegisterJudge,
) -> Result<Judge> {
    let found = get_board_uncounted(cx, board)?;
    if found.coordinator.as_ref() != Some(caller) {
        return Err(forbidden(
            format!("{caller} does not hold the coordinator lease of {board}"),
            "only the lease holder registers judges; to have a judgment made, add a note on \
             the task (task_note) for the board's coordinator",
        ));
    }
    let agent = agent_by_id_or_handle(cx, &req.agent)?;
    if &agent == caller {
        return Err(invalid(
            format!("{caller} names itself as a judge of {board}"),
            "register the judge's own agent id, such as t3:<its thread id>",
        ));
    }
    // An agent that has not registered yet (its session is still starting) may be named;
    // one that has ended would never use the rights.
    if find_agent(cx, &agent)?.is_some_and(|a| a.state == AgentState::Ended) {
        return Err(invalid(
            format!("agent {agent} has ended"),
            "start a new judge thread and register its agent id",
        ));
    }
    for task in &req.tasks {
        let found = get_task(cx, task)?;
        if &found.board != board {
            return Err(invalid(
                format!("task {task} is on board {}, not {board}", found.board),
                format!(
                    "name only tasks of {board}, or register the judge on {}",
                    found.board
                ),
            ));
        }
    }
    let now = cx.now_ms();
    // Registrations that are over never give rights again: drop them, so the table holds
    // about the judges running.
    cx.db.execute(
        "DELETE FROM judges WHERE board = ?1 AND (ended_at IS NOT NULL OR expires_at <= ?2)",
        params![board.as_str(), now],
    )?;
    let tasks = serde_json::to_string(&req.tasks).expect("ids serialize");
    // A row left after the delete is live, or waits for its registrant to hold the lease
    // again: it keeps its expiry.
    cx.db.execute(
        "INSERT INTO judges (board, agent, kind, tasks, registered_by, created_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT (board, agent) DO UPDATE SET kind = ?3, tasks = ?4, registered_by = ?5",
        params![
            board.as_str(),
            agent.as_str(),
            enum_str(&req.kind),
            tasks,
            caller.as_str(),
            now,
            now.saturating_add(JUDGE_MS),
        ],
    )?;
    let judge = cx.db.query_row(
        &format!("SELECT {COLUMNS} FROM judges j WHERE j.board = ?1 AND j.agent = ?2"),
        params![board.as_str(), agent.as_str()],
        judge_row,
    )?;
    cx.emit(
        Some(board),
        caller.as_str(),
        EventType::BoardUpdated,
        None,
        serde_json::json!({
            "fields": ["judges"],
            "judge": judge.agent,
            "kind": judge.kind,
            "tasks": judge.tasks,
            "expires_at": judge.expires_at,
        }),
    )?;
    Ok(judge)
}

/// Ends every registration of `agent`, whose session ended: a judge's rights do not come
/// back when its thread resumes.
pub(crate) fn end_agent(cx: &mut Ctx<'_>, agent: &AgentId) -> Result<()> {
    cx.db.execute(
        "UPDATE judges SET ended_at = ?2 WHERE agent = ?1 AND ended_at IS NULL",
        params![agent.as_str(), cx.now_ms()],
    )?;
    Ok(())
}
