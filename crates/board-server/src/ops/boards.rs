use board_proto::{
    Board, BoardSlug, BoardStatus, ClaimCoordinator, CreateBoard, DeviceName, EventType, TaskId,
    UpdateBoard,
};
use rusqlite::{OptionalExtension, params};
use std::collections::BTreeMap;

use crate::config::Settings;
use crate::db::{Ctx, enum_str, live_sql, parse_id};
use crate::error::{Result, bad_transition, forbidden, invalid, refused};
use crate::ops::{expires, join_board, touch_agent};
use crate::store::{board_missing, get_board};
use crate::text::{ago, check_line_or, check_text, normalize_repo};
use board_proto::BOARD_TITLE_LONGER_TEXT_GOES;

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
    req: UpdateBoard,
    by_user: bool,
) -> Result<Board> {
    let before = get_board(cx, slug)?;
    if req.temporary == Some(true) && !before.temporary && !by_user {
        return Err(forbidden(
            format!("only the user may make board {slug} temporary"),
            "temporary boards are deleted once idle; ask the user (ask_user), or create a \
             new board with temporary set",
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
    if fields.is_empty() {
        return Ok(before);
    }
    cx.db.execute(
        "UPDATE boards SET title = ?2, goal = ?3, status = ?4, repos = ?5, autopilot = ?6, \
         home_devices = ?7, updated_at = ?8, temporary = ?9 WHERE slug = ?1",
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
        ],
    )?;
    cx.emit(
        Some(slug),
        actor,
        EventType::BoardUpdated,
        None,
        serde_json::json!({ "fields": fields }),
    )?;
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

/// What the sweep did to one temporary board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Swept {
    Closed,
    Deleted,
}

/// Idle temporary boards: no open ask, no task in `doing` with a live owner, and no task not
/// done that an unfinished task on another board waits on (deleting the board would let
/// that task start without its prerequisite). Each comes
/// with whether it is closed and when it was last active: the newest of its own change, its
/// newest event and `since`, the server's start, so time the server was down does not
/// count as idle.
fn idle_temporary(
    cx: &Ctx<'_>,
    since: i64,
    only: Option<&BoardSlug>,
) -> Result<Vec<(BoardSlug, bool, i64)>> {
    let rows: Vec<(String, String, i64)> = cx
        .db
        .prepare_cached(&format!(
            "SELECT b.slug, b.status, MAX(:since, b.updated_at, COALESCE( \
                 (SELECT e.at FROM events e WHERE e.board = b.slug ORDER BY e.seq DESC LIMIT 1), \
                 0)) \
             FROM boards b \
             WHERE b.temporary = 1 AND (:only IS NULL OR b.slug = :only) \
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
                ":since": since,
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
                last,
            ))
        })
        .collect()
}

/// What is due for a board last active at `last`: closing once idle for
/// `temporary_close_after_secs`, deleting once closed and idle for
/// `temporary_delete_after_secs`.
fn due(cx: &Ctx<'_>, closed: bool, last: i64) -> Option<Swept> {
    let idle = cx.now_ms() - last;
    if closed {
        (idle >= cx.s.temporary_delete_ms()).then_some(Swept::Deleted)
    } else {
        (idle >= cx.s.temporary_close_ms()).then_some(Swept::Closed)
    }
}

/// Temporary boards due to be closed or deleted, given the server started at `since`.
pub(crate) fn temporary_due(cx: &Ctx<'_>, since: i64) -> Result<Vec<BoardSlug>> {
    Ok(idle_temporary(cx, since, None)?
        .into_iter()
        .filter(|&(_, closed, last)| due(cx, closed, last).is_some())
        .map(|(slug, _, _)| slug)
        .collect())
}

/// Closes or deletes one temporary board if that is still due. The sweep runs one
/// transaction per board, so a failure on one does not hold up the others.
pub(crate) fn sweep_temporary(
    cx: &mut Ctx<'_>,
    slug: &BoardSlug,
    since: i64,
) -> Result<Option<Swept>> {
    let Some((_, closed, last)) = idle_temporary(cx, since, Some(slug))?.pop() else {
        return Ok(None);
    };
    let swept = due(cx, closed, last);
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

pub(crate) fn join(
    cx: &mut Ctx<'_>,
    slug: &BoardSlug,
    agent: &board_proto::AgentId,
) -> Result<Board> {
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
    if let Some(agent) = &req.agent {
        touch_agent(cx, agent)?;
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
