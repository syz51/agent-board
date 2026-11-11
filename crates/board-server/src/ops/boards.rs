use board_proto::{
    AgentId, Board, BoardSlug, BoardStatus, ClaimCoordinator, CreateBoard, DeviceName, Dispatch,
    EventType, TaskId, UpdateBoard,
};
use rusqlite::{OptionalExtension, params};
use std::collections::BTreeMap;

use crate::config::Settings;
use crate::db::{Ctx, enum_str, live_sql, parse_id};
use crate::error::{Result, bad_transition, forbidden, invalid, refused};
use crate::ops::{expires, join_board, touch_agent};
use crate::store::{agent_missing, board_missing, get_board};
use crate::text::{ago, check_line_or, check_text, normalize_repo};
use board_proto::{BOARD_TITLE_LONGER_TEXT_GOES, PARALLEL_TASKS_MAX};

fn check_repos(repos: Vec<String>) -> Result<Vec<String>> {
    if repos.len() > 20 {
        return Err(invalid(
            "a board covers at most 20 repos",
            "split the work into several boards",
        ));
    }
    let mut out = Vec::new();
    for repo in repos {
        let key = normalize_repo(&repo)?;
        if !out.contains(&key) {
            out.push(key);
        }
    }
    Ok(out)
}

fn check_devices(devices: Vec<DeviceName>) -> Result<Vec<DeviceName>> {
    if devices.len() > 10 {
        return Err(invalid(
            "at most 10 home devices",
            "list the devices that should host the coordinator, in order of preference",
        ));
    }
    let mut out: Vec<DeviceName> = Vec::new();
    for device in devices {
        if !out.contains(&device) {
            out.push(device);
        }
    }
    Ok(out)
}

pub(crate) fn validate_create(mut req: CreateBoard, s: &Settings) -> Result<CreateBoard> {
    check_line_or(
        "title",
        &req.title,
        s.title_chars,
        Some(BOARD_TITLE_LONGER_TEXT_GOES),
    )?;
    check_text("goal", &req.goal, s.body_chars)?;
    req.repos = check_repos(req.repos)?;
    req.home_devices = check_devices(req.home_devices)?;
    Ok(req)
}

pub(crate) fn validate_update(mut req: UpdateBoard, s: &Settings) -> Result<UpdateBoard> {
    if let Some(title) = &req.title {
        check_line_or(
            "title",
            title,
            s.title_chars,
            Some(BOARD_TITLE_LONGER_TEXT_GOES),
        )?;
    }
    if let Some(goal) = &req.goal {
        check_text("goal", goal, s.body_chars)?;
    }
    req.repos = req.repos.map(check_repos).transpose()?;
    req.home_devices = req.home_devices.map(check_devices).transpose()?;
    if let Some(n) = req.parallel_tasks
        && !(1..=PARALLEL_TASKS_MAX).contains(&n)
    {
        return Err(invalid(
            format!("parallel_tasks={n} is out of range"),
            format!("send parallel_tasks between 1 and {PARALLEL_TASKS_MAX}"),
        ));
    }
    if let Some(n) = req.usage_stop_at
        && !(1..=100).contains(&n)
    {
        return Err(invalid(
            format!("usage_stop_at={n} is out of range"),
            "send usage_stop_at between 1 and 100 (percent of a usage window)",
        ));
    }
    Ok(req)
}

pub(crate) fn create(cx: &mut Ctx<'_>, actor: &str, req: CreateBoard) -> Result<Board> {
    if cx.exists("SELECT 1 FROM boards WHERE slug = ?1", [req.slug.as_str()])? {
        return Err(invalid(
            format!("board {} already exists", req.slug),
            "use the existing board, or pick another slug",
        ));
    }
    let now = cx.now_ms();
    cx.db.execute(
        // A slug that was deleted before resumes its task numbering.
        "INSERT INTO boards (slug, title, goal, status, repos, autopilot, home_devices, \
         temporary, next_number, created_at, updated_at) VALUES (?1, ?2, ?3, 'active', ?4, ?5, \
         ?6, ?7, COALESCE((SELECT next_number FROM deleted_boards WHERE slug = ?1), 1), ?8, ?8)",
        params![
            req.slug.as_str(),
            req.title,
            req.goal,
            serde_json::to_string(&req.repos).expect("strings serialize"),
            req.autopilot,
            serde_json::to_string(&req.home_devices).expect("strings serialize"),
            req.temporary,
            now,
        ],
    )?;
    cx.emit(
        Some(&req.slug),
        actor,
        EventType::BoardCreated,
        None,
        serde_json::json!({
            "title": req.title,
            "repos": req.repos,
            "autopilot": req.autopilot,
            "temporary": req.temporary,
        }),
    )?;
    get_board(cx, &req.slug)
}

