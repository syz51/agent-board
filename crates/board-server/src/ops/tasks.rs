use std::collections::{HashSet, VecDeque};

use board_proto::{
    AddNote, AgentId, BoardSlug, CreateTask, DoneTask, ErrorCode, EventType, EvidenceKind, Note,
    Priority, Task, TaskId, TaskStatus, Timestamp, UpdateTask,
};
use chrono::TimeDelta;
use rusqlite::{OptionalExtension, params};

use crate::config::Settings;
use crate::db::{Ctx, enum_str, parse_id, ts};
use crate::error::{
    Error, Result, bad_transition, conflict, forbidden, invalid, not_found, refused,
};
use crate::ops::{join_board, touch_agent};
use crate::store::{NamedParams, get_board, get_task, handle_of, load_tasks, seen};
use crate::text::{ago, check_line, check_text};

/// Status changes allowed through `PATCH` (`docs/API.md`).
pub(crate) fn transition_allowed(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    matches!(
        (from, to),
        (Open, Blocked | Dropped)
            | (Doing, Blocked | Review | Open | Dropped)
            | (Blocked, Open | Doing | Dropped)
            | (Review, Doing | Dropped)
            | (Done | Dropped, Open)
    )
}

fn allowed_targets(from: TaskStatus) -> Vec<&'static str> {
    use TaskStatus::*;
    [Open, Doing, Blocked, Review, Done, Dropped]
        .into_iter()
        .filter(|to| transition_allowed(from, *to))
        .map(status_str)
        .collect()
}

pub(crate) fn status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Open => "open",
        TaskStatus::Doing => "doing",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Review => "review",
        TaskStatus::Done => "done",
        TaskStatus::Dropped => "dropped",
    }
}

pub(crate) fn priority_str(priority: Priority) -> &'static str {
    match priority {
        Priority::P1 => "p1",
        Priority::P2 => "p2",
        Priority::P3 => "p3",
    }
}

fn check_body(s: &Settings, body: &str, acceptance: &str) -> Result<()> {
    check_text("body", body, s.body_chars)?;
    check_text("acceptance", acceptance, s.body_chars)
}

pub(crate) fn validate_create(req: &CreateTask, s: &Settings) -> Result<()> {
    check_line("title", &req.title, s.title_chars)?;
    check_body(s, &req.body, &req.acceptance)?;
    if req.blockers.len() > 20 {
        return Err(invalid(
            "a task has at most 20 blockers",
            "split the task, or depend on a parent task instead",
        ));
    }
    Ok(())
}

pub(crate) fn validate_update(req: &UpdateTask, s: &Settings) -> Result<()> {
    if let Some(title) = &req.title {
        check_line("title", title, s.title_chars)?;
    }
    check_body(
        s,
        req.body.as_deref().unwrap_or(""),
        req.acceptance.as_deref().unwrap_or(""),
    )?;
    if let Some(reason) = &req.blocked_reason {
        check_line("blocked_reason", reason, s.note_chars)?;
    }
    if req.blockers.as_ref().is_some_and(|b| b.len() > 20) {
        return Err(invalid(
            "a task has at most 20 blockers",
            "split the task, or depend on a parent task instead",
        ));
    }
    Ok(())
}

