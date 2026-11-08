use std::collections::{HashSet, VecDeque};

use board_proto::{
    AddNote, AgentId, AgentState, BoardSlug, CreateTask, DoneTask, ErrorCode, EventType,
    EvidenceKind, Note, Priority, SUMMARY_CHARS_MAX, Task, TaskId, TaskStatus, Timestamp,
    UpdateTask,
};
use chrono::TimeDelta;
use rusqlite::{OptionalExtension, params};

use crate::config::Settings;
use crate::db::{Ctx, enum_str, parse_id, proposal_sql, ready_sql, ts};
use crate::error::{
    Error, Result, bad_transition, conflict, forbidden, invalid, not_found, refused,
};
use crate::ops::{join_board, note_agent_seen, touch_agent};
use crate::store::{
    NamedParams, find_agent, get_board, get_task, handle_of, is_live, load_tasks_up_to, seen,
};
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
    if let Some(summary) = &req.summary {
        if summary.trim().is_empty() {
            return Err(invalid(
                "summary is empty",
                "send what changed and what the user should know, or leave summary out",
            ));
        }
        check_text("summary", summary, SUMMARY_CHARS_MAX)?;
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
    // A proposal waits for another agent's verdict, on every board: it is stored not auto
    // whatever was asked, and only an accept sets auto.
    let proposal = board_proto::is_proposal_body(&req.body);
    if proposal && req.claim {
        return Err(invalid(
            "a proposal cannot be claimed by the agent that files it",
            "file it without claim; another agent records a verdict, and an accept sets auto",
        ));
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
            !proposal && req.auto.unwrap_or(board.autopilot),
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
                "finish {0} (task_done) or {1}, then add the task with claim again; or add it \
                 without claim",
                doing[0],
                park_hint(&doing[0])
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

/// Announces each task left in `review` for `review_quiet_secs` with no note from its
/// owner: a `task_updated` event by `server` (no field changes; `quiet_review` names the
/// last note or the move to review, `in_review_since` the move). The snapshot turns it into
/// an alert for the owner, or for the board's coordinator when the owner is gone. While the
/// task stays quiet it is announced again every `review_quiet_secs`; a note from the owner
/// or a new move to review starts the clock again. Run by the sweeper.
pub(crate) fn announce_quiet_reviews(cx: &mut Ctx<'_>) -> Result<usize> {
    let quiet: Vec<(TaskId, BoardSlug, i64, i64)> = cx
        .db
        .prepare_cached(
            "SELECT id, board, since, MAX(since, noted) FROM ( \
               SELECT t.id, t.board, \
                 COALESCE((SELECT MAX(e.at) FROM events e WHERE e.task = t.id \
                   AND e.type = 'task_updated' \
                   AND json_extract(e.payload, '$.to') = 'review'), t.updated_at) AS since, \
                 COALESCE((SELECT MAX(n.at) FROM notes n \
                   WHERE n.task = t.id AND n.agent = t.owner), 0) AS noted, \
                 COALESCE((SELECT MAX(e.at) FROM events e WHERE e.task = t.id \
                   AND e.type = 'task_updated' \
                   AND json_extract(e.payload, '$.quiet_review') IS NOT NULL), 0) AS announced \
               FROM tasks t WHERE t.status = 'review') \
             WHERE MAX(since, noted, announced) <= ?1",
        )?
        .query_map([cx.now_ms() - cx.s.review_quiet_ms()], |r| {
            Ok((
                parse_id(&r.get::<_, String>(0)?)?,
                parse_id(&r.get::<_, String>(1)?)?,
                r.get(2)?,
                r.get(3)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, board, since, last) in &quiet {
        cx.emit(
            Some(board),
            "server",
            EventType::TaskUpdated,
            Some(id),
            serde_json::json!({
                "fields": [],
                "quiet_review": ts(*last),
                "in_review_since": ts(*since),
            }),
        )?;
    }
    Ok(quiet.len())
}

/// The seq of `?1`'s last state change to `running`, 0 when it has none. Read backwards
/// along `events_actor`, so it stops at the newest match.
pub(crate) const LAST_RUN: &str = "SELECT COALESCE((SELECT seq FROM events \
     WHERE actor = ?1 AND type = 'agent_state_changed' \
       AND json_extract(payload, '$.to') = 'running' \
     ORDER BY seq DESC LIMIT 1), 0)";

/// When `?1` stopped running: its first state change to anything but `running` after seq
/// `?2`, its last change to `running`. A turn ending, a usage-limit pause and the session's
/// end all follow the same stop, so the spell away starts at the first of them.
pub(crate) const AWAY_SINCE: &str = "SELECT at FROM events \
     WHERE actor = ?1 AND seq > ?2 AND type = 'agent_state_changed' \
       AND json_extract(payload, '$.to') <> 'running' \
     ORDER BY seq LIMIT 1";

/// The seq of the last event that put task `?1` in `doing` or in new hands (a claim, a
/// take, a move to `doing`, a handover), 0 when none. Read backwards along `events_task`.
pub(crate) const ENTERED_DOING: &str = "SELECT COALESCE((SELECT seq FROM events \
     WHERE task = ?1 AND (type IN ('task_claimed', 'task_taken') \
       OR (type = 'task_updated' AND (json_extract(payload, '$.to') = 'doing' \
         OR json_extract(payload, '$.owner') IS NOT NULL))) \
     ORDER BY seq DESC LIMIT 1), 0)";

/// How many idle notices about agent `?3` task `?1` has after seq `?2`, and at or after
/// `?4` when that is not null, and when the first of them was recorded (null when none).
pub(crate) const TOLD: &str = "SELECT COUNT(*), MIN(at) FROM events \
     WHERE task = ?1 AND seq > ?2 AND type = 'task_updated' \
       AND json_extract(payload, '$.idle_doing') IS NOT NULL \
       AND json_extract(payload, '$.away') = ?3 AND (?4 IS NULL OR at >= ?4)";

/// Whether a T3 thread agent `?1` delegated, to depth `?2`, is at work: running and seen
/// after `?3`, or waiting for a usage-limit reset still to come (`?4`, now), paused or with
/// its session closed meanwhile.
pub(crate) const DELEGATE_AT_WORK: &str = concat!(
    "WITH RECURSIVE ",
    crate::ops::leases::delegated_down_cte!(),
    " SELECT EXISTS (SELECT 1 FROM down JOIN agents a ON a.id = down.id \
       WHERE down.depth > 0 \
         AND ((a.state = 'running' AND a.last_seen > ?3) \
           OR (a.state IN ('paused', 'ended') AND a.paused_until > ?4)))"
);

/// Announces each task in `doing` on an active board whose owner has been away for
/// `doing_idle_secs` while no thread it delegated (at any depth) is at work: a
/// `task_updated` event by `server` (no field changes; `idle_doing` names when the spell
/// away began, `away` the owner). Away means not running since then (idle, paused with no
/// reset ahead, or ended), or marked running but not seen for that long, as a turn cut
/// short by an error leaves a thread. An owner waiting for a usage-limit reset still to
/// come (paused, or ended meanwhile) is not away. Twice per spell at most: once at
/// `doing_idle_secs`, and once more at twice that, when the board's coordinator stands in
/// for a T3 owner, unless the first came that late. An owner that runs again and stops
/// starts a new spell, and so does a running owner seen again. The snapshot shows it as an
/// alert to the owner, or to the board's coordinator, and the owner's device daemon wakes
/// a T3 owner. Run by the sweeper.
pub(crate) fn announce_idle_doing(cx: &mut Ctx<'_>) -> Result<usize> {
    let now = cx.now_ms();
    let cutoff = now - cx.s.doing_idle_ms();
    let doing: Vec<(TaskId, BoardSlug, String, String, i64, Option<i64>)> = cx
        .db
        .prepare_cached(
            "SELECT t.id, t.board, t.owner, a.state, a.last_seen, a.paused_until FROM tasks t \
             JOIN agents a ON a.id = t.owner JOIN boards b ON b.slug = t.board \
             WHERE t.status = 'doing' AND b.status = 'active'",
        )?
        .query_map([], |r| {
            Ok((
                parse_id(&r.get::<_, String>(0)?)?,
                parse_id(&r.get::<_, String>(1)?)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut announced = 0;
    for (id, board, owner, state, last_seen, paused_until) in &doing {
        let running = state == "running";
        let waits_for_reset =
            matches!(state.as_str(), "paused" | "ended") && paused_until.is_some_and(|p| p > now);
        if waits_for_reset {
            continue;
        }
        let last_run: i64 = cx
            .db
            .prepare_cached(LAST_RUN)?
            .query_row([owner], |r| r.get(0))?;
        let since = if running {
            *last_seen
        } else {
            cx.db
                .prepare_cached(AWAY_SINCE)?
                .query_row(params![owner, last_run], |r| r.get(0))
                .optional()?
                .unwrap_or(*last_seen)
        };
        if since > cutoff {
            continue;
        }
        // Told this spell: notices after the owner last ran and after the task last came to
        // it. A session closing later moves neither, so it is the same spell. A running
        // owner's spell starts when it was last seen, so one seen again is told again.
        let entered: i64 = cx
            .db
            .prepare_cached(ENTERED_DOING)?
            .query_row([id.as_str()], |r| r.get(0))?;
        let (told, first): (i64, Option<i64>) = cx.db.prepare_cached(TOLD)?.query_row(
            params![
                id.as_str(),
                last_run.max(entered),
                owner,
                running.then_some(since)
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // Once more at twice the period: the snapshot shows the coordinator a T3 owner's
        // notice only from then, and alerts are read once, so one that snapshot in between
        // passed the first. A first notice that came that late needs no repeat.
        let twice = since + 2 * cx.s.doing_idle_ms();
        let due = match told {
            0 => true,
            1 => now >= twice && first.is_some_and(|f| f < twice),
            _ => false,
        };
        if !due {
            continue;
        }
        let at_work: bool = cx.db.prepare_cached(DELEGATE_AT_WORK)?.query_row(
            params![owner, crate::ops::leases::DELEGATION_DEPTH, cutoff, now],
            |r| r.get(0),
        )?;
        if at_work {
            continue;
        }
        cx.emit(
            Some(board),
            "server",
            EventType::TaskUpdated,
            Some(id),
            serde_json::json!({
                "fields": [],
                "idle_doing": ts(since),
                "away": owner,
            }),
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
    /// Only pending proposals (true), or none of them (false).
    pub proposal: Option<bool>,
    pub limit: usize,
}

pub(crate) fn list(cx: &Ctx<'_>, filter: TaskFilter) -> Result<Vec<Task>> {
    let board = filter.board.as_ref().map(|b| b.as_str().to_owned());
    let status = filter.status.map(status_str);
    let owner = filter
        .owner
        .as_ref()
        .map(|o| crate::store::agent_by_id_or_handle(cx, o))
        .transpose()?
        .map(|o| o.as_str().to_owned());
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
    // The same test as `Task::ready`, so the limit applies in SQL.
    let ready = match filter.ready {
        Some(true) => Some(ready_sql("t")),
        Some(false) => Some(format!("NOT {}", ready_sql("t"))),
        None => None,
    };
    if let Some(ready) = &ready {
        clauses.push(ready);
    }
    let proposal = match filter.proposal {
        Some(true) => Some(proposal_sql("t")),
        Some(false) => Some(format!("NOT {}", proposal_sql("t"))),
        None => None,
    };
    if let Some(proposal) = &proposal {
        clauses.push(proposal);
    }
    load_tasks_up_to(cx, &clauses.join(" AND "), &params, Some(filter.limit))
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

/// How to park the task in doing by what it waits on (the coordinator prompt's rule).
fn park_hint(task: &str) -> String {
    format!(
        "park it by what it waits on: CI or a review -> task_update {task} status=review, then \
         task_note {task} naming the PR; another task, the user or anything else -> task_update \
         {task} status=blocked with blocked_reason (or blockers); only a date -> release {task} \
         and set its not_before"
    )
}

fn check_doing_limit(cx: &Ctx<'_>, agent: &AgentId, task: &TaskId) -> Result<()> {
    let doing = doing_tasks(cx, agent, Some(task))?;
    if doing.len() >= cx.s.max_doing_per_agent as usize {
        return Err(refused(
            format!("you already have {} in doing", doing.join(", ")),
            format!(
                "finish {0} (task_done) or {1}, then take {task}",
                doing[0],
                park_hint(&doing[0])
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
            format!("its owner is stale: take it with {}", take_hint(id))
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
    if task.proposal {
        return Err(unaccepted_proposal(id));
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
                    "the owner is stale: take it first with {}",
                    take_hint(&task.id)
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

/// Who may hand `task` to `target` (`docs/API.md`, "Tasks"): its owner, the board's
/// coordinator or the user. `owned` is whether the task has an owner once the request's
/// status change applies, and `to` that status. The target must be a live agent.
fn check_handover(
    cx: &Ctx<'_>,
    task: &Task,
    agent: &AgentId,
    by_user: bool,
    target: &AgentId,
    owned: bool,
    to: TaskStatus,
) -> Result<()> {
    let id = &task.id;
    let allowed = by_user
        || task.owner.as_ref() == Some(agent)
        || get_board(cx, &task.board)?.coordinator.as_ref() == Some(agent);
    if !allowed {
        return Err(match &task.owner {
            Some(owner) => forbidden(
                format!(
                    "only the owner {owner}, the board's coordinator or the user may hand {id} over"
                ),
                if task.owner_stale {
                    format!(
                        "the owner is stale: take it first ({}), then hand it over",
                        take_hint(id)
                    )
                } else {
                    format!("ask the owner in a note (task_note {id})")
                },
            ),
            None => forbidden(
                format!(
                    "nobody owns {id}; only the board's coordinator or the user may give it to an agent"
                ),
                if task.status == TaskStatus::Open {
                    format!("to work on it yourself, claim it (task_claim {id})")
                } else {
                    format!(
                        "to work on it yourself, resume it (task_update {id} status=doing) without owner"
                    )
                },
            ),
        });
    }
    if to == TaskStatus::Open {
        return Err(invalid(
            format!("an open task has no owner, so {id} cannot be handed over"),
            if task.status == TaskStatus::Open {
                format!("claim it first (task_claim {id}), then hand it over with owner")
            } else {
                format!(
                    "leave owner out to release {id}, or send owner without status=open to hand it over"
                )
            },
        ));
    }
    if to.is_closed() {
        return Err(bad_transition(
            if task.status == to {
                format!("{id} is {}, so it cannot be handed over", status_str(to))
            } else {
                format!(
                    "{id} cannot be handed over while it moves to {}",
                    status_str(to)
                )
            },
            if task.status == to {
                format!("reopen it (task_update {id} status=open) and claim it to work on it")
            } else {
                format!("leave owner out to move it to {}", status_str(to))
            },
        ));
    }
    if !owned {
        return Err(invalid(
            format!("nobody owns {id}, so it cannot be handed over"),
            format!("resume it for that agent: task_update {id} status=doing owner={target}"),
        ));
    }
    let Some(found) = find_agent(cx, target)? else {
        return Err(not_found(
            format!("agent {target} is not registered"),
            "hand it to an agent that has started a session: its id shows among the peers in board_snapshot",
        ));
    };
    if found.state == AgentState::Ended || !is_live(cx, target.as_str())? {
        return Err(invalid(
            format!(
                "agent {target} is not live (its session ended or it is stale), so it would never finish {id}"
            ),
            "hand it to a live agent among the peers in board_snapshot, or to yourself",
        ));
    }
    Ok(())
}

/// How to take a stale owner's task, through MCP or the CLI.
/// The refusal of an unaccepted proposal moving to doing. The hint keeps the claimant,
/// who may be the proposer, from accepting it itself.
fn unaccepted_proposal(id: &TaskId) -> Error {
    bad_transition(
        format!(
            "{id} is a proposal not yet accepted: it becomes work only once another agent accepts it"
        ),
        "pick other ready work (task_list ready=true); an agent that did not file it reviews it \
         with a `verdict:` decision note, and an accept sets auto",
    )
}

fn take_hint(id: &TaskId) -> String {
    format!("task_claim {id} take=true, or board take {id}")
}

/// `check_doing_limit` for `owner`, worded for the caller when someone else is the owner.
fn check_owner_doing_limit(
    cx: &Ctx<'_>,
    owner: &AgentId,
    caller: &AgentId,
    task: &TaskId,
) -> Result<()> {
    if owner == caller {
        return check_doing_limit(cx, owner, task);
    }
    let doing = doing_tasks(cx, owner, Some(task))?;
    if doing.len() >= cx.s.max_doing_per_agent as usize {
        return Err(refused(
            format!("{owner} already has {} in doing", doing.join(", ")),
            format!(
                "hand {task} to another agent, or wait until {owner} finishes {0} or moves it to review or blocked",
                doing[0]
            ),
        ));
    }
    Ok(())
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
    crate::ops::leases::release_for_task(cx, id, &task.board, agent.as_str())?;
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
    if by_user {
        note_agent_seen(cx, &req.agent)?;
    } else {
        touch_agent(cx, &req.agent)?;
    }
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
        // The proposal gate holds on every way into doing, not only claim: a proposal
        // moved to blocked and on to doing is still unaccepted.
        if to == TaskStatus::Doing
            && board_proto::is_unaccepted_proposal(
                req.auto.unwrap_or(task.auto),
                req.body.as_deref().unwrap_or(&task.body),
                task.feedback.is_some(),
            )
        {
            return Err(unaccepted_proposal(id));
        }
        match to {
            // Open means unowned, whatever it was before.
            TaskStatus::Open => owner = None,
            // Whoever moves a task to doing works on it, as with claim and take.
            TaskStatus::Doing => owner = Some(agent.clone()),
            _ => {}
        }
        fields.push("status");
    }
    // A handover names another owner than the status change leaves.
    let target = req
        .owner
        .as_ref()
        .map(|t| crate::store::agent_by_id_or_handle(cx, t))
        .transpose()?;
    if let Some(target) = target.as_ref().filter(|t| owner.as_ref() != Some(*t)) {
        check_handover(cx, &task, agent, by_user, target, owner.is_some(), to)?;
        owner = Some(target.clone());
    }
    // A task entering doing, or changing hands in doing, counts against its owner's limit.
    if let Some(worker) = owner
        .as_ref()
        .filter(|_| to == TaskStatus::Doing && (from != to || owner != task.owner))
    {
        check_owner_doing_limit(cx, worker, agent, id)?;
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
         done_at = CASE WHEN ?2 = 'done' THEN done_at ELSE NULL END, \
         summary = CASE WHEN ?2 = 'done' THEN summary ELSE NULL END \
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
    // Work on the task has stopped: done or dropped, or open (unowned, for the next owner).
    if to != from && (to.is_closed() || to == TaskStatus::Open) {
        crate::ops::leases::release_for_task(cx, id, &task.board, agent.as_str())?;
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
        "UPDATE tasks SET status = 'done', done_at = ?2, blocked_reason = NULL, summary = ?3, \
         version = version + 1, updated_at = ?2 WHERE id = ?1",
        params![id.as_str(), now, req.summary],
    )?;
    crate::ops::leases::release_for_task(cx, id, &task.board, req.agent.as_str())?;
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