/// Changes a board. Only the user may make a board temporary, since the sweep then
/// deletes it once idle.
pub(crate) fn update(
    cx: &mut Ctx<'_>,
    actor: &str,
    slug: &BoardSlug,
    mut req: UpdateBoard,
    by_user: bool,
) -> Result<Board> {
    let before = get_board(cx, slug)?;
    // The user keeps the feedback board on whatever other boards do, so triage never stops.
    // A pause of it is dropped, not refused, so an older client's `board pause` of every
    // board goes on to the rest. Closing it, to retire or delete it, is a separate act.
    if *slug == cx.s.feedback_board && req.status == Some(BoardStatus::Paused) {
        req.status = None;
    }
    if req.temporary == Some(true) && !before.temporary && !by_user {
        return Err(forbidden(
            format!("only the user may make board {slug} temporary"),
            "temporary boards are deleted once idle; ask the user (ask_user), or create a \
             new board with temporary set",
        ));
    }
    if let Some(n) = req.parallel_tasks
        && n > before.parallel_tasks
        && !by_user
    {
        return Err(forbidden(
            format!("only the user may raise parallel_tasks on board {slug}"),
            "more tasks at once spend the shared usage limit faster; ask the user (ask_user) \
             to raise it",
        ));
    }
    if let Some(n) = req.usage_stop_at
        && n > before.usage_stop_at
        && !by_user
    {
        return Err(forbidden(
            format!("only the user may raise usage_stop_at on board {slug}"),
            "a higher stop lets the board spend more of the shared usage limit; ask the user \
             (ask_user) to raise it",
        ));
    }
    let mut fields = Vec::new();
    let mut set = |field: &'static str, changed: bool| {
        if changed {
            fields.push(field);
        }
    };
    set(
        "title",
        req.title.as_ref().is_some_and(|v| *v != before.title),
    );
    set("goal", req.goal.as_ref().is_some_and(|v| *v != before.goal));
    set("status", req.status.is_some_and(|v| v != before.status));
    set(
        "repos",
        req.repos.as_ref().is_some_and(|v| *v != before.repos),
    );
    set(
        "autopilot",
        req.autopilot.is_some_and(|v| v != before.autopilot),
    );
    set(
        "home_devices",
        req.home_devices
            .as_ref()
            .is_some_and(|v| *v != before.home_devices),
    );
    set(
        "temporary",
        req.temporary.is_some_and(|v| v != before.temporary),
    );
    set(
        "parallel_tasks",
        req.parallel_tasks
            .is_some_and(|v| v != before.parallel_tasks),
    );
    set(
        "dispatch",
        req.dispatch.is_some_and(|v| v != before.dispatch),
    );
    set(
        "usage_stop_at",
        req.usage_stop_at.is_some_and(|v| v != before.usage_stop_at),
    );
    if fields.is_empty() {
        return Ok(before);
    }
    // A status change names the new status, so a daemon can wake a resumed board's
    // coordinator without reading the board.
    let mut payload = serde_json::json!({ "fields": fields });
    if let Some(status) = req.status.filter(|&s| s != before.status) {
        payload["status"] = serde_json::Value::from(enum_str(&status));
    }
    // A board that stops running its coordinator thread holds that thread from now on.
    let stands_down =
        if req.status == Some(BoardStatus::Closed) && before.status != BoardStatus::Closed {
            Some("the board is closed")
        } else if req.dispatch == Some(Dispatch::Daemon) && before.dispatch != Dispatch::Daemon {
            Some("the board is set to daemon dispatch")
        } else if req.autopilot == Some(false) && before.autopilot {
            Some("autopilot was switched off")
        } else if let Some(homes) = &req.home_devices
            && last_coordinator(cx, slug)?
                .and_then(|(_, device)| device)
                .is_some_and(|device| !homes.iter().any(|home| home.as_str() == device))
        {
            Some("its device is no longer one of the board's home devices")
        } else {
            None
        };
    if let Some(reason) = stands_down {
        stand_down_last(cx, slug, reason)?;
    }
    // `paused_at` keeps the time of the pause while the board stays paused; the hooks halt
    // only turns that began before it.
    cx.db.execute(
        "UPDATE boards SET title = ?2, goal = ?3, status = ?4, repos = ?5, autopilot = ?6, \
         home_devices = ?7, updated_at = ?8, temporary = ?9, parallel_tasks = ?10, \
         dispatch = ?11, usage_stop_at = ?12, \
         paused_at = CASE WHEN ?4 != 'paused' THEN NULL \
                          WHEN status = 'paused' THEN paused_at ELSE ?8 END \
         WHERE slug = ?1",
        params![
            slug.as_str(),
            req.title.unwrap_or(before.title),
            req.goal.unwrap_or(before.goal),
            enum_str(&req.status.unwrap_or(before.status)),
            serde_json::to_string(&req.repos.unwrap_or(before.repos)).expect("serializes"),
            req.autopilot.unwrap_or(before.autopilot),
            serde_json::to_string(&req.home_devices.unwrap_or(before.home_devices))
                .expect("serializes"),
            cx.now_ms(),
            req.temporary.unwrap_or(before.temporary),
            req.parallel_tasks.unwrap_or(before.parallel_tasks),
            enum_str(&req.dispatch.unwrap_or(before.dispatch)),
            req.usage_stop_at.unwrap_or(before.usage_stop_at),
        ],
    )?;
    cx.emit(Some(slug), actor, EventType::BoardUpdated, None, payload)?;
    get_board(cx, slug)
}

