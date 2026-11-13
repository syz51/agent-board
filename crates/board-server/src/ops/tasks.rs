use std::collections::{HashSet, VecDeque};

use board_proto::{
    AddNote, AgentId, AgentState, Board, BoardSlug, BoardStatus, CreateTask, DoneTask, ErrorCode,
    EventType, EvidenceKind, Note, NoteKind, Priority, RankTasks, RankedTasks, SUMMARY_CHARS_MAX,
    TASKS_Q_CHARS_MAX, TASKS_Q_WORDS_MAX, Task, TaskId, TaskStatus, Timestamp, UpdateTask,
};
use chrono::{DateTime, TimeDelta};
use rusqlite::{OptionalExtension, params};

use crate::config::Settings;
use crate::db::{Ctx, enum_str, parse_id, proposal_sql, ready_sql, ts};
use crate::error::{
    Error, Result, bad_transition, conflict, forbidden, invalid, not_found, refused,
};
use crate::ops::feedback::{DUPLICATE_DEPTH_MAX, LIVE_ITEMS};
use crate::ops::{join_board, note_agent_seen, touch_agent};
use crate::store::{
    NamedParams, SEARCH_RANK, SEARCH_RANK_ANY, find_agent, get_board_uncounted, get_task,
    handle_of, is_live, load_tasks_found, load_tasks_up_to, seen,
};
use crate::text::{ago, check_line, check_text};