pub(crate) fn validate_done(req: &DoneTask, id: &TaskId) -> Result<()> {
    if req.evidence.is_empty() {
        return Err(Error::api(
            ErrorCode::EvidenceRequired,
            format!("done needs evidence for {id}"),
            "send at least one evidence item: a check with its exit_code, a commit, a pr or an artifact",
        ));
    }
    if req.evidence.len() > 20 {
        return Err(invalid(
            "at most 20 evidence items",
            "send the checks and commits that show the acceptance is met",
        ));
    }
    for item in &req.evidence {
        check_line(
            "evidence value",
            &item.value,
            board_proto::EVIDENCE_VALUE_CHARS_MAX,
        )?;
        if item.kind == EvidenceKind::Check && item.exit_code.is_none() {
            return Err(Error::api(
                ErrorCode::EvidenceRequired,
                format!("check evidence {:?} has no exit_code", item.value),
                "send the command's exit_code with each check",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_note(req: &AddNote, s: &Settings) -> Result<()> {
    if req.text.trim().is_empty() {
        return Err(invalid("note text is empty", "send the note text"));
    }
    check_text("text", &req.text, s.note_chars)
}

fn version_conflict(task: &Task, sent: i64) -> Error {
    conflict(
        format!(
            "{} is at version {}; you sent {sent}",
            task.id, task.version
        ),
        format!(
            "re-read {} (task_show) and retry with version {}",
            task.id, task.version
        ),
    )
}

/// The owner's identity for messages: handle and when it was last seen.
fn owner_desc(cx: &Ctx<'_>, owner: &AgentId) -> Result<String> {
    let handle = handle_of(cx, owner.as_str())?;
    Ok(match seen(cx, owner.as_str())? {
        Some(s) => format!(
            "{handle} (seen {} ago)",
            ago(TimeDelta::milliseconds(cx.now_ms() - s.last_seen))
        ),
        None => handle,
    })
}

/// Blockers must exist and not form a cycle. They may be on any board: the task is not
/// ready until they are done, wherever they are.
fn check_blockers(cx: &Ctx<'_>, task: Option<&TaskId>, blockers: &[TaskId]) -> Result<Vec<TaskId>> {
    let mut out: Vec<TaskId> = Vec::with_capacity(blockers.len());
    for blocker in blockers {
        if out.contains(blocker) {
            continue;
        }
        if Some(blocker) == task {
            return Err(invalid(
                format!("{blocker} cannot block itself"),
                "remove it from blockers",
            ));
        }
        if !cx.exists("SELECT 1 FROM tasks WHERE id = ?1", [blocker.as_str()])? {
            let board = blocker.board();
            let next = if cx.exists("SELECT 1 FROM boards WHERE slug = ?1", [board.as_str()])? {
                format!("check the id; task_list board={board} lists the tasks")
            } else {
                format!("there is no board {board}; check the id (board_status lists the boards)")
            };
            return Err(invalid(format!("blocker {blocker} does not exist"), next));
        }
        out.push(blocker.clone());
    }
    if let Some(task) = task {
        // A cycle exists if the task is reachable from a new blocker through blockers.
        let mut queue: VecDeque<String> = out.iter().map(|b| b.to_string()).collect();
        let mut visited: HashSet<String> = HashSet::new();
        let mut stmt = cx
            .db
            .prepare_cached("SELECT blocker FROM task_blockers WHERE task = ?1")?;
        while let Some(current) = queue.pop_front() {
            if !visited.insert(current.clone()) {
                continue;
            }
            let next: Vec<String> = stmt
                .query_map([&current], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for n in next {
                if n == task.as_str() {
                    return Err(invalid(
                        format!(
                            "{current} already waits on {task}, so blocking {task} on it forms a cycle"
                        ),
                        "remove that blocker; split the work if both depend on each other",
                    ));
                }
                queue.push_back(n);
            }
        }
    }
    Ok(out)
}

fn set_blockers(cx: &Ctx<'_>, task: &TaskId, blockers: &[TaskId]) -> Result<()> {
    cx.db
        .execute("DELETE FROM task_blockers WHERE task = ?1", [task.as_str()])?;
    for blocker in blockers {
        cx.db.execute(
            "INSERT INTO task_blockers (task, blocker) VALUES (?1, ?2)",
            params![task.as_str(), blocker.as_str()],
        )?;
    }
    Ok(())
}

pub(crate) fn create(cx: &mut Ctx<'_>, req: CreateTask) -> Result<Task> {
    let board = get_board(cx, &req.board)?;
    touch_agent(cx, &req.agent)?;
    if let Some(parent) = &req.parent {
        if parent.board() != req.board {
            return Err(invalid(
                format!("parent {parent} is on another board"),
                "a parent must be on the same board",
            ));
        }
        if !cx.exists("SELECT 1 FROM tasks WHERE id = ?1", [parent.as_str()])? {
            return Err(invalid(
                format!("parent {parent} does not exist"),
                "check the parent id",
            ));
        }
    }
    let blockers = check_blockers(cx, None, &req.blockers)?;
    if req.claim {
        check_claimable_new(cx, &req, &blockers)?;
    }
    let number: i64 = cx.db.query_row(
        "UPDATE boards SET next_number = next_number + 1 WHERE slug = ?1 RETURNING next_number - 1",
        [req.board.as_str()],
        |r| r.get(0),
    )?;
    let id = TaskId::new(&req.board, number as u64);
    let now = cx.now_ms();
    let not_before = req
        .not_before
        .map(|at| at.timestamp_millis())
        .filter(|&at| at > now);
    // A claimed task starts in doing, owned by its creator: no other agent sees it ready.
    let (status, owner, doing_since) = if req.claim {
        (TaskStatus::Doing, Some(req.agent.as_str()), Some(now))
    } else {
        (TaskStatus::Open, None, None)
    };
    cx.db.execute(
        "INSERT INTO tasks (id, board, number, parent, title, body, acceptance, status, priority, \
         auto, created_by, version, created_at, updated_at, not_before, owner, doing_since) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?13, ?8, ?9, ?10, 1, ?11, ?11, ?12, ?14, ?15)",
        params![
            id.as_str(),
            req.board.as_str(),
            number,
            req.parent.as_ref().map(TaskId::as_str),
            req.title,
            req.body,
            req.acceptance,
            priority_str(req.priority),
            req.auto.unwrap_or(board.autopilot),
            req.agent.as_str(),
            now,
            not_before,
            status_str(status),
            owner,
            doing_since,
        ],
    )?;
    set_blockers(cx, &id, &blockers)?;
    join_board(cx, &req.board, &req.agent)?;
    let mut payload = serde_json::json!({ "title": req.title, "priority": req.priority });
    if let Some(parent) = &req.parent {
        payload["parent"] = parent.as_str().into();
    }
    if req.claim {
        payload["claimed"] = true.into();
    }
    cx.emit(
        Some(&req.board),
        req.agent.as_str(),
        EventType::TaskCreated,
        Some(&id),
        payload,
    )?;
    if req.claim {
        // As a claim of an open task records it, so history and clients that follow
        // claims see who works on it.
        cx.emit(
            Some(&req.board),
            req.agent.as_str(),
            EventType::TaskClaimed,
            Some(&id),
            serde_json::json!({ "previous_owner": null }),
        )?;
    }
    get_task(cx, &id)
}

/// A task created with `claim` starts as a claim would leave it, so it must pass what a
/// claim checks: no blocker left to finish, no `not_before` ahead, and room in the
/// creator's `doing` limit. Checked before anything is written.
fn check_claimable_new(cx: &Ctx<'_>, req: &CreateTask, blockers: &[TaskId]) -> Result<()> {
    let mut waiting: Vec<String> = Vec::new();
    for b in blockers {
        let status: String = cx.db.query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            [b.as_str()],
            |r| r.get(0),
        )?;
        if status != "done" {
            waiting.push(format!("{b} ({status})"));
        }
    }
    if !waiting.is_empty() {
        return Err(bad_transition(
            format!(
                "a claimed task starts in doing, but it would wait on {}",
                waiting.join(", ")
            ),
            "add it without claim; it becomes ready when its blockers are done",
        ));
    }
    if req
        .not_before
        .is_some_and(|at| at.timestamp_millis() > cx.now_ms())
    {
        return Err(bad_transition(
            "a claimed task starts in doing, but not_before is still ahead",
            "add it without claim, or without not_before if it should start now",
        ));
    }
    let doing = doing_tasks(cx, &req.agent, None)?;
    if doing.len() >= cx.s.max_doing_per_agent as usize {
        return Err(refused(
            format!(
                "you already have {} in doing; nothing was created",
                doing.join(", ")
            ),
            format!(
                "move {0} to review or blocked first (task_update {0} status=review), finish it \
                 (task_done) or release it, then add the task with claim again; or add it \
                 without claim",
                doing[0]
            ),
        ));
    }
    Ok(())
}

/// Announces every `not_before` the clock has passed, once: a `task_updated` event by
/// `server` (no field changes; `not_before_passed` names the time) for open tasks, which
/// wakes the coordinator. The task itself is left as it is. Run by the sweeper.
pub(crate) fn announce_not_before(cx: &mut Ctx<'_>) -> Result<usize> {
    let due: Vec<(TaskId, BoardSlug, i64, String)> = cx
        .db
        .prepare_cached(
            "UPDATE tasks SET not_before_announced = 1 \
             WHERE not_before <= ?1 AND not_before_announced = 0 \
             RETURNING id, board, not_before, status",
        )?
        .query_map([cx.now_ms()], |r| {
            Ok((
                parse_id(&r.get::<_, String>(0)?)?,
                parse_id(&r.get::<_, String>(1)?)?,
                r.get(2)?,
                r.get(3)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut announced = 0;
    for (id, board, at, status) in &due {
        // Only an open task can become ready; for the rest the date no longer matters.
        if status != "open" {
            continue;
        }
        cx.emit(
            Some(board),
            "server",
            EventType::TaskUpdated,
            Some(id),
            serde_json::json!({ "fields": [], "not_before_passed": ts(*at) }),
        )?;
        announced += 1;
    }
    Ok(announced)
}

/// Refuses to start a task whose `not_before` has not passed.
fn check_not_before(cx: &Ctx<'_>, task: &Task) -> Result<()> {
    match task.not_before {
        Some(at) if at > cx.now => Err(bad_transition(
            format!("{} waits until {}", task.id, at.format("%Y-%m-%d %H:%MZ")),
            format!(
                "pick a ready task (task_list ready=true), or clear its not_before (task_update {} not_before=none) if it should start now",
                task.id
            ),
        )),
        _ => Ok(()),
    }
}

/// A time as stored: whole milliseconds.
fn to_millis(at: Timestamp) -> Timestamp {
    ts(at.timestamp_millis())
}

/// Filters of `GET /v1/tasks`.
#[derive(Debug, Default)]
pub(crate) struct TaskFilter {
    pub board: Option<BoardSlug>,
    pub status: Option<TaskStatus>,
    pub owner: Option<AgentId>,
    pub ready: Option<bool>,
    pub limit: usize,
}

pub(crate) fn list(cx: &Ctx<'_>, filter: TaskFilter) -> Result<Vec<Task>> {
    let board = filter.board.as_ref().map(|b| b.as_str().to_owned());
    let status = filter.status.map(status_str);
    let owner = filter.owner.as_ref().map(|o| o.as_str().to_owned());
    let mut clauses = vec!["1 = 1"];
    let mut params: NamedParams<'_> = Vec::new();
    if let Some(board) = &board {
        clauses.push("t.board = :board");
        params.push((":board", board));
    }
    if let Some(status) = &status {
        clauses.push("t.status = :status");
        params.push((":status", status));
    }
    if let Some(owner) = &owner {
        clauses.push("t.owner = :owner");
        params.push((":owner", owner));
    }
    if filter.ready == Some(true) {
        clauses.push("t.status = 'open'");
    }
    let tasks = load_tasks(cx, &clauses.join(" AND "), &params)?;
    Ok(tasks
        .into_iter()
        .filter(|t| filter.ready.is_none_or(|r| t.ready == r))
        .take(filter.limit)
        .collect())
}

/// The tasks the agent has in `doing`, other than `except`.
fn doing_tasks(cx: &Ctx<'_>, agent: &AgentId, except: Option<&TaskId>) -> Result<Vec<String>> {
    Ok(cx
        .db
        .prepare_cached(
            "SELECT id FROM tasks WHERE owner = ?1 AND status = 'doing' AND id IS NOT ?2 \
             ORDER BY id",
        )?
        .query_map(params![agent.as_str(), except.map(TaskId::as_str)], |r| {
            r.get(0)
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn check_doing_limit(cx: &Ctx<'_>, agent: &AgentId, task: &TaskId) -> Result<()> {
    let doing = doing_tasks(cx, agent, Some(task))?;
    if doing.len() >= cx.s.max_doing_per_agent as usize {
        return Err(refused(
            format!("you already have {} in doing", doing.join(", ")),
            format!(
                "finish {0} (task_done), move it to review or blocked, or release it before taking {task}",
                doing[0]
            ),
        ));
    }
    Ok(())
}

/// Blockers of `task` that are not done, with their statuses.
fn open_blockers(cx: &Ctx<'_>, task: &TaskId) -> Result<Vec<(String, String)>> {
    Ok(cx
        .db
        .prepare_cached(
            "SELECT b.blocker, t.status FROM task_blockers b JOIN tasks t ON t.id = b.blocker \
             WHERE b.task = ?1 AND t.status != 'done' ORDER BY t.number, t.board",
        )?
        .query_map([task.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Refuses to start `task` while a blocker is not done.
fn check_blockers_done(cx: &Ctx<'_>, task: &TaskId) -> Result<()> {
    let waiting = open_blockers(cx, task)?;
    if waiting.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = waiting.iter().map(|(b, s)| format!("{b} ({s})")).collect();
    Err(bad_transition(
        format!("{task} waits on {}", list.join(", ")),
        "pick a ready task (task_list ready=true), or help finish its blockers",
    ))
}

/// Claims an open task: one conditional update, so concurrent claims have one winner.
pub(crate) fn claim(cx: &mut Ctx<'_>, id: &TaskId, agent: &AgentId) -> Result<Task> {
    touch_agent(cx, agent)?;
    let task = get_task(cx, id)?;
    let owned_by_me = task.owner.as_ref() == Some(agent);
    if owned_by_me && task.status == TaskStatus::Doing {
        return Ok(task);
    }
    if let Some(owner) = task.owner.as_ref().filter(|o| *o != agent)
        && !task.owner_stale
        && !task.status.is_closed()
    {
        return Err(refused(
            format!("claim refused: {} owns {id}", owner_desc(cx, owner)?),
            "pick another task from ready (task_list ready=true)",
        ));
    }
    if task.status != TaskStatus::Open {
        let next = if task.owner_stale && !task.status.is_closed() {
            format!("its owner is stale: take it with task_take {id}")
        } else if task.status.is_closed() {
            format!("reopen it first (task_update {id} status=open) if it needs more work")
        } else {
            "pick another task from ready (task_list ready=true)".to_owned()
        };
        return Err(bad_transition(
            format!(
                "{id} is {}; only open tasks can be claimed",
                status_str(task.status)
            ),
            next,
        ));
    }
    check_blockers_done(cx, id)?;
    check_not_before(cx, &task)?;
    check_doing_limit(cx, agent, id)?;
    let now = cx.now_ms();
    let previous = task.owner.clone();
    let updated = cx.db.execute(
        "UPDATE tasks SET status = 'doing', owner = ?2, doing_since = ?3, version = version + 1, \
         updated_at = ?3 WHERE id = ?1 AND status = 'open' AND version = ?4",
        params![id.as_str(), agent.as_str(), now, task.version],
    )?;
    if updated != 1 {
        return Err(refused(
            format!("claim refused: {id} changed while claiming"),
            "re-read it and pick another task from ready",
        ));
    }
    join_board(cx, &task.board, agent)?;
    cx.emit(
        Some(&task.board),
        agent.as_str(),
        EventType::TaskClaimed,
        Some(id),
        serde_json::json!({ "previous_owner": previous }),
    )?;
    get_task(cx, id)
}

/// Records a refused claim for the stats (the refused transaction itself rolled back).
pub(crate) fn record_refusal(cx: &mut Ctx<'_>, id: &TaskId, agent: &AgentId) -> Result<()> {
    cx.db.execute(
        "INSERT INTO claim_refusals (task, agent, at) VALUES (?1, ?2, ?3)",
        params![id.as_str(), agent.as_str(), cx.now_ms()],
    )?;
    Ok(())
}

/// Takes a task whose owner is stale.
pub(crate) fn take(cx: &mut Ctx<'_>, id: &TaskId, agent: &AgentId) -> Result<Task> {
    touch_agent(cx, agent)?;
    let task = get_task(cx, id)?;
    let Some(owner) = task.owner.clone() else {
        return Err(bad_transition(
            format!("nobody owns {id}"),
            format!("claim it with task_claim {id}"),
        ));
    };
    if owner == *agent {
        return Ok(task);
    }
    if task.status.is_closed() {
        return Err(bad_transition(
            format!("{id} is {}", status_str(task.status)),
            format!("reopen it first (task_update {id} status=open) if it needs more work"),
        ));
    }
    if !task.owner_stale {
        let hours = cx.s.owner_stale_after_secs / 3600;
        return Err(refused(
            format!(
                "take refused: {} owns {id}; owners count as stale after {hours}h unseen",
                owner_desc(cx, &owner)?
            ),
            "pick another task from ready (task_list ready=true)",
        ));
    }
    let status = match task.status {
        TaskStatus::Blocked => TaskStatus::Blocked,
        _ => TaskStatus::Doing,
    };
    if task.status == TaskStatus::Open {
        check_blockers_done(cx, id)?;
        check_not_before(cx, &task)?;
    }
    if status == TaskStatus::Doing {
        check_doing_limit(cx, agent, id)?;
    }
    let now = cx.now_ms();
    cx.db.execute(
        "UPDATE tasks SET owner = ?2, status = ?3, doing_since = CASE WHEN ?3 = 'doing' THEN ?4 \
         ELSE doing_since END, version = version + 1, updated_at = ?4 WHERE id = ?1",
        params![id.as_str(), agent.as_str(), status_str(status), now],
    )?;
    join_board(cx, &task.board, agent)?;
    cx.emit(
        Some(&task.board),
        agent.as_str(),
        EventType::TaskTaken,
        Some(id),
        serde_json::json!({ "from": owner, "status": status }),
    )?;
    get_task(cx, id)
}

fn require_owner(task: &Task, agent: &AgentId, action: &str) -> Result<()> {
    match &task.owner {
        Some(owner) if owner == agent => Ok(()),
        Some(owner) => Err(forbidden(
            format!("only the owner {owner} may {action} {}", task.id),
            if task.owner_stale {
                format!(
                    "the owner is stale: take it first with task_take {}",
                    task.id
                )
            } else {
                format!("add a note instead (task_note {})", task.id)
            },
        )),
        None => Err(forbidden(
            format!("nobody owns {}, so it cannot be {action}d by you", task.id),
            format!("claim it first with task_claim {}", task.id),
        )),
    }
}

/// Who may change the status of `task` (`docs/API.md`, "Tasks"): anyone while it has no
/// owner or is closed; otherwise its owner, the board's coordinator or the user. Others
/// take a stale owner's task first, which is logged.
fn require_status_rights(
    cx: &Ctx<'_>,
    task: &Task,
    agent: &AgentId,
    by_user: bool,
    action: &str,
) -> Result<()> {
    if task.owner.is_none() || task.status.is_closed() || by_user {
        return Ok(());
    }
    if get_board(cx, &task.board)?.coordinator.as_ref() == Some(agent) {
        return Ok(());
    }
    require_owner(task, agent, action)
}

pub(crate) fn release(
    cx: &mut Ctx<'_>,
    id: &TaskId,
    agent: &AgentId,
    by_user: bool,
) -> Result<Task> {
    touch_agent(cx, agent)?;
    let task = get_task(cx, id)?;
    if task.owner.is_none() {
        // Nothing to release: refused with the "claim it first" hint.
        require_owner(&task, agent, "release")?;
    }
    require_status_rights(cx, &task, agent, by_user, "release")?;
    if task.status.is_closed() {
        return Err(bad_transition(
            format!("{id} is {}", status_str(task.status)),
            "closed tasks have nothing to release",
        ));
    }
    cx.db.execute(
        "UPDATE tasks SET status = 'open', owner = NULL, doing_since = NULL, blocked_reason = NULL, \
         version = version + 1, updated_at = ?2 WHERE id = ?1",
        params![id.as_str(), cx.now_ms()],
    )?;
    cx.emit(
        Some(&task.board),
        agent.as_str(),
        EventType::TaskReleased,
        Some(id),
        serde_json::json!({ "from": task.status }),
    )?;
    get_task(cx, id)
}

pub(crate) fn update(
    cx: &mut Ctx<'_>,
    id: &TaskId,
    req: UpdateTask,
    by_user: bool,
) -> Result<Task> {
    touch_agent(cx, &req.agent)?;
    let task = get_task(cx, id)?;
    if req.version != task.version {
        return Err(version_conflict(&task, req.version));
    }
    let agent = &req.agent;
    let from = task.status;
    let to = req.status.unwrap_or(from);
    let mut owner = task.owner.clone();
    let mut fields: Vec<&str> = Vec::new();

    if to != from {
        if !transition_allowed(from, to) {
            let next = match to {
                TaskStatus::Doing if from == TaskStatus::Open => {
                    format!("claim it with task_claim {id}")
                }
                TaskStatus::Done => format!("mark it done with task_done {id} and evidence"),
                _ => format!(
                    "from {} it can move to: {}",
                    status_str(from),
                    allowed_targets(from).join(", ")
                ),
            };
            return Err(bad_transition(
                format!(
                    "{id} cannot go from {} to {}",
                    status_str(from),
                    status_str(to)
                ),
                next,
            ));
        }
        require_status_rights(cx, &task, agent, by_user, "move")?;
        match to {
            // Open means unowned, whatever it was before.
            TaskStatus::Open => owner = None,
            // Whoever moves a task to doing works on it, as with claim and take.
            TaskStatus::Doing => {
                check_doing_limit(cx, agent, id)?;
                owner = Some(agent.clone());
            }
            _ => {}
        }
        fields.push("status");
    }
    if owner != task.owner {
        fields.push("owner");
    }

    let blockers = match &req.blockers {
        Some(new) => {
            let new = check_blockers(cx, Some(id), new)?;
            // The same set in another order is no change.
            let as_set = |list: &[TaskId]| list.iter().cloned().collect::<HashSet<TaskId>>();
            if as_set(&new) != as_set(&task.blockers) {
                fields.push("blockers");
            }
            new
        }
        None => task.blockers.clone(),
    };

    let blocked_reason = if to == TaskStatus::Blocked {
        let reason = req
            .blocked_reason
            .clone()
            .or_else(|| task.blocked_reason.clone())
            .filter(|r| !r.trim().is_empty());
        let mut undone = false;
        for b in &blockers {
            let status: Option<String> = cx
                .db
                .query_row(
                    "SELECT status FROM tasks WHERE id = ?1",
                    [b.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            undone |= status.is_some_and(|s| s != "done");
        }
        if reason.is_none() && !undone {
            return Err(invalid(
                format!("blocked needs a reason or a blocker that is not done ({id})"),
                "send blocked_reason, or blockers naming the tasks it waits on",
            ));
        }
        reason
    } else {
        if req
            .blocked_reason
            .as_ref()
            .is_some_and(|r| !r.trim().is_empty())
        {
            return Err(invalid(
                "blocked_reason applies only to blocked tasks",
                format!("send status=blocked with the reason, or leave blocked_reason out ({id})"),
            ));
        }
        None
    };
    if blocked_reason != task.blocked_reason {
        fields.push("blocked_reason");
    }

    let mut text_field = |name: &'static str, new: &Option<String>, old: &str| -> String {
        match new {
            Some(v) if v != old => {
                fields.push(name);
                v.clone()
            }
            _ => old.to_owned(),
        }
    };
    let title = text_field("title", &req.title, &task.title);
    let body = text_field("body", &req.body, &task.body);
    let acceptance = text_field("acceptance", &req.acceptance, &task.acceptance);
    let priority = req.priority.unwrap_or(task.priority);
    if priority != task.priority {
        fields.push("priority");
    }
    let auto = req.auto.unwrap_or(task.auto);
    if auto != task.auto {
        fields.push("auto");
    }
    // A time already passed means no wait, as on create.
    let not_before = match req.not_before {
        Some(new) => new.map(to_millis).filter(|&at| at > cx.now),
        None => task.not_before,
    };
    if not_before != task.not_before {
        fields.push("not_before");
    }
    if fields.is_empty() {
        return Ok(task);
    }

    let now = cx.now_ms();
    cx.db.execute(
        "UPDATE tasks SET status = ?2, owner = ?3, title = ?4, body = ?5, acceptance = ?6, \
         priority = ?7, auto = ?8, blocked_reason = ?9, version = version + 1, updated_at = ?10, \
         not_before_announced = CASE WHEN not_before IS ?11 THEN not_before_announced ELSE 0 END, \
         not_before = ?11, \
         doing_since = CASE WHEN ?2 = 'doing' THEN COALESCE(doing_since, ?10) \
                            WHEN ?2 = 'open' THEN NULL ELSE doing_since END, \
         done_at = CASE WHEN ?2 = 'done' THEN done_at ELSE NULL END \
         WHERE id = ?1",
        params![
            id.as_str(),
            status_str(to),
            owner.as_ref().map(AgentId::as_str),
            title,
            body,
            acceptance,
            priority_str(priority),
            auto,
            blocked_reason,
            now,
            not_before.map(|at| at.timestamp_millis()),
        ],
    )?;
    if fields.contains(&"blockers") {
        set_blockers(cx, id, &blockers)?;
    }
    if from.is_closed() && to == TaskStatus::Open {
        // A reopened feedback item is live again.
        crate::ops::feedback::reopen(cx, id)?;
    }
    if let Some(new_owner) = owner.as_ref().filter(|o| task.owner.as_ref() != Some(*o)) {
        join_board(cx, &task.board, new_owner)?;
    }
    let mut payload = serde_json::json!({ "fields": fields });
    if to != from {
        payload["from"] = serde_json::json!(from);
        payload["to"] = serde_json::json!(to);
    }
    if fields.contains(&"title") {
        payload["title"] = serde_json::json!(title);
    }
    if fields.contains(&"not_before") {
        payload["not_before"] = serde_json::json!(not_before);
    }
    if fields.contains(&"owner") {
        payload["owner"] = serde_json::json!(owner);
        payload["previous_owner"] = serde_json::json!(task.owner);
    }
    cx.emit(
        Some(&task.board),
        agent.as_str(),
        EventType::TaskUpdated,
        Some(id),
        payload,
    )?;
    get_task(cx, id)
}

pub(crate) fn done(cx: &mut Ctx<'_>, id: &TaskId, req: DoneTask) -> Result<Task> {
    touch_agent(cx, &req.agent)?;
    let task = get_task(cx, id)?;
    if req.version != task.version {
        return Err(version_conflict(&task, req.version));
    }
    require_owner(&task, &req.agent, "complete")?;
    if !matches!(task.status, TaskStatus::Doing | TaskStatus::Review) {
        return Err(bad_transition(
            format!(
                "{id} is {}; only doing or review tasks can be done",
                status_str(task.status)
            ),
            match task.status {
                TaskStatus::Blocked => format!("resume it first (task_update {id} status=doing)"),
                _ => format!("claim it first with task_claim {id}"),
            },
        ));
    }
    let now = cx.now_ms();
    for item in &req.evidence {
        cx.db.execute(
            "INSERT INTO evidence (task, kind, value, exit_code, agent, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id.as_str(),
                enum_str(&item.kind),
                item.value,
                item.exit_code,
                req.agent.as_str(),
                now
            ],
        )?;
    }
    cx.db.execute(
        "UPDATE tasks SET status = 'done', done_at = ?2, blocked_reason = NULL, \
         version = version + 1, updated_at = ?2 WHERE id = ?1",
        params![id.as_str(), now],
    )?;
    let doing_since: Option<i64> = cx.db.query_row(
        "SELECT doing_since FROM tasks WHERE id = ?1",
        [id.as_str()],
        |r| r.get(0),
    )?;
    cx.emit(
        Some(&task.board),
        req.agent.as_str(),
        EventType::TaskDone,
        Some(id),
        serde_json::json!({
            "evidence": req.evidence,
            "doing_ms": doing_since.map(|d| now - d),
        }),
    )?;
    get_task(cx, id)
}

pub(crate) fn add_note(cx: &mut Ctx<'_>, id: &TaskId, req: AddNote) -> Result<Note> {
    touch_agent(cx, &req.agent)?;
    let task = get_task(cx, id)?;
    let now = cx.now_ms();
    cx.db.execute(
        "INSERT INTO notes (task, agent, kind, text, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id.as_str(),
            req.agent.as_str(),
            enum_str(&req.kind),
            req.text,
            now
        ],
    )?;
    let note_id = cx.db.last_insert_rowid();
    cx.emit(
        Some(&task.board),
        req.agent.as_str(),
        EventType::NoteAdded,
        Some(id),
        serde_json::json!({ "note": note_id, "kind": req.kind }),
    )?;
    Ok(Note {
        id: note_id,
        task: id.clone(),
        agent: req.agent,
        kind: req.kind,
        text: req.text,
        at: cx.now,
    })
}

/// Parses a task id from a path segment.
pub(crate) fn parse_task_id(raw: &str) -> Result<TaskId> {
    raw.parse().map_err(|e: board_proto::InvalidId| {
        not_found(
            e.to_string(),
            "task ids look like <board>-<number>, such as factory-12",
        )
    })
}