/// Deletes a closed board without open asks, with its tasks, notes, evidence, asks,
/// members and events. Records one `board_updated` event (`fields: ["deleted"]`), an
/// existing type, so clients that predate deletion still read the feed. Tasks on other
/// boards that waited on one of its tasks lose that blocker, each with a `task_updated`
/// event on its own board, so a task that becomes ready is announced. The slug keeps its
/// task numbering for a board created later under it.
pub(crate) fn delete(cx: &mut Ctx<'_>, actor: &str, slug: &BoardSlug) -> Result<()> {
    let board = get_board(cx, slug)?;
    if board.status != BoardStatus::Closed {
        return Err(bad_transition(
            format!("board {slug} is {}", enum_str(&board.status)),
            format!(
                "only closed boards may be deleted; close it first (board board update {slug} \
                 --status closed)"
            ),
        ));
    }
    if board.open_asks > 0 {
        return Err(bad_transition(
            format!("board {slug} has {} open asks", board.open_asks),
            format!("answer them first (board asks --board {slug}), then delete the board"),
        ));
    }
    const TASKS: &str = "SELECT id FROM tasks WHERE board = ?1";
    let s = slug.as_str();
    // Unfinished tasks elsewhere that wait on this board's tasks, with the blockers they
    // lose. Finished ones lose the link too, unannounced: nothing waits on it any more.
    let mut dependents: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for row in cx
        .db
        .prepare_cached(
            "SELECT t.id, t.board, b.blocker FROM task_blockers b JOIN tasks t ON t.id = b.task \
             JOIN tasks bt ON bt.id = b.blocker \
             WHERE bt.board = ?1 AND t.board != ?1 AND t.status NOT IN ('done', 'dropped') \
             ORDER BY t.id, bt.number",
        )?
        .query_map([s], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
    {
        let (task, board, blocker) = row?;
        dependents
            .entry(task)
            .or_insert_with(|| (board, Vec::new()))
            .1
            .push(blocker);
    }
    for sql in [
        format!("DELETE FROM task_blockers WHERE task IN ({TASKS}) OR blocker IN ({TASKS})"),
        format!("DELETE FROM notes WHERE task IN ({TASKS})"),
        format!("DELETE FROM evidence WHERE task IN ({TASKS})"),
        format!("DELETE FROM claim_refusals WHERE task IN ({TASKS})"),
        format!("DELETE FROM feedback WHERE task IN ({TASKS})"),
        format!("DELETE FROM feedback_reports WHERE task IN ({TASKS})"),
        // Leases belong to agents and expire on their own; only the task link goes.
        format!("UPDATE leases SET task = NULL WHERE task IN ({TASKS})"),
        // Lease events name a task but no board.
        format!("DELETE FROM events WHERE board = ?1 OR task IN ({TASKS})"),
        "DELETE FROM asks WHERE board = ?1".to_owned(),
        "DELETE FROM said WHERE board = ?1".to_owned(),
        "DELETE FROM judges WHERE board = ?1".to_owned(),
        "DELETE FROM stood_down WHERE board = ?1".to_owned(),
        // Parents are on the same board, so no task elsewhere refers to these.
        "UPDATE tasks SET parent = NULL WHERE board = ?1".to_owned(),
        "INSERT OR REPLACE INTO deleted_boards (slug, next_number) \
         SELECT slug, next_number FROM boards WHERE slug = ?1"
            .to_owned(),
        "DELETE FROM tasks WHERE board = ?1".to_owned(),
        "DELETE FROM board_members WHERE board = ?1".to_owned(),
        "DELETE FROM boards WHERE slug = ?1".to_owned(),
    ] {
        cx.db.execute(&sql, [s])?;
    }
    cx.emit(
        Some(slug),
        actor,
        EventType::BoardUpdated,
        None,
        serde_json::json!({ "fields": ["deleted"], "title": board.title }),
    )?;
    let now = cx.now_ms();
    for (task, (board, removed)) in dependents {
        cx.db.execute(
            "UPDATE tasks SET version = version + 1, updated_at = ?2 WHERE id = ?1",
            params![task, now],
        )?;
        let (task, board): (TaskId, BoardSlug) = (parse_id(&task)?, parse_id(&board)?);
        cx.emit(
            Some(&board),
            actor,
            EventType::TaskUpdated,
            Some(&task),
            serde_json::json!({
                "fields": ["blockers"],
                "blockers_removed": removed,
                "board_deleted": slug,
            }),
        )?;
    }
    Ok(())
}

/// Resumes the feedback board when it is paused, a state left from servers that allowed it,
/// as the server, so the daemon wakes its coordinator. Returns whether it did.
pub(crate) fn resume_feedback_board(cx: &mut Ctx<'_>) -> Result<bool> {
    let slug = cx.s.feedback_board.clone();
    if !cx.exists(
        "SELECT 1 FROM boards WHERE slug = ?1 AND status = 'paused'",
        [slug.as_str()],
    )? {
        return Ok(false);
    }
    let resume = UpdateBoard {
        status: Some(BoardStatus::Active),
        ..UpdateBoard::default()
    };
    update(cx, "server", &slug, resume, false)?;
    Ok(true)
}

/// What the sweep did to one temporary board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Swept {
    Closed,
    Deleted,
}