/// Status changes allowed through `PATCH` (`docs/API.md`). A parked task moves between
/// `review` and `blocked` directly as what it waits on changes.
pub(crate) fn transition_allowed(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    matches!(
        (from, to),
        (Open, Blocked | Dropped)
            | (Doing, Blocked | Review | Open | Dropped)
            | (Blocked, Open | Doing | Review | Dropped)
            | (Review, Doing | Blocked | Dropped)
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

/// Checks a `PATCH` body and returns it as `update` takes it: a blank `blocked_reason`,
/// which clients send for "no reason", is read as absent, so it changes nothing.
pub(crate) fn validate_update(mut req: UpdateTask, s: &Settings) -> Result<UpdateTask> {
    if let Some(title) = &req.title {
        check_line("title", title, s.title_chars)?;
    }
    check_body(
        s,
        req.body.as_deref().unwrap_or(""),
        req.acceptance.as_deref().unwrap_or(""),
    )?;
    req.blocked_reason = req.blocked_reason.filter(|r| !r.trim().is_empty());
    if let Some(reason) = &req.blocked_reason {
        check_line("blocked_reason", reason, s.note_chars)?;
    }
    // A blank reason is read as absent, so a drop that needs one is refused for want of it.
    req.reason = req.reason.filter(|r| !r.trim().is_empty());
    if let Some(reason) = &req.reason {
        if req.status != Some(TaskStatus::Dropped) {
            return Err(invalid(
                "reason applies only to a drop",
                "send status=dropped with the reason; for a blocked task send it as \
                 blocked_reason; else add the text as a note (task_note)",
            ));
        }
        check_text("reason", reason, s.note_chars)?;
    }
    if req.blockers.as_ref().is_some_and(|b| b.len() > 20) {
        return Err(invalid(
            "a task has at most 20 blockers",
            "split the task, or depend on a parent task instead",
        ));
    }
    Ok(req)
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
    let board = get_board_uncounted(cx, &req.board)?;
    touch_agent(cx, &req.agent)?;
    if let Some(parent) = &req.parent {
        if parent.board() != req.board {
            return Err(invalid(
                format!("parent {parent} is on another board"),
                "a parent must be on the same board; for a task on another board, name it in the body, or make that task wait for this one by adding this one to its blockers",
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
    let waiting = holding_blockers(cx, req.parent.as_ref(), blockers, &req.agent)?;
    if !waiting.is_empty() {
        return Err(bad_transition(
            format!(
                "a claimed task starts in doing, but it would wait on {}",
                waiting.join(", ")
            ),
            format!(
                "add it without claim; it becomes ready when its blockers are done; \
                 {ALONGSIDE_RULE}"
            ),
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
/// end all follow the same stop, so the spell away starts at the first of them. Its second
/// column is the `unseen_since` of a session the sweeper ended ([`end_unseen`]), which
/// stopped then, not when it was ended.
///
/// [`end_unseen`]: crate::ops::agents::end_unseen
pub(crate) const AWAY_SINCE: &str = "SELECT at, json_extract(payload, '$.unseen_since') FROM events \
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
            let stop: Option<(i64, Option<String>)> = cx
                .db
                .prepare_cached(AWAY_SINCE)?
                .query_row(params![owner, last_run], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            match stop {
                Some((at, None)) => at,
                Some((at, Some(unseen))) => DateTime::parse_from_rfc3339(&unseen)
                    .map_err(|e| Error::Internal(format!("unseen_since {unseen}: {e}")))?
                    .timestamp_millis()
                    .min(at),
                None => *last_seen,
            }
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
pub(crate) fn check_not_before(cx: &Ctx<'_>, task: &Task) -> Result<()> {
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
    /// Only the direct children of this task.
    pub parent: Option<TaskId>,
    /// Only feedback items that take reports (true), or none of them (false).
    pub live: Option<bool>,
    /// Only done and dropped tasks (true), or none of them (false).
    pub closed: Option<bool>,
    /// A search for these words ([`search_words`]), best match first; none is no search.
    pub words: Vec<String>,
    /// Only tasks with an event after this point.
    pub since: Option<Since>,
    /// Only the tasks that follow this one in list order: the cursor of a cut list.
    pub after: Option<TaskId>,
    pub limit: usize,
}

/// The point a `since` filter starts after: an event sequence number or a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Since {
    Seq(i64),
    At(Timestamp),
}

/// Reads `since`: digits alone are an event sequence number, anything else an RFC 3339
/// time. A blank value is no filter, as a blank `q` is.
pub(crate) fn parse_since(since: Option<&str>) -> Result<Option<Since>> {
    let Some(since) = since.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let parsed = if since.bytes().all(|b| b.is_ascii_digit()) {
        since.parse().ok().map(Since::Seq)
    } else {
        chrono::DateTime::parse_from_rfc3339(since)
            .ok()
            .map(|at| Since::At(at.to_utc()))
    };
    parsed.map(Some).ok_or_else(|| {
        invalid(
            "since is neither an event sequence number nor an RFC 3339 time",
            "send since=<seq> (the snapshot's seq, or an event's) or a time such as \
             since=2026-01-02T03:04:05Z",
        )
    })
}

/// The words of a `q` search: split at white space. A blank `q` is no search, as on
/// `/v1/errors`.
pub(crate) fn search_words(q: Option<&str>) -> Result<Vec<String>> {
    let Some(q) = q.filter(|q| !q.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    check_line("q", q, TASKS_Q_CHARS_MAX)?;
    let words: Vec<String> = q.split_whitespace().map(str::to_owned).collect();
    if words.len() > TASKS_Q_WORDS_MAX {
        return Err(invalid(
            format!(
                "q has {} words; the limit is {TASKS_Q_WORDS_MAX}",
                words.len()
            ),
            "search for fewer, more specific words",
        ));
    }
    Ok(words)
}

/// The FTS5 query of search words: each word is a phrase (its parts in a row, such as
/// `mcp` and `rs` of `mcp.rs`) that may go on (`snap` finds `snapshot`), and the index
/// stems both sides. Every word must match, or with `any` at least one.
/// A task id is searched whole and in the id column too: `factory-12` does not find
/// `factory-120`. Other words skip the id column, where a board's slug would match each
/// of its tasks.
/// A word without a letter or digit holds nothing the index stores: it is left out, and
/// a search of such words alone is None, which finds nothing.
fn search_query(words: &[String], any: bool) -> Option<String> {
    let terms: Vec<String> = words
        .iter()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .map(|w| {
            if w.parse::<TaskId>().is_ok() {
                format!("\"{w}\"")
            } else {
                format!("{{title body notes}} : \"{}\"*", w.replace('"', "\"\""))
            }
        })
        .collect();
    (!terms.is_empty()).then(|| terms.join(if any { " OR " } else { " AND " }))
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
    let parent = filter.parent.as_ref().map(TaskId::as_str);
    if let Some(parent) = &parent {
        clauses.push("t.parent = :parent");
        params.push((":parent", parent));
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
    match filter.closed {
        Some(true) => clauses.push("t.status IN ('done', 'dropped')"),
        Some(false) => clauses.push("t.status NOT IN ('done', 'dropped')"),
        None => {}
    }
    // Feedback items are filed on the feedback board. On a board without any, `live=true`
    // can only be a mistake, so say where they are rather than answer with an empty list.
    if filter.live == Some(true)
        && let Some(board) = &filter.board
        && *board != cx.s.feedback_board
        && !cx.exists(
            "SELECT 1 FROM tasks t JOIN feedback f ON f.task = t.id WHERE t.board = ?1",
            [board.as_str()],
        )?
    {
        let feedback = &cx.s.feedback_board;
        return Err(invalid(
            format!(
                "board {board} holds no feedback items for live to list; they are on board {feedback}"
            ),
            format!("send board={feedback} with live=true, or leave board out"),
        ));
    }
    let live = filter.live.map(|want| {
        let not = if want { "" } else { "NOT " };
        format!("t.id {not}IN ({LIVE_ITEMS})")
    });
    let live_depth = DUPLICATE_DEPTH_MAX as i64;
    if let Some(live) = &live {
        clauses.push(live);
        params.push((":live_depth", &live_depth));
    }
    // A task changes with an event that names it (a note, a claim, an update, an ask). The
    // server's own notices (an update of no field) and the leases held for it change nothing
    // an agent wrote. The `events_task` index finds a task's events; the sequence form also
    // seeks in it, the time form reads that task's events.
    let since = match filter.since {
        Some(Since::Seq(seq)) => Some(("e.seq", seq)),
        Some(Since::At(at)) => Some(("e.at", at.timestamp_millis())),
        None => None,
    };
    let since_sql = since.map(|(column, _)| {
        format!(
            "EXISTS (SELECT 1 FROM events e WHERE e.task = t.id AND {column} > :since \
             AND e.type NOT IN ('lease_created', 'lease_released', 'lease_expired') \
             AND NOT (e.type = 'task_updated' \
                      AND json_array_length(e.payload, '$.fields') = 0))"
        )
    });
    if let (Some(sql), Some((_, value))) = (&since_sql, &since) {
        clauses.push(sql);
        params.push((":since", value));
    }
    let after = filter.after.as_ref().map(TaskId::as_str);
    if let Some(after) = &after
        && !cx.exists("SELECT 1 FROM tasks WHERE id = ?1", [after])?
    {
        return Err(not_found(
            format!("after names {after}, which is not a task"),
            "send after=<the id of the last task of the list you continue>",
        ));
    }
    if filter.words.is_empty() {
        // The cursor's place is read as the list is, so a page follows the task's place now.
        if let Some(after) = &after {
            clauses.push(AFTER_IN_LIST_ORDER);
            params.push((":after", after));
        }
        return load_tasks_up_to(cx, &clauses.join(" AND "), &params, Some(filter.limit));
    }

    // A search lists the best match first. No task with every word: list the tasks with
    // any of them, and mark each so the reader knows.
    let (Some(every), Some(any)) = (
        search_query(&filter.words, false),
        search_query(&filter.words, true),
    ) else {
        return Ok(Vec::new());
    };
    let found = |query: &str, rank: &str, after: Option<&str>, limit: usize| {
        found_page(cx, &clauses, &params, query, rank, after, limit)
    };
    let not_listed = || {
        invalid(
            format!(
                "after names {}, which this search does not list",
                after.unwrap_or_default()
            ),
            "send after=<the id of the last task of the page before> with the same q and \
             filters",
        )
    };
    let page = found(&every, SEARCH_RANK, after, filter.limit)?;
    // One word: both searches are the same.
    if every == any || page.as_ref().is_some_and(|tasks| !tasks.is_empty()) {
        return page.ok_or_else(not_listed);
    }
    // An empty page after a cursor is the end of a list that has tasks with every word.
    if after.is_some()
        && found(&every, SEARCH_RANK, None, 1)?.is_some_and(|tasks| !tasks.is_empty())
    {
        return page.ok_or_else(not_listed);
    }
    let mut some = found(&any, SEARCH_RANK_ANY, after, filter.limit)?.ok_or_else(not_listed)?;
    for task in &mut some {
        task.any_word = true;
    }
    Ok(some)
}

/// The cursor clause of a list in list order; expects `:after`.
const AFTER_IN_LIST_ORDER: &str = "(t.priority, t.rank IS NULL, COALESCE(t.rank, 0), t.number, t.board) > \
     (SELECT c.priority, c.rank IS NULL, COALESCE(c.rank, 0), c.number, c.board \
      FROM tasks c WHERE c.id = :after)";

/// One page of a search with the FTS5 query `query`, after the task `after` when given.
/// None when the search does not list that task: its place is its rank by `rank_sql`.
fn found_page(
    cx: &Ctx<'_>,
    clauses: &[&str],
    params: &NamedParams<'_>,
    query: &str,
    rank_sql: &str,
    after: Option<&str>,
    limit: usize,
) -> Result<Option<Vec<Task>>> {
    let mut clauses = clauses.to_vec();
    let mut params = params.clone();
    params.push((":match", &query));
    let rank: Option<f64> = match after {
        Some(after) => {
            let rank = cx
                .db
                .prepare_cached(&format!(
                    "SELECT {rank_sql} FROM task_search WHERE task_search MATCH ?1 \
                     AND rowid = (SELECT doc FROM task_search_docs WHERE task = ?2)"
                ))?
                .query_row(params![query, after], |r| r.get(0))
                .optional()?;
            if rank.is_none() {
                return Ok(None);
            }
            rank
        }
        None => None,
    };
    let cursor = format!("({rank_sql} > :rank OR ({rank_sql} = :rank AND {AFTER_IN_LIST_ORDER}))");
    if let (Some(rank), Some(after)) = (&rank, &after) {
        clauses.push(&cursor);
        params.push((":rank", rank));
        params.push((":after", after));
    }
    load_tasks_found(cx, rank_sql, &clauses.join(" AND "), &params, limit).map(Some)
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
                "finish {0} (task_done) or {1}, then claim {task}",
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

/// What a refusal for blockers adds: the blockers that do not hold a start.
const ALONGSIDE_RULE: &str = "a blocker in review that is an ancestor of the task or yours \
                              does not hold it, so a task done in that blocker's PR is claimed \
                              and moved to review with a note naming the PR";

/// Refuses `agent`'s start of `task` while a blocker holds it ([`holding_blockers`]).
pub(crate) fn check_blockers_done(cx: &Ctx<'_>, task: &Task, agent: &AgentId) -> Result<()> {
    let waiting = holding_blockers(cx, task.parent.as_ref(), &task.blockers, agent)?;
    if waiting.is_empty() {
        return Ok(());
    }
    Err(bad_transition(
        format!("{} waits on {}", task.id, waiting.join(", ")),
        format!(
            "pick a ready task (task_list ready=true), or help finish its blockers; \
             {ALONGSIDE_RULE}"
        ),
    ))
}

/// The tasks in `blockers` that hold a start of a task under `parent` by `agent`, each as
/// `id (status)`: those not done, except one in `review` that is an ancestor of the task
/// (`parent` or above) or that `agent` owns. The task's work then goes out in that
/// blocker's PR, so it moves to `review` alongside it. It stays out of `ready` all the
/// same, so no coordinator starts it.
fn holding_blockers(
    cx: &Ctx<'_>,
    parent: Option<&TaskId>,
    blockers: &[TaskId],
    agent: &AgentId,
) -> Result<Vec<String>> {
    // Read only once a blocker in review that is not the agent's needs it.
    let mut ancestors: Option<HashSet<String>> = None;
    let mut statement = cx
        .db
        .prepare_cached("SELECT status, owner FROM tasks WHERE id = ?1 AND status != 'done'")?;
    let mut waiting = Vec::new();
    for b in blockers {
        let row: Option<(String, Option<String>)> = statement
            .query_row([b.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((status, owner)) = row else {
            continue;
        };
        if status == status_str(TaskStatus::Review) {
            if owner.as_deref() == Some(agent.as_str()) {
                continue;
            }
            let ancestors = match &mut ancestors {
                Some(set) => set,
                None => ancestors.insert(match parent {
                    Some(parent) => cx
                        .db
                        .prepare_cached(
                            "WITH RECURSIVE up(id) AS (SELECT ?1 UNION SELECT t.parent \
                             FROM tasks t JOIN up ON t.id = up.id WHERE t.parent IS NOT NULL) \
                             SELECT id FROM up",
                        )?
                        .query_map([parent.as_str()], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?,
                    None => HashSet::new(),
                }),
            };
            if ancestors.contains(b.as_str()) {
                continue;
            }
        }
        waiting.push(format!("{b} ({status})"));
    }
    Ok(waiting)
}

/// Claims an open task: one conditional update, so concurrent claims have one winner. A
/// parked task (review or blocked) that is the caller's or nobody's resumes instead.
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
        // A stand-in is told its step without counting as a refused claim.
        if acts_for_ended_owner(cx, &task, agent)? {
            return Err(bad_transition(
                format!(
                    "{id} is {} and its owner {}'s session has ended; claim does not take it",
                    status_str(task.status),
                    owner_desc(cx, owner)?
                ),
                format!(
                    "you created {id} or delegated its owner: take it with {}, or move or \
                     close it as its owner would",
                    take_hint(id)
                ),
            ));
        }
        return Err(refused(
            format!("claim refused: {} owns {id}", owner_desc(cx, owner)?),
            if left_unrun(cx, &task, owner, agent)? {
                format!(
                    "pick another task from ready (task_list ready=true); {}",
                    left_owner_hint(id)
                )
            } else {
                "pick another task from ready (task_list ready=true)".to_owned()
            },
        ));
    }
    if matches!(task.status, TaskStatus::Review | TaskStatus::Blocked)
        && (owned_by_me || task.owner.is_none())
    {
        return resume(cx, &task, agent);
    }
    // Left: a closed task, or one in progress whose owner is stale.
    if task.status.is_closed() {
        return Err(bad_transition(
            format!(
                "{id} is {}; closed tasks cannot be claimed",
                status_str(task.status)
            ),
            format!("reopen it first (task_update {id} status=open) if it needs more work"),
        ));
    }
    if task.status != TaskStatus::Open {
        return Err(bad_transition(
            format!(
                "{id} is {} and its owner is stale; claim does not take it",
                status_str(task.status)
            ),
            format!("take it with {}", take_hint(id)),
        ));
    }
    if task.proposal {
        return Err(unaccepted_proposal(id));
    }
    check_blockers_done(cx, &task, agent)?;
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

/// The oldest open ask on `task`, if any.
fn open_ask(cx: &Ctx<'_>, task: &TaskId) -> Result<Option<i64>> {
    Ok(cx
        .db
        .query_row(
            "SELECT id FROM asks WHERE task = ?1 AND status = 'open' ORDER BY id LIMIT 1",
            [task.as_str()],
            |r| r.get(0),
        )
        .optional()?)
}

/// Who last set or changed `task`'s `blocked_reason`: the actor of the latest update that
/// lists the field. `None` when the log no longer holds that event.
fn reason_set_by(cx: &Ctx<'_>, task: &TaskId) -> Result<Option<String>> {
    Ok(cx
        .db
        .query_row(
            "SELECT e.actor FROM events e WHERE e.task = ?1 AND e.type = 'task_updated' \
             AND EXISTS (SELECT 1 FROM json_each(e.payload, '$.fields') f \
                         WHERE f.value = 'blocked_reason') \
             ORDER BY e.seq DESC LIMIT 1",
            [task.as_str()],
            |r| r.get(0),
        )
        .optional()?)
}

/// How the agent that asked `ask` withdraws it, through MCP or the CLI.
fn withdraw_hint(ask: i64) -> String {
    format!(
        "withdraw it if you asked it and it no longer applies (ask_withdraw {ask}; CLI: board \
         withdraw {ask})"
    )
}

/// Resumes a parked task (review or blocked) that is the caller's or nobody's, for
/// `claim`: a claim's checks, then `update` with `status: doing`, so the change and its
/// event are the same as `task_update status=doing` and stats do not count it as a claim
/// from ready. An open ask on the task refuses it: it waits on the user. So does a
/// `blocked_reason` that someone else set, which may name such a wait. The reason the
/// caller set on its own task is its own to judge, so its claim clears it, as
/// `status: doing` does.
fn resume(cx: &mut Ctx<'_>, task: &Task, agent: &AgentId) -> Result<Task> {
    let id = &task.id;
    let owned = task.owner.as_ref() == Some(agent);
    // Stored only on blocked tasks and only when not blank.
    if let Some(reason) = &task.blocked_reason
        && !(owned && reason_set_by(cx, id)?.as_deref() == Some(agent.as_str()))
    {
        return Err(bad_transition(
            format!("{id} is blocked for \"{reason}\", so claiming does not resume it"),
            if owned {
                format!(
                    "once \"{reason}\" no longer holds, resume it ({}), which clears the \
                     reason, or close it with task_done {id} if its work is finished",
                    resume_hint(id)
                )
            } else {
                format!(
                    "once \"{reason}\" no longer holds, clear the reason with task_update {id} \
                     status=open, then task_claim {id}"
                )
            },
        ));
    }
    if let Some(ask) = open_ask(cx, id)? {
        return Err(bad_transition(
            format!("{id} waits on ask #{ask}, so claiming does not resume it"),
            format!(
                "wait for the user's answer to ask #{ask}, or {}; then claim it again",
                withdraw_hint(ask)
            ),
        ));
    }
    let waiting = holding_blockers(cx, task.parent.as_ref(), &task.blockers, agent)?;
    if !waiting.is_empty() {
        return Err(bad_transition(
            format!("{id} waits on {}", waiting.join(", ")),
            format!(
                "help finish them, or remove the ones it no longer waits on (task_update {id} \
                 with blockers), then claim it again; {ALONGSIDE_RULE}"
            ),
        ));
    }
    check_not_before(cx, task)?;
    update(
        cx,
        id,
        UpdateTask {
            agent: agent.clone(),
            version: task.version,
            status: Some(TaskStatus::Doing),
            title: None,
            body: None,
            acceptance: None,
            priority: None,
            blockers: None,
            blocked_reason: None,
            auto: None,
            not_before: None,
            rank: None,
            owner: None,
            reason: None,
        },
        false,
    )
}

/// Records a refused claim for the stats (the refused transaction itself rolled back).
pub(crate) fn record_refusal(cx: &mut Ctx<'_>, id: &TaskId, agent: &AgentId) -> Result<()> {
    cx.db.execute(
        "INSERT INTO claim_refusals (task, agent, at) VALUES (?1, ?2, ?3)",
        params![id.as_str(), agent.as_str(), cx.now_ms()],
    )?;
    Ok(())
}

/// Takes a task whose owner is stale, or whose owner's session ended when the caller
/// stands in for it ([`acts_for_ended_owner`]).
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
    let stand_in = !task.owner_stale;
    if stand_in && !acts_for_ended_owner(cx, &task, agent)? {
        let hours = cx.s.owner_stale_after_secs / 3600;
        return Err(refused(
            format!(
                "take refused: {} owns {id}; owners count as stale after {hours}h unseen",
                owner_desc(cx, &owner)?
            ),
            // A take starts work, so it waits even when the owner has left.
            if left_unrun(cx, &task, &owner, agent)? {
                format!(
                    "{}; to work on it yourself, take it once the owner is stale",
                    left_owner_hint(id)
                )
            } else {
                format!("pick another task from ready (task_list ready=true); {STAND_IN_RULE}")
            },
        ));
    }
    let status = match task.status {
        TaskStatus::Blocked => TaskStatus::Blocked,
        _ => TaskStatus::Doing,
    };
    if task.status == TaskStatus::Open {
        check_blockers_done(cx, &task, agent)?;
        check_not_before(cx, &task)?;
    }
    if status == TaskStatus::Doing {
        // A stale owner's review task whose work is finished needs no take, and no room.
        check_doing_limit(cx, agent, id).map_err(|e| match e {
            Error::Api(mut e) if task.status == TaskStatus::Review && task.owner_stale => {
                e.next = Some(format!(
                    "{}; or, if its work is finished, close it without taking it: {}",
                    e.next.unwrap_or_default(),
                    done_take_hint(id)
                ));
                Error::Api(e)
            }
            e => e,
        })?;
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
        if stand_in {
            serde_json::json!({ "from": owner, "status": status, "stand_in": true })
        } else {
            serde_json::json!({ "from": owner, "status": status })
        },
    )?;
    get_task(cx, id)
}

/// The query of [`acts_for_ended_owner`]: whether `?3` delegated `?1`, directly or through
/// its own children, within depth `?2`.
pub(crate) const DELEGATED_BY: &str = concat!(
    "WITH RECURSIVE ",
    crate::ops::leases::delegated_up_cte!(),
    " SELECT EXISTS (SELECT 1 FROM up WHERE depth > 0 AND id = ?3)"
);

/// Whether `owner`'s session has ended with nothing of it left to come back: its state is
/// `ended`, with no usage-limit reset ahead, and no thread it delegated (at any depth) is
/// at work, as [`DELEGATE_AT_WORK`] judges for the idle sweep.
pub(crate) fn owner_session_ended(cx: &Ctx<'_>, owner: &AgentId) -> Result<bool> {
    let now = cx.now_ms();
    let ended = cx
        .db
        .query_row(
            "SELECT state = 'ended' AND (paused_until IS NULL OR paused_until <= ?2) \
             FROM agents WHERE id = ?1",
            params![owner.as_str(), now],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false);
    Ok(ended && !delegate_at_work(cx, owner)?)
}

/// Whether a thread that `owner` delegated (at any depth) is at work, as
/// [`DELEGATE_AT_WORK`] judges for the idle sweep.
pub(crate) fn delegate_at_work(cx: &Ctx<'_>, owner: &AgentId) -> Result<bool> {
    let now = cx.now_ms();
    Ok(cx.db.prepare_cached(DELEGATE_AT_WORK)?.query_row(
        params![
            owner.as_str(),
            crate::ops::leases::DELEGATION_DEPTH,
            now - cx.s.doing_idle_ms(),
            now
        ],
        |r| r.get(0),
    )?)
}

/// Whether `agent` stands in for `task`'s owner (`docs/API.md`, "Tasks"): the owner's
/// session has ended ([`owner_session_ended`]), and `agent` created the task (unless it is
/// a feedback item, whose creator is only its reporter) or delegated the owner (a T3 thread
/// whose `t3_parent` chain leads from the owner to `agent`). Such an agent may take the
/// task at once and move, hand over, release or close it as its owner may, so a parent
/// can finish what its finished child still owns.
fn acts_for_ended_owner(cx: &Ctx<'_>, task: &Task, agent: &AgentId) -> Result<bool> {
    let Some(owner) = task.owner.as_ref().filter(|o| *o != agent) else {
        return Ok(false);
    };
    let created = task.created_by == *agent && task.feedback.is_none();
    let delegated = created
        || cx.db.prepare_cached(DELEGATED_BY)?.query_row(
            params![
                owner.as_str(),
                crate::ops::leases::DELEGATION_DEPTH,
                agent.as_str()
            ],
            |r| r.get(0),
        )?;
    Ok(delegated && owner_session_ended(cx, owner)?)
}

/// What the task's creator or the agent that delegated its owner may do once the owner's
/// session ends, for refusals of other callers.
const STAND_IN_RULE: &str = "the task's creator and the agents that \
     delegated its owner may take, move or close it once the owner's session has ended";

/// Why `owner` still holds its task against a caller that does not stand in for it, for
/// refusals: the state of its session, and what of it is left to come back.
fn owner_session_desc(cx: &Ctx<'_>, owner: &AgentId) -> Result<String> {
    let now = cx.now_ms();
    let Some((state, paused_until)) = cx
        .db
        .query_row(
            "SELECT state, paused_until FROM agents WHERE id = ?1",
            [owner.as_str()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
        )
        .optional()?
    else {
        return Ok("it is not registered".to_owned());
    };
    if state != "ended" {
        return Ok(format!("its session is {state}"));
    }
    Ok(match paused_until.filter(|&p| p > now) {
        Some(until) => format!(
            "its session ended while a usage limit holds it, and it comes back after the reset \
             at {}",
            ts(until).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        None if delegate_at_work(cx, owner)? => {
            "its session ended, but a thread it delegated is still at work".to_owned()
        }
        None => "its session has ended".to_owned(),
    })
}

/// The owner of `task`, or an agent standing in for it ([`acts_for_ended_owner`]), may go
/// on; anyone else is refused, with the step that fits the caller.
fn require_owner(cx: &Ctx<'_>, task: &Task, agent: &AgentId, action: &str) -> Result<()> {
    let id = &task.id;
    match &task.owner {
        Some(owner) if owner == agent => Ok(()),
        Some(_) if acts_for_ended_owner(cx, task, agent)? => Ok(()),
        Some(owner) => {
            let ended = owner_session_ended(cx, owner)?;
            let message = if ended {
                format!(
                    "{owner} owns {id} and its session has ended, but you do not stand in \
                     for it, so you may not {action} it"
                )
            } else {
                format!(
                    "only the owner {owner} may {action} {id}: {}",
                    owner_session_desc(cx, owner)?
                )
            };
            let left = left_unrun(cx, task, owner, agent)?;
            let next = if action == "complete"
                && (left || (task.status == TaskStatus::Review && task.owner_stale))
            {
                format!(
                    "the owner is stale or has left: close it in one step with {}",
                    done_take_hint(id)
                )
            } else if task.owner_stale {
                let take = format!("the owner is stale: take it first with {}", take_hint(id));
                if left {
                    format!("{take}; also, {}", left_owner_hint(id))
                } else {
                    take
                }
            } else if left {
                left_owner_hint(id)
            } else if ended && crate::ops::judges::has_coordinator_rights(cx, &task.board, agent)? {
                format!(
                    "the owner's session has ended; with the board's coordinator rights, hand \
                     it to yourself first (task_update {id} owner={agent}), then {action} it"
                )
            } else {
                let hours = cx.s.owner_stale_after_secs / 3600;
                format!(
                    "ask the owner in a note (task_note {id}); {STAND_IN_RULE}, and anyone may \
                     take it once the owner is stale ({hours}h unseen)"
                )
            };
            Err(forbidden(message, next))
        }
        None => Err(forbidden(
            format!("nobody owns {id}, so it cannot be {action}d by you"),
            format!("claim it first with task_claim {id}"),
        )),
    }
}

/// Who may change the status of `task` (`docs/API.md`, "Tasks"): anyone while it has no
/// owner or is closed; otherwise its owner, an agent standing in for an owner whose session
/// ended ([`acts_for_ended_owner`]), the board's coordinator (or a live judge of the board)
/// or the user. Others take a stale owner's task first, which is logged.
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
    if crate::ops::judges::has_coordinator_rights(cx, &task.board, agent)? {
        return Ok(());
    }
    require_owner(cx, task, agent, action)
}

/// Who may hand `task` to `target` (`docs/API.md`, "Tasks"): its owner, an agent standing in
/// for an owner whose session ended ([`acts_for_ended_owner`]), the board's coordinator (or a
/// live judge of the board) or the user. `owned` is whether the task has an owner once the
/// request's status change applies, and `to` that status. The target must be a live agent.
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
        || crate::ops::judges::has_coordinator_rights(cx, &task.board, agent)?
        || acts_for_ended_owner(cx, task, agent)?;
    if !allowed {
        return Err(match &task.owner {
            Some(owner) => forbidden(
                format!(
                    "only the owner {owner}, the board's coordinator or the user may hand \
                     {id} over, and a stand-in once the owner's session has ended: {}",
                    owner_session_desc(cx, owner)?
                ),
                if task.owner_stale {
                    format!(
                        "the owner is stale: take it first ({}), then hand it over",
                        take_hint(id)
                    )
                } else {
                    format!("ask the owner in a note (task_note {id}); {STAND_IN_RULE}")
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
                        "to work on it yourself, resume it ({}) without owner",
                        resume_hint(id)
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
            format!(
                "resume it for that agent: {} owner={target}",
                resume_hint(id)
            ),
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

/// The refusal of an unaccepted proposal moving to doing. The hint keeps the claimant,
/// who may be the proposer, from accepting it itself.
pub(crate) fn unaccepted_proposal(id: &TaskId) -> Error {
    bad_transition(
        format!(
            "{id} is a proposal not yet accepted: it becomes work only once another agent accepts it"
        ),
        "pick other ready work (task_list ready=true); an agent that did not file it reviews it \
         with a `verdict:` decision note, and an accept sets auto",
    )
}

/// How to take a stale owner's task, through MCP or the CLI.
fn take_hint(id: &TaskId) -> String {
    format!("task_claim {id} take=true, or board take {id}")
}

/// How to close a stale owner's review task in one step, through MCP or the CLI.
pub(crate) fn done_take_hint(id: &TaskId) -> String {
    format!("task_done {id} take=true and a pr or a passed check, or board done {id} --take")
}

/// How to move a parked task back to doing.
fn resume_hint(id: &TaskId) -> String {
    format!("task_update {id} status=doing")
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
        require_owner(cx, &task, agent, "release")?;
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

/// Refuses a `PATCH` into `doing` for `owner` while `blockers` (the list the request leaves)
/// has one that holds it ([`holding_blockers`]) or `not_before` is still ahead, as a claim would.
fn check_ready_to_start(
    cx: &Ctx<'_>,
    task: &Task,
    blockers: &[TaskId],
    not_before: Option<Timestamp>,
    owner: &AgentId,
) -> Result<()> {
    let id = &task.id;
    let waiting = holding_blockers(cx, task.parent.as_ref(), blockers, owner)?;
    // Both waits at once, so one refusal names everything that holds the task.
    let mut waits: Vec<String> = Vec::new();
    let mut drop: Vec<&str> = Vec::new();
    if !waiting.is_empty() {
        waits.push(format!("on {}", waiting.join(", ")));
        drop.push("blockers=[the rest]");
    }
    if let Some(at) = not_before.filter(|&at| at > cx.now) {
        waits.push(format!("until {}", at.format("%Y-%m-%d %H:%MZ")));
        drop.push("not_before=none");
    }
    if waits.is_empty() {
        return Ok(());
    }
    Err(bad_transition(
        format!(
            "{id} waits {}, so it cannot move to doing",
            waits.join(" and ")
        ),
        format!(
            "wait until that ends, or drop what it no longer waits on in the same request \
             (task_update {id} status=doing {}); {ALONGSIDE_RULE}",
            drop.join(" ")
        ),
    ))
}

/// The tasks in `blockers` that are not done, each as `id (status)`.
fn undone_blockers(cx: &Ctx<'_>, blockers: &[TaskId]) -> Result<Vec<String>> {
    let mut statement = cx
        .db
        .prepare_cached("SELECT status FROM tasks WHERE id = ?1 AND status != 'done'")?;
    let mut waiting = Vec::new();
    for b in blockers {
        let status: Option<String> = statement.query_row([b.as_str()], |r| r.get(0)).optional()?;
        if let Some(status) = status {
            waiting.push(format!("{b} ({status})"));
        }
    }
    Ok(waiting)
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
    // The owner that left the task this drop is for, when the caller does not own it.
    let mut left_owner: Option<AgentId> = None;

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
        // On a board nobody runs, an agent on its repositories drops a task whose owner has
        // left ([`left_unrun`]), and says why.
        if let Err(refusal) = require_status_rights(cx, &task, agent, by_user, "move") {
            match task.owner.as_ref().filter(|_| to == TaskStatus::Dropped) {
                Some(owner) if left_unrun(cx, &task, owner, agent)? => {
                    if req.reason.is_none() {
                        return Err(invalid(
                            format!(
                                "dropping {id} for its owner {}, who has left, needs a reason",
                                handle_of(cx, owner.as_str())?
                            ),
                            format!("send why with the drop: {}", drop_hint(id)),
                        ));
                    }
                    left_owner = Some(owner.clone());
                }
                _ => return Err(refusal),
            }
        }
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
            // A task in review has an owner to finish it, who came through doing and so
            // past the proposal gate.
            TaskStatus::Review if owner.is_none() => {
                return Err(bad_transition(
                    format!("nobody owns {id}, so it cannot wait in review"),
                    format!(
                        "to work on it yourself, resume it ({}), then move it to review",
                        resume_hint(id)
                    ),
                ));
            }
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

    // Blank reasons were read as absent at the boundary; stored ones are never blank.
    let blocked_reason = if to == TaskStatus::Blocked {
        let reason = req
            .blocked_reason
            .clone()
            .or_else(|| task.blocked_reason.clone());
        if reason.is_none() && undone_blockers(cx, &blockers)?.is_empty() {
            return Err(invalid(
                format!("blocked needs a reason or a blocker that is not done ({id})"),
                "send blocked_reason, or blockers naming the tasks it waits on",
            ));
        }
        reason
    } else {
        if req.blocked_reason.is_some() {
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
    // A rank orders a task inside one priority level, so it does not follow the task to
    // another level unless the same request sets one there. A closed task holds none: it
    // waits for nothing, and a reopened one is placed anew.
    let rank = match req.rank {
        _ if to.is_closed() => None,
        Some(new) => new,
        None if priority != task.priority => None,
        None => task.rank,
    };
    if rank != task.rank {
        fields.push("rank");
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
    // Starting work waits as a claim does, on what the task names after this request: the
    // same request may remove the blockers or the not_before it no longer waits on. Checked
    // before the doing limit, as a claim does, so no task is parked for a refused move.
    if to == TaskStatus::Doing && from != TaskStatus::Doing {
        // A move to doing always leaves an owner; the caller is the fallback all the same.
        let starter = owner.as_ref().unwrap_or(agent);
        check_ready_to_start(cx, &task, &blockers, not_before, starter)?;
    }
    // A task entering doing, or changing hands in doing, counts against its owner's limit.
    if let Some(worker) = owner
        .as_ref()
        .filter(|_| to == TaskStatus::Doing && (from != to || owner != task.owner))
    {
        check_owner_doing_limit(cx, worker, agent, id)?;
    }
    // Validation let a reason through only with a drop. It is kept even when the task was
    // already dropped and nothing else changes, so a reason sent late is not lost.
    if let Some(reason) = req.reason {
        insert_note(cx, &task.board, id, agent, NoteKind::Decision, reason)?;
    }
    if fields.is_empty() {
        return Ok(task);
    }

    let now = cx.now_ms();
    // A rank alone places the task and changes nothing of it: its version and `updated_at`
    // stay, so no writer holding the task meets a conflict and no age restarts.
    let placed_only = fields == ["rank"];
    cx.db.execute(
        "UPDATE tasks SET status = ?2, owner = ?3, title = ?4, body = ?5, acceptance = ?6, \
         priority = ?7, auto = ?8, blocked_reason = ?9, version = version + ?13, \
         updated_at = CASE WHEN ?13 = 1 THEN ?10 ELSE updated_at END, \
         not_before_announced = CASE WHEN not_before IS ?11 THEN not_before_announced ELSE 0 END, \
         not_before = ?11, rank = ?12, \
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
            rank,
            i64::from(!placed_only),
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
    if let Some(left) = &left_owner {
        payload["left_owner"] = serde_json::json!(left);
    }
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
    if fields.contains(&"rank") {
        payload["rank"] = serde_json::json!(rank);
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

/// Most tasks one [`RankTasks`] call orders.
pub(crate) const RANK_TASKS_MAX: usize = 500;

pub(crate) fn validate_rank(req: &RankTasks) -> Result<()> {
    if req.tasks.len() > RANK_TASKS_MAX {
        return Err(invalid(
            format!("one call ranks at most {RANK_TASKS_MAX} tasks"),
            "send the first tasks of each priority level; the rest follow them by number",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(twice) = req.tasks.iter().find(|id| !seen.insert(*id)) {
        return Err(invalid(
            format!("{twice} is listed twice"),
            "list each task once, first to start first",
        ));
    }
    Ok(())
}

/// Orders tasks of `board` inside their priority levels: the listed tasks get the ranks
/// 1, 2, 3 and so on in the order sent, counted per level. With `clear_others`, every other
/// task of the board loses its rank. A rank places a task and changes nothing of it, so its
/// version and `updated_at` stay. A closed task takes no rank.
pub(crate) fn rank(
    cx: &mut Ctx<'_>,
    board: &BoardSlug,
    req: RankTasks,
    by_user: bool,
) -> Result<RankedTasks> {
    if by_user {
        note_agent_seen(cx, &req.agent)?;
    } else {
        touch_agent(cx, &req.agent)?;
    }
    get_board_uncounted(cx, board)?;
    let mut next = [0u32; 3];
    let mut changes: Vec<(TaskId, Option<u32>)> = Vec::new();
    let mut tasks = Vec::with_capacity(req.tasks.len());
    let mut closed = Vec::new();
    for id in &req.tasks {
        let mut task = get_task(cx, id)?;
        if &task.board != board {
            return Err(invalid(
                format!("{id} is on board {}, not {board}", task.board),
                format!("rank it with the tasks of {}", task.board),
            ));
        }
        if task.status.is_closed() {
            closed.push(id.to_string());
            continue;
        }
        let level = &mut next[task.priority as usize];
        *level += 1;
        if task.rank != Some(*level) {
            changes.push((id.clone(), Some(*level)));
            task.rank = Some(*level);
        }
        tasks.push(task);
    }
    if !closed.is_empty() {
        return Err(invalid(
            format!("done or dropped, so not ranked: {}", closed.join(", ")),
            "send the list again without them; a closed task holds no rank",
        ));
    }
    if req.clear_others {
        let listed: HashSet<&TaskId> = req.tasks.iter().collect();
        let mut stmt = cx
            .db
            .prepare_cached("SELECT id FROM tasks WHERE board = ?1 AND rank IS NOT NULL")?;
        let ranked = stmt
            .query_map([board.as_str()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for id in ranked {
            let id: TaskId = parse_id(&id)?;
            if !listed.contains(&id) {
                changes.push((id, None));
            }
        }
    }
    for (id, rank) in &changes {
        cx.db.execute(
            "UPDATE tasks SET rank = ?2 WHERE id = ?1",
            params![id.as_str(), rank],
        )?;
        cx.emit(
            Some(board),
            req.agent.as_str(),
            EventType::TaskUpdated,
            Some(id),
            serde_json::json!({ "fields": ["rank"], "rank": rank }),
        )?;
    }
    Ok(RankedTasks { tasks })
}

/// Whether nobody runs `board`: it is active with neither a coordinator (the server drops
/// one whose lease ran out) nor autopilot, whose dispatcher closes reviewed tasks itself.
pub(crate) fn nobody_runs(board: &Board) -> bool {
    board.status == BoardStatus::Active && board.coordinator.is_none() && !board.autopilot
}

/// How long unseen a session still marked `running` counts as having stopped without an
/// end hook (`owner_left_secs`): the sweeper marks it `ended` then
/// ([`crate::ops::agents::end_unseen`]), and an owner unseen that long has left its task
/// ([`owner_left`]) even before the sweep runs.
pub(crate) fn owner_left_ms(cx: &Ctx<'_>) -> i64 {
    cx.s.owner_left_ms()
}

/// Whether `owner` has left its task: its session ended with nothing of it left to come
/// back ([`owner_session_ended`]), or it stopped without an end hook. A running session
/// marks itself seen at each hook, so one that is still `running` and unseen for
/// [`owner_left_ms`], with no usage-limit reset ahead and no delegated thread at work,
/// has stopped. An idle session waits, on CI or on the user, and has not left.
pub(crate) fn owner_left(cx: &Ctx<'_>, owner: &AgentId) -> Result<bool> {
    if owner_session_ended(cx, owner)? {
        return Ok(true);
    }
    let now = cx.now_ms();
    let stopped = cx
        .db
        .query_row(
            "SELECT state = 'running' AND last_seen <= ?2 \
             AND (paused_until IS NULL OR paused_until <= ?3) FROM agents WHERE id = ?1",
            params![owner.as_str(), now - owner_left_ms(cx), now],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false);
    Ok(stopped && !delegate_at_work(cx, owner)?)
}

/// Whether `agent` may drop `task`, or close it in one step, for an `owner` that is not
/// stale: the task is not closed, nobody runs its board ([`nobody_runs`]), so no
/// coordinator finishes or drops a task for an owner that left ([`owner_left`]), and
/// `agent` works on one of the board's repositories, as the agents told of such a task do.
/// A take that starts work still waits until the owner is stale.
fn left_unrun(cx: &Ctx<'_>, task: &Task, owner: &AgentId, agent: &AgentId) -> Result<bool> {
    if task.status.is_closed() {
        return Ok(false);
    }
    let board = get_board_uncounted(cx, &task.board)?;
    if !nobody_runs(&board) {
        return Ok(false);
    }
    let on_repo = find_agent(cx, agent)?
        .and_then(|a| a.repo)
        .is_some_and(|r| board.repos.contains(&r));
    Ok(on_repo && owner_left(cx, owner)?)
}

/// The call that drops `id` with the reason it needs.
pub(crate) fn drop_hint(id: &TaskId) -> String {
    format!(
        "task_update {id} status=dropped reason=<why>, or board update {id} --status dropped \
         --reason <why>"
    )
}

/// What an agent may do with a left owner's task on a board nobody runs ([`left_unrun`]),
/// for refusals that name another step.
fn left_owner_hint(id: &TaskId) -> String {
    format!(
        "nobody runs this board and the owner has left, so you may drop {id} with a reason \
         ({}), or close it once its work has landed ({})",
        drop_hint(id),
        done_take_hint(id)
    )
}

/// `take` on done: the owner that `agent` takes `task` from as it closes it, or `None` when
/// `agent` needs no take (its own task, nobody's, or one it stands in for), which leaves
/// the caller to [`require_owner`]. A left owner's task on a board nobody runs
/// ([`left_unrun`]) is closed this way from `doing`, `review` or `blocked`; a stale owner's
/// only from `review`, whose work is finished. The take never moves the task to `doing`, so
/// the doing limit does not apply. A task still in progress whose owner is only stale is
/// taken with [`take`], which keeps the limit.
fn done_take(cx: &Ctx<'_>, task: &Task, agent: &AgentId) -> Result<Option<AgentId>> {
    let id = &task.id;
    let Some(owner) = task.owner.as_ref().filter(|o| *o != agent) else {
        return Ok(None);
    };
    // A stand-in closes the task as its owner may, whether or not the owner is stale too.
    if acts_for_ended_owner(cx, task, agent)? {
        return Ok(None);
    }
    if left_unrun(cx, task, owner, agent)? {
        return Ok(Some(owner.clone()));
    }
    if !task.owner_stale {
        let hours = cx.s.owner_stale_after_secs / 3600;
        let ask = format!("ask the owner in a note (task_note {id}); {STAND_IN_RULE}");
        return Err(refused(
            format!(
                "take refused: {} owns {id}; owners count as stale after {hours}h unseen",
                owner_desc(cx, owner)?
            ),
            if !task.status.is_closed() && nobody_runs(&get_board_uncounted(cx, &task.board)?) {
                format!(
                    "{ask}; nobody runs this board, so an agent on one of its repositories \
                     may drop it with a reason, or close it with take, once the owner's \
                     session has ended, or has stayed running for {} unseen",
                    ago(TimeDelta::milliseconds(owner_left_ms(cx)))
                )
            } else {
                ask
            },
        ));
    }
    if task.status != TaskStatus::Review {
        return Err(bad_transition(
            format!(
                "{id} is {}; take on done closes a stale owner's task only in review",
                status_str(task.status)
            ),
            if task.status.is_closed() {
                format!(
                    "it is closed already; reopen it (task_update {id} status=open) if it needs more work"
                )
            } else {
                format!(
                    "take it first ({}), then finish its work and mark it done",
                    take_hint(id)
                )
            },
        ));
    }
    Ok(Some(owner.clone()))
}

pub(crate) fn done(cx: &mut Ctx<'_>, id: &TaskId, req: DoneTask) -> Result<Task> {
    touch_agent(cx, &req.agent)?;
    let task = get_task(cx, id)?;
    if req.version != task.version {
        return Err(version_conflict(&task, req.version));
    }
    let taken_from = if req.take {
        done_take(cx, &task, &req.agent)?
    } else {
        None
    };
    if taken_from.is_none() {
        require_owner(cx, &task, &req.agent, "complete")?;
    } else if !req.evidence.iter().any(|e| match e.kind {
        EvidenceKind::Pr => true,
        EvidenceKind::Check => e.exit_code == Some(0),
        _ => false,
    }) {
        // The closer did not do the work, so it shows that the work landed.
        return Err(Error::api(
            ErrorCode::EvidenceRequired,
            format!("closing {id} for its owner needs a pr or a passed check as evidence"),
            "send the merged pull request, or a check you ran that exited with 0",
        ));
    }
    // The owner closes its blocked task directly: the wait often ends off the board, and
    // resuming it first would hit the doing limit. Done clears `blocked_reason`.
    if !matches!(
        task.status,
        TaskStatus::Doing | TaskStatus::Review | TaskStatus::Blocked
    ) {
        return Err(bad_transition(
            format!(
                "{id} is {}; only doing, review or blocked tasks can be done",
                status_str(task.status)
            ),
            if task.status.is_closed() {
                format!(
                    "it is closed already; reopen it (task_update {id} status=open) if it needs more work"
                )
            } else {
                format!("claim it first with task_claim {id}")
            },
        ));
    }
    // What a blocked task still names as its wait is settled before it closes, so no ask
    // stays open for the user and no waiting task becomes ready early.
    if task.status == TaskStatus::Blocked {
        if let Some(ask) = open_ask(cx, id)? {
            return Err(bad_transition(
                format!("{id} waits on ask #{ask}, so it cannot be done yet"),
                format!(
                    "wait for the user's answer to ask #{ask}, or {}; then mark it done",
                    withdraw_hint(ask)
                ),
            ));
        }
        let waiting = open_blockers(cx, id)?;
        if !waiting.is_empty() {
            let list: Vec<String> = waiting.iter().map(|(b, s)| format!("{b} ({s})")).collect();
            return Err(bad_transition(
                format!(
                    "{id} waits on {}, so it cannot be done yet",
                    list.join(", ")
                ),
                format!(
                    "help finish them, or remove the ones it no longer waits on (task_update {id} \
                     with blockers), then mark it done"
                ),
            ));
        }
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
    if let Some(from) = &taken_from {
        join_board(cx, &task.board, &req.agent)?;
        cx.emit(
            Some(&task.board),
            req.agent.as_str(),
            EventType::TaskTaken,
            Some(id),
            serde_json::json!({ "from": from, "status": task.status }),
        )?;
    }
    let new_owner = if taken_from.is_some() {
        Some(&req.agent)
    } else {
        task.owner.as_ref()
    };
    cx.db.execute(
        "UPDATE tasks SET status = 'done', done_at = ?2, blocked_reason = NULL, summary = ?3, \
         rank = NULL, owner = ?4, version = version + 1, updated_at = ?2 WHERE id = ?1",
        params![
            id.as_str(),
            now,
            req.summary,
            new_owner.map(AgentId::as_str)
        ],
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
    insert_note(cx, &task.board, id, &req.agent, req.kind, req.text)
}

/// Stores a note on the task and records its `note_added` event: the one path for notes
/// an agent adds and for the reason a drop keeps.
fn insert_note(
    cx: &mut Ctx<'_>,
    board: &BoardSlug,
    id: &TaskId,
    agent: &AgentId,
    kind: NoteKind,
    text: String,
) -> Result<Note> {
    cx.db.execute(
        "INSERT INTO notes (task, agent, kind, text, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id.as_str(),
            agent.as_str(),
            enum_str(&kind),
            text,
            cx.now_ms()
        ],
    )?;
    let note_id = cx.db.last_insert_rowid();
    cx.emit(
        Some(board),
        agent.as_str(),
        EventType::NoteAdded,
        Some(id),
        serde_json::json!({ "note": note_id, "kind": kind }),
    )?;
    Ok(Note {
        id: note_id,
        task: id.clone(),
        agent: agent.clone(),
        kind,
        text,
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