/// Idle temporary boards: not paused (a pause waits on the user, however long it lasts), no
/// open ask, no task in `doing` with a live owner, and no task not done that an unfinished
/// task on another board waits on (deleting the board would let that task start without its
/// prerequisite). Each comes
/// with whether it is closed and how long it has been idle: the time since the newer of its
/// own change and its newest event, less the time the server was down since then. The
/// server's notices (a `task_updated` of its own that changes no field: a quiet review, an
/// owner away, a passed `not_before`) do not count: they repeat, or come, while the board
/// is left alone.
fn idle_temporary(cx: &Ctx<'_>, only: Option<&BoardSlug>) -> Result<Vec<(BoardSlug, bool, i64)>> {
    let rows: Vec<(String, String, i64)> = cx
        .db
        .prepare_cached(&format!(
            "SELECT b.slug, b.status, MAX(b.updated_at, COALESCE( \
                 (SELECT e.at FROM events e WHERE e.board = b.slug \
                  AND NOT (e.actor = 'server' AND e.type = 'task_updated' \
                           AND json_extract(e.payload, '$.fields') = '[]') \
                  ORDER BY e.seq DESC LIMIT 1), \
                 0)) \
             FROM boards b \
             WHERE b.temporary = 1 AND b.status != 'paused' \
               AND (:only IS NULL OR b.slug = :only) \
               AND NOT EXISTS (SELECT 1 FROM asks a WHERE a.board = b.slug AND a.status = 'open') \
               AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.board = b.slug \
                               AND t.status = 'doing' AND {live}) \
               AND NOT EXISTS (SELECT 1 FROM task_blockers tb \
                               JOIN tasks bt ON bt.id = tb.blocker \
                               JOIN tasks wt ON wt.id = tb.task \
                               WHERE bt.board = b.slug AND bt.status != 'done' \
                               AND wt.board != b.slug \
                               AND wt.status NOT IN ('done', 'dropped')) \
             ORDER BY b.slug",
            live = live_sql("t.owner")
        ))?
        .query_map(
            rusqlite::named_params! {
                ":now": cx.now_ms(),
                ":cutoff": cx.stale_cutoff(),
                ":only": only.map(BoardSlug::as_str),
            },
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(slug, status, last)| {
            Ok((
                parse_id(&slug)?,
                status == enum_str(&BoardStatus::Closed),
                cx.now_ms() - last - downtime_since(cx, last)?,
            ))
        })
        .collect()
}

/// Records that the server run started at `started` is alive now. The sweeper calls it
/// every minute, before it sweeps temporary boards, and at shutdown, so the next run knows
/// how long the server was down.
pub(crate) fn mark_alive(cx: &mut Ctx<'_>, started: i64) -> Result<()> {
    cx.db
        .prepare_cached(
            "INSERT INTO server_runs (started_at, alive_at) VALUES (?1, ?2) \
             ON CONFLICT (started_at) DO UPDATE SET alive_at = excluded.alive_at",
        )?
        .execute(params![started, cx.now_ms()])?;
    Ok(())
}

/// How long the server was down after `last`: of each run that started after it, the time
/// from the later of `last` and the run before's last sign of life to its start.
fn downtime_since(cx: &Ctx<'_>, last: i64) -> Result<i64> {
    Ok(cx
        .db
        .prepare_cached(
            "SELECT COALESCE(SUM(MAX(0, started_at - MAX(before_alive, ?1))), 0) FROM ( \
               SELECT started_at, LAG(alive_at) OVER (ORDER BY started_at) AS before_alive \
               FROM server_runs) \
             WHERE before_alive IS NOT NULL AND started_at > ?1",
        )?
        .query_row([last], |r| r.get(0))?)
}

/// What is due for a board idle for `idle`: closing once idle for
/// `temporary_close_after_secs`, deleting once closed and idle for
/// `temporary_delete_after_secs`.
fn due(cx: &Ctx<'_>, closed: bool, idle: i64) -> Option<Swept> {
    if closed {
        (idle >= cx.s.temporary_delete_ms()).then_some(Swept::Deleted)
    } else {
        (idle >= cx.s.temporary_close_ms()).then_some(Swept::Closed)
    }
}

/// Temporary boards due to be closed or deleted.
pub(crate) fn temporary_due(cx: &Ctx<'_>) -> Result<Vec<BoardSlug>> {
    Ok(idle_temporary(cx, None)?
        .into_iter()
        .filter(|&(_, closed, idle)| due(cx, closed, idle).is_some())
        .map(|(slug, _, _)| slug)
        .collect())
}

/// Closes or deletes one temporary board if that is still due. The sweep runs one
/// transaction per board, so a failure on one does not hold up the others.
pub(crate) fn sweep_temporary(cx: &mut Ctx<'_>, slug: &BoardSlug) -> Result<Option<Swept>> {
    let Some((_, closed, idle)) = idle_temporary(cx, Some(slug))?.pop() else {
        return Ok(None);
    };
    let swept = due(cx, closed, idle);
    match swept {
        Some(Swept::Deleted) => delete(cx, "server", slug)?,
        Some(Swept::Closed) => {
            let close = UpdateBoard {
                status: Some(BoardStatus::Closed),
                ..UpdateBoard::default()
            };
            update(cx, "server", slug, close, false)?;
        }
        None => {}
    }
    Ok(swept)
}

pub(crate) fn join(cx: &mut Ctx<'_>, slug: &BoardSlug, agent: &AgentId) -> Result<Board> {
    get_board(cx, slug)?;
    touch_agent(cx, agent)?;
    join_board(cx, slug, agent)?;
    get_board(cx, slug)
}

struct Lease {
    agent: Option<String>,
    device: Option<String>,
    expires_at: Option<i64>,
    autopilot: bool,
    status: String,
}

fn coordinator_lease(cx: &Ctx<'_>, slug: &BoardSlug) -> Result<Lease> {
    cx.db
        .query_row(
            "SELECT coordinator, coordinator_device, coordinator_expires_at, autopilot, status \
             FROM boards WHERE slug = ?1",
            [slug.as_str()],
            |r| {
                Ok(Lease {
                    agent: r.get(0)?,
                    device: r.get(1)?,
                    expires_at: r.get(2)?,
                    autopilot: r.get(3)?,
                    status: r.get(4)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| board_missing(slug))
}

/// The thread `slug` last named coordinator, with its device: kept after the lease ends.
fn last_coordinator(cx: &Ctx<'_>, slug: &BoardSlug) -> Result<Option<(String, Option<String>)>> {
    let row: Option<(Option<String>, Option<String>)> = cx
        .db
        .query_row(
            "SELECT last_coordinator, last_coordinator_device FROM boards WHERE slug = ?1",
            [slug.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.and_then(|(agent, device)| agent.map(|agent| (agent, device))))
}

/// Stands down the thread `slug` last named coordinator, for `reason`: the snapshot holds
/// it ([`crate::snapshot`]'s `held_by`) until a claim names it coordinator again. A thread
/// already stood down keeps its first time and reason.
fn stand_down_last(cx: &Ctx<'_>, slug: &BoardSlug, reason: &str) -> Result<()> {
    cx.db.execute(
        "INSERT INTO stood_down (board, agent, since, reason) \
         SELECT slug, last_coordinator, ?2, ?3 FROM boards \
         WHERE slug = ?1 AND last_coordinator IS NOT NULL \
         ON CONFLICT (board, agent) DO NOTHING",
        params![slug.as_str(), cx.now_ms(), reason],
    )?;
    Ok(())
}

/// Takes or renews the coordinator lease for `req.device`; `req.agent`, once the thread
/// exists, names the coordinator.
pub(crate) fn claim_coordinator(
    cx: &mut Ctx<'_>,
    actor: &str,
    slug: &BoardSlug,
    req: ClaimCoordinator,
) -> Result<Board> {
    let lease = coordinator_lease(cx, slug)?;
    if !lease.autopilot {
        return Err(invalid(
            format!("board {slug} is not on autopilot"),
            format!("switch autopilot on (PATCH /v1/boards/{slug}) before starting a coordinator"),
        ));
    }
    if lease.status != enum_str(&BoardStatus::Active) {
        return Err(invalid(
            format!("board {slug} is {}", lease.status),
            "only active boards run a coordinator; set the board active first",
        ));
    }
    let now = cx.now_ms();
    let held = lease.expires_at.is_some_and(|e| e > now);
    if held && lease.device.as_deref() != Some(req.device.as_str()) {
        let holder = lease.device.unwrap_or_default();
        let left = lease.expires_at.unwrap_or(now) - now;
        return Err(refused(
            format!(
                "device {holder} holds the coordinator lease of {slug} for {} more",
                ago(chrono::TimeDelta::milliseconds(left))
            ),
            "leave the coordinator to that device; retry after the lease expires",
        ));
    }
    if let Some(agent) = &req.agent
        && let Some(device) = agent.dispatch_device()
        && device != req.device
    {
        return Err(forbidden(
            format!(
                "{agent} is device {device}'s dispatch agent, not device {}'s",
                req.device
            ),
            format!(
                "name your own device's dispatch agent: agent={}",
                AgentId::dispatch(&req.device)
            ),
        ));
    }
    if let Some(agent) = &req.agent {
        // A device claims or renews the lease for the agent: no sign the agent itself is at
        // work, so its last_seen stays, and an agent left marked running is found away.
        let known: bool = cx.db.query_row(
            "SELECT EXISTS (SELECT 1 FROM agents WHERE id = ?1)",
            [agent.as_str()],
            |r| r.get(0),
        )?;
        if !known {
            return Err(agent_missing(agent));
        }
        join_board(cx, slug, agent)?;
        cx.db.execute(
            "UPDATE agents SET role = 'coordinator' WHERE id = ?1",
            [agent.as_str()],
        )?;
    }
    // A renewal by the same device keeps the coordinator it named before.
    let agent = match (&req.agent, held) {
        (Some(agent), _) => Some(agent.to_string()),
        (None, true) => lease.agent.clone(),
        (None, false) => None,
    };
    // A thread the claim names coordinator, or keeps named, is no longer stood down, and
    // the thread it replaces is. The daemon's dispatch agent is not a thread: a switch to
    // daemon dispatch already stood the last thread down. A claim on a board that runs no
    // coordinator thread on this device changes neither: a daemon that has not yet seen
    // the switch to daemon dispatch, or that its device is no longer a home, still renews.
    let runs_thread: bool = cx.db.query_row(
        "SELECT dispatch = 'thread' \
                AND EXISTS (SELECT 1 FROM json_each(home_devices) WHERE value = ?2) \
         FROM boards WHERE slug = ?1",
        params![slug.as_str(), req.device.as_str()],
        |r| r.get(0),
    )?;
    if let Some(agent) = agent
        .as_deref()
        .filter(|a| runs_thread && !a.starts_with("dispatch:"))
    {
        if let Some((last, _)) = last_coordinator(cx, slug)?
            && last != agent
        {
            stand_down_last(cx, slug, "another thread coordinates the board now")?;
        }
        cx.db.execute(
            "DELETE FROM stood_down WHERE board = ?1 AND agent = ?2",
            params![slug.as_str(), agent],
        )?;
        cx.db.execute(
            "UPDATE boards SET last_coordinator = ?2, last_coordinator_device = ?3 WHERE slug = ?1",
            params![slug.as_str(), agent, req.device.as_str()],
        )?;
    }
    cx.db.execute(
        "UPDATE boards SET coordinator = ?2, coordinator_device = ?3, coordinator_expires_at = ?4 \
         WHERE slug = ?1",
        params![
            slug.as_str(),
            agent,
            req.device.as_str(),
            expires(cx, cx.s.coordinator_ms())
        ],
    )?;
    if !held || agent != lease.agent {
        cx.db.execute(
            "UPDATE boards SET updated_at = ?2 WHERE slug = ?1",
            params![slug.as_str(), now],
        )?;
        cx.emit(
            Some(slug),
            actor,
            EventType::BoardUpdated,
            None,
            serde_json::json!({
                "fields": ["coordinator"],
                "coordinator": agent,
                "device": req.device,
            }),
        )?;
    }
    get_board(cx, slug)
}

/// Releases the coordinator lease. Only the holding device or the user may.
pub(crate) fn release_coordinator(
    cx: &mut Ctx<'_>,
    actor: &str,
    slug: &BoardSlug,
    device: Option<&DeviceName>,
    is_user: bool,
) -> Result<Board> {
    let lease = coordinator_lease(cx, slug)?;
    let held = lease.expires_at.is_some_and(|e| e > cx.now_ms());
    if !held {
        return get_board(cx, slug);
    }
    if !is_user && lease.device.as_deref() != device.map(DeviceName::as_str) {
        return Err(forbidden(
            format!(
                "device {} holds the coordinator lease of {slug}",
                lease.device.unwrap_or_default()
            ),
            "only the holding device or the user may release it",
        ));
    }
    cx.db.execute(
        "UPDATE boards SET coordinator = NULL, coordinator_device = NULL, \
         coordinator_expires_at = NULL, updated_at = ?2 WHERE slug = ?1",
        params![slug.as_str(), cx.now_ms()],
    )?;
    cx.emit(
        Some(slug),
        actor,
        EventType::BoardUpdated,
        None,
        // The device that held the lease, so its daemon can recognize its own release.
        serde_json::json!({ "fields": ["coordinator"], "coordinator": null, "device": lease.device }),
    )?;
    get_board(cx, slug)
}
