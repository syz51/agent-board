//! Loading rows into wire types, with the computed fields (`ready`, `owner_stale`, board
//! counts, the live coordinator).

use std::collections::{BTreeMap, HashMap, HashSet};

use board_proto::{
    Agent, AgentId, Ask, Board, BoardSlug, Event, EventType, Evidence, Handoff, Lease, Note,
    StatusCounts, Task, TaskDetail, TaskId, TaskStatus, Timestamp,
};
use rusqlite::types::ToSql;
use rusqlite::{OptionalExtension, Row, named_params, params};

use crate::db::{Ctx, enum_str, parse_enum, parse_id, parse_json, ready_sql, ts};
use crate::error::{Result, not_found};

pub(crate) type NamedParams<'a> = Vec<(&'a str, &'a dyn ToSql)>;

/// Liveness facts about one agent.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub live: bool,
    pub last_seen: i64,
}

/// Whether `agent` is live: seen within the stale period, or paused until a time ahead.
pub(crate) fn seen(cx: &Ctx<'_>, agent: &str) -> Result<Option<Seen>> {
    let row = cx
        .db
        .query_row(
            "SELECT last_seen, state, paused_until FROM agents WHERE id = ?1",
            [agent],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(|(last_seen, state, paused_until)| Seen {
        live: live(cx, last_seen, &state, paused_until),
        last_seen,
    }))
}

/// Whether an agent with these columns is live (see [`seen`]).
fn live(cx: &Ctx<'_>, last_seen: i64, state: &str, paused_until: Option<i64>) -> bool {
    last_seen > cx.stale_cutoff()
        || (state == "paused" && paused_until.is_some_and(|p| p > cx.now_ms()))
}

pub(crate) fn is_live(cx: &Ctx<'_>, agent: &str) -> Result<bool> {
    Ok(seen(cx, agent)?.is_some_and(|s| s.live))
}

pub(crate) fn handle_of(cx: &Ctx<'_>, agent: &str) -> Result<String> {
    Ok(cx
        .db
        .query_row("SELECT handle FROM agents WHERE id = ?1", [agent], |r| {
            r.get(0)
        })
        .optional()?
        .unwrap_or_else(|| agent.to_owned()))
}

// ---------------------------------------------------------------------------------------
// Tasks

const TASK_COLUMNS: &str = "t.id, t.board, t.parent, t.title, t.body, t.acceptance, t.status, \
     t.priority, t.blocked_reason, t.owner, t.auto, t.created_by, t.version, t.created_at, \
     t.updated_at, t.not_before, t.summary";

fn task_row(r: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: parse_id(&r.get::<_, String>(0)?)?,
        board: parse_id(&r.get::<_, String>(1)?)?,
        parent: r
            .get::<_, Option<String>>(2)?
            .map(|p| parse_id(&p))
            .transpose()?,
        title: r.get(3)?,
        body: r.get(4)?,
        acceptance: r.get(5)?,
        status: parse_enum(&r.get::<_, String>(6)?)?,
        priority: parse_enum(&r.get::<_, String>(7)?)?,
        blocked_reason: r.get(8)?,
        owner: r
            .get::<_, Option<String>>(9)?
            .map(|o| parse_id(&o))
            .transpose()?,
        owner_stale: false,
        blockers: Vec::new(),
        ready: false,
        not_before: r.get::<_, Option<i64>>(15)?.map(ts),
        auto: r.get(10)?,
        created_by: parse_id(&r.get::<_, String>(11)?)?,
        version: r.get(12)?,
        created_at: ts(r.get(13)?),
        updated_at: ts(r.get(14)?),
        feedback: None,
        summary: r.get(16)?,
    })
}

/// Loads tasks matching `filter` (SQL over alias `t`, may use `:now` and `:cutoff`), in
/// list order: priority, then id number.
pub(crate) fn load_tasks(cx: &Ctx<'_>, filter: &str, extra: &NamedParams<'_>) -> Result<Vec<Task>> {
    load_tasks_up_to(cx, filter, extra, None)
}

/// Like [`load_tasks`], the first `limit` of them when given.
pub(crate) fn load_tasks_up_to(
    cx: &Ctx<'_>,
    filter: &str,
    extra: &NamedParams<'_>,
    limit: Option<usize>,
) -> Result<Vec<Task>> {
    let sql = format!(
        "SELECT {TASK_COLUMNS} FROM tasks t WHERE {filter} \
         ORDER BY t.priority, t.number, t.board LIMIT :limit"
    );
    let now = cx.now_ms();
    let cutoff = cx.stale_cutoff();
    // A negative limit is no limit.
    let limit = limit.map_or(-1, |l| i64::try_from(l).unwrap_or(i64::MAX));
    let mut params: NamedParams<'_> = Vec::with_capacity(extra.len() + 3);
    params.push((":limit", &limit));
    if filter.contains(":now") {
        params.push((":now", &now));
    }
    if filter.contains(":cutoff") {
        params.push((":cutoff", &cutoff));
    }
    params.extend(extra.iter().copied());
    let mut stmt = cx.db.prepare_cached(&sql)?;
    let tasks = stmt
        .query_map(params.as_slice(), task_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    complete_tasks(cx, tasks)
}

/// The blockers of a batch of tasks, in blocker order. Every batch query binds its ids as
/// one JSON array read with `json_each`, so a batch is one statement whatever its size.
pub(crate) const BATCH_BLOCKERS: &str = "SELECT b.task, b.blocker, bt.status \
     FROM task_blockers b JOIN tasks bt ON bt.id = b.blocker \
     WHERE b.task IN (SELECT value FROM json_each(?1)) ORDER BY bt.number, bt.board";

/// Fills in blockers, `owner_stale`, `ready` and feedback, with one query for each across
/// all of `tasks`.
fn complete_tasks(cx: &Ctx<'_>, mut tasks: Vec<Task>) -> Result<Vec<Task>> {
    if tasks.is_empty() {
        return Ok(tasks);
    }
    let ids = json_ids(tasks.iter().map(|t| t.id.as_str()));

    // Each task's blockers in blocker order, and whether all of them are done.
    let mut blockers: HashMap<String, (Vec<TaskId>, bool)> = HashMap::new();
    let mut stmt = cx.db.prepare_cached(BATCH_BLOCKERS)?;
    let mut rows = stmt.query([&ids])?;
    while let Some(r) = rows.next()? {
        let task: String = r.get(0)?;
        let blocker = parse_id(&r.get::<_, String>(1)?)?;
        let done = r.get::<_, String>(2)? == "done";
        let entry = blockers.entry(task).or_insert_with(|| (Vec::new(), true));
        entry.0.push(blocker);
        entry.1 &= done;
    }
    drop(rows);
    drop(stmt);

    let owners = json_ids(
        tasks
            .iter()
            .filter_map(|t| t.owner.as_ref().map(AgentId::as_str)),
    );
    let mut live_owners: HashSet<String> = HashSet::new();
    let mut stmt = cx.db.prepare_cached(
        "SELECT id, last_seen, state, paused_until FROM agents \
         WHERE id IN (SELECT value FROM json_each(?1))",
    )?;
    let mut rows = stmt.query([&owners])?;
    while let Some(r) = rows.next()? {
        let id: String = r.get(0)?;
        if live(cx, r.get(1)?, &r.get::<_, String>(2)?, r.get(3)?) {
            live_owners.insert(id);
        }
    }
    drop(rows);
    drop(stmt);

    let with_feedback: HashSet<String> = cx
        .db
        .prepare_cached(
            "SELECT task FROM feedback WHERE task IN (SELECT value FROM json_each(?1))",
        )?
        .query_map([&ids], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    for task in &mut tasks {
        let all_done = match blockers.remove(task.id.as_str()) {
            Some((ids, all_done)) => {
                task.blockers = ids;
                all_done
            }
            None => true,
        };
        let owner_live = task
            .owner
            .as_ref()
            .is_some_and(|o| live_owners.contains(o.as_str()));
        task.owner_stale = task.owner.is_some() && !owner_live;
        let waiting = task.not_before.is_some_and(|at| at > cx.now);
        task.ready = task.status == TaskStatus::Open && all_done && !owner_live && !waiting;
        if with_feedback.contains(task.id.as_str()) {
            task.feedback = crate::ops::feedback::load(cx, &task.id)?;
        }
    }
    Ok(tasks)
}

/// Ids as one JSON array, for a batch query.
fn json_ids<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    serde_json::to_string(&ids.collect::<Vec<_>>()).expect("strings serialize")
}

/// A task's evidence, oldest first.
pub(crate) fn load_evidence(cx: &Ctx<'_>, task: &str) -> Result<Vec<Evidence>> {
    Ok(cx
        .db
        .prepare_cached(
            "SELECT kind, value, exit_code, agent, at FROM evidence WHERE task = ?1 ORDER BY id",
        )?
        .query_map([task], |r| {
            Ok(Evidence {
                kind: parse_enum(&r.get::<_, String>(0)?)?,
                value: r.get(1)?,
                exit_code: r.get(2)?,
                agent: parse_id(&r.get::<_, String>(3)?)?,
                at: ts(r.get(4)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn find_task(cx: &Ctx<'_>, id: &TaskId) -> Result<Option<Task>> {
    Ok(load_tasks(cx, "t.id = :id", &vec![(":id", &id.as_str())])?
        .into_iter()
        .next())
}

pub(crate) fn get_task(cx: &Ctx<'_>, id: &TaskId) -> Result<Task> {
    find_task(cx, id)?.ok_or_else(|| {
        not_found(
            format!("task {id} does not exist"),
            format!(
                "check the id; task_list board={} lists its tasks",
                id.board()
            ),
        )
    })
}

pub(crate) fn task_detail(cx: &Ctx<'_>, id: &TaskId) -> Result<TaskDetail> {
    let task = get_task(cx, id)?;
    let notes = cx
        .db
        .prepare_cached("SELECT id, agent, kind, text, at FROM notes WHERE task = ?1 ORDER BY id")?
        .query_map([id.as_str()], |r| {
            Ok(Note {
                id: r.get(0)?,
                task: id.clone(),
                agent: parse_id(&r.get::<_, String>(1)?)?,
                kind: parse_enum(&r.get::<_, String>(2)?)?,
                text: r.get(3)?,
                at: ts(r.get(4)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let evidence = load_evidence(cx, id.as_str())?;
    let blocks = task_ids(
        cx,
        "SELECT b.task FROM task_blockers b JOIN tasks t ON t.id = b.task \
         WHERE b.blocker = ?1 ORDER BY t.number, t.board",
        id,
    )?;
    let children = task_ids(
        cx,
        "SELECT id FROM tasks WHERE parent = ?1 ORDER BY number",
        id,
    )?;
    // The newest events, oldest first; a cut history no longer starts with task_created.
    let mut history = latest_events(
        cx,
        "task = :task",
        &vec![(":task", &id.as_str())],
        0,
        TASK_HISTORY_LIMIT,
    )?;
    history.reverse();
    Ok(TaskDetail {
        task,
        notes,
        evidence,
        blocks,
        children,
        history,
    })
}

/// Events a task's detail shows at most.
pub(crate) const TASK_HISTORY_LIMIT: u32 = 200;

fn task_ids(cx: &Ctx<'_>, sql: &str, id: &TaskId) -> Result<Vec<TaskId>> {
    Ok(cx
        .db
        .prepare_cached(sql)?
        .query_map([id.as_str()], |r| parse_id(&r.get::<_, String>(0)?))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------------------
// Boards

pub(crate) fn counts(cx: &Ctx<'_>, board: &BoardSlug) -> Result<StatusCounts> {
    let mut counts = StatusCounts::default();
    let mut stmt = cx
        .db
        .prepare_cached("SELECT status, COUNT(*) FROM tasks WHERE board = ?1 GROUP BY status")?;
    let rows = stmt.query_map([board.as_str()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?))
    })?;
    for row in rows {
        let (status, n) = row?;
        match parse_enum::<TaskStatus>(&status)? {
            TaskStatus::Open => counts.open = n,
            TaskStatus::Doing => counts.doing = n,
            TaskStatus::Blocked => counts.blocked = n,
            TaskStatus::Review => counts.review = n,
            TaskStatus::Done => counts.done = n,
            TaskStatus::Dropped => counts.dropped = n,
        }
    }
    counts.ready = cx.db.query_row(
        &format!(
            "SELECT COUNT(*) FROM tasks t WHERE t.board = :board AND {}",
            ready_sql("t")
        ),
        named_params! {
            ":board": board.as_str(),
            ":now": cx.now_ms(),
            ":cutoff": cx.stale_cutoff(),
        },
        |r| r.get(0),
    )?;
    Ok(counts)
}

pub(crate) fn find_board(cx: &Ctx<'_>, slug: &BoardSlug) -> Result<Option<Board>> {
    let row = cx
        .db
        .query_row(
            "SELECT slug, title, goal, status, repos, autopilot, home_devices, coordinator, \
             coordinator_expires_at, created_at, updated_at, temporary FROM boards \
             WHERE slug = ?1",
            [slug.as_str()],
            |r| {
                let expires: Option<i64> = r.get(8)?;
                let coordinator: Option<String> = r.get(7)?;
                Ok(Board {
                    slug: parse_id(&r.get::<_, String>(0)?)?,
                    title: r.get(1)?,
                    goal: r.get(2)?,
                    status: parse_enum(&r.get::<_, String>(3)?)?,
                    temporary: r.get(11)?,
                    repos: parse_json(&r.get::<_, String>(4)?)?,
                    autopilot: r.get(5)?,
                    coordinator: match (coordinator, expires) {
                        (Some(c), Some(e)) if e > cx.now_ms() => Some(parse_id(&c)?),
                        _ => None,
                    },
                    home_devices: parse_json(&r.get::<_, String>(6)?)?,
                    counts: StatusCounts::default(),
                    open_asks: 0,
                    created_at: ts(r.get(9)?),
                    updated_at: ts(r.get(10)?),
                })
            },
        )
        .optional()?;
    let Some(mut board) = row else {
        return Ok(None);
    };
    board.counts = counts(cx, slug)?;
    board.open_asks = cx.db.query_row(
        "SELECT COUNT(*) FROM asks WHERE board = ?1 AND status = 'open'",
        [slug.as_str()],
        |r| r.get(0),
    )?;
    Ok(Some(board))
}

pub(crate) fn get_board(cx: &Ctx<'_>, slug: &BoardSlug) -> Result<Board> {
    find_board(cx, slug)?.ok_or_else(|| board_missing(slug))
}

pub(crate) fn board_missing(slug: &BoardSlug) -> crate::Error {
    not_found(
        format!("board {slug} does not exist"),
        "check the slug; GET /v1/boards lists the boards",
    )
}

pub(crate) fn list_boards(cx: &Ctx<'_>) -> Result<Vec<Board>> {
    let slugs = cx
        .db
        .prepare_cached("SELECT slug FROM boards ORDER BY slug")?
        .query_map([], |r| parse_id::<BoardSlug>(&r.get::<_, String>(0)?))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    slugs.iter().map(|s| get_board(cx, s)).collect()
}

// ---------------------------------------------------------------------------------------
// Agents

const AGENT_COLUMNS: &str = "id, handle, runtime, device, role, title, repo, branch, cwd, \
     native_ids, forked_from, state, paused_until, last_seen, created_at";

fn agent_row(r: &Row<'_>) -> rusqlite::Result<Agent> {
    Ok(Agent {
        id: parse_id(&r.get::<_, String>(0)?)?,
        handle: r.get(1)?,
        runtime: parse_enum(&r.get::<_, String>(2)?)?,
        device: parse_id(&r.get::<_, String>(3)?)?,
        role: parse_enum(&r.get::<_, String>(4)?)?,
        title: r.get(5)?,
        repo: r.get(6)?,
        branch: r.get(7)?,
        cwd: r.get(8)?,
        native_ids: parse_json::<BTreeMap<String, String>>(&r.get::<_, String>(9)?)?,
        forked_from: r
            .get::<_, Option<String>>(10)?
            .map(|f| parse_id(&f))
            .transpose()?,
        state: parse_enum(&r.get::<_, String>(11)?)?,
        paused_until: r.get::<_, Option<i64>>(12)?.map(ts),
        boards: Vec::new(),
        last_seen: ts(r.get(13)?),
        created_at: ts(r.get(14)?),
    })
}

pub(crate) fn load_agents(
    cx: &Ctx<'_>,
    filter: &str,
    extra: &NamedParams<'_>,
) -> Result<Vec<Agent>> {
    let sql = format!("SELECT {AGENT_COLUMNS} FROM agents WHERE {filter} ORDER BY handle");
    let mut agents = cx
        .db
        .prepare_cached(&sql)?
        .query_map(extra.as_slice(), agent_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for agent in &mut agents {
        agent.boards = agent_boards(cx, &agent.id)?;
    }
    Ok(agents)
}

pub(crate) fn agent_boards(cx: &Ctx<'_>, agent: &AgentId) -> Result<Vec<BoardSlug>> {
    Ok(cx
        .db
        .prepare_cached("SELECT board FROM board_members WHERE agent = ?1 ORDER BY board")?
        .query_map([agent.as_str()], |r| parse_id(&r.get::<_, String>(0)?))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn find_agent(cx: &Ctx<'_>, id: &AgentId) -> Result<Option<Agent>> {
    Ok(load_agents(cx, "id = :id", &vec![(":id", &id.as_str())])?
        .into_iter()
        .next())
}

pub(crate) fn get_agent(cx: &Ctx<'_>, id: &AgentId) -> Result<Agent> {
    find_agent(cx, id)?.ok_or_else(|| agent_missing(id))
}

pub(crate) fn agent_missing(id: &AgentId) -> crate::Error {
    not_found(
        format!("agent {id} is not registered"),
        "register first with POST /v1/agents (the session-start hook does this)",
    )
}

pub(crate) fn find_handoff(cx: &Ctx<'_>, agent: &AgentId) -> Result<Option<Handoff>> {
    let Some(mut handoff) = cx
        .db
        .query_row(
            "SELECT now, next, blockers, version, updated_at FROM handoffs WHERE agent = ?1",
            [agent.as_str()],
            |r| {
                Ok(Handoff {
                    agent: agent.clone(),
                    now: r.get(0)?,
                    next: r.get(1)?,
                    blockers: r.get(2)?,
                    version: r.get(3)?,
                    updated_at: ts(r.get(4)?),
                    last_change_at: None,
                })
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    handoff.last_change_at = last_change(cx, agent)?;
    Ok(Some(handoff))
}

/// Events that are not a change to the board's work: the handoff itself and presence.
const NOT_CHANGES: [EventType; 3] = [
    EventType::HandoffUpdated,
    EventType::AgentRegistered,
    EventType::AgentStateChanged,
];

/// When the agent last changed the board or touched a file: its newest other event.
fn last_change(cx: &Ctx<'_>, agent: &AgentId) -> Result<Option<Timestamp>> {
    let [a, b, c] = NOT_CHANGES.map(|t| enum_str(&t));
    Ok(cx
        .db
        .query_row(
            "SELECT at FROM events WHERE actor = ?1 AND type NOT IN (?2, ?3, ?4) \
             ORDER BY seq DESC LIMIT 1",
            params![agent.as_str(), a, b, c],
            |r| r.get(0),
        )
        .optional()?
        .map(ts))
}

// ---------------------------------------------------------------------------------------
// Leases, asks, events

pub(crate) fn lease_row(r: &Row<'_>) -> rusqlite::Result<Lease> {
    Ok(Lease {
        id: r.get(0)?,
        agent: parse_id(&r.get::<_, String>(1)?)?,
        task: r
            .get::<_, Option<String>>(2)?
            .map(|t| parse_id(&t))
            .transpose()?,
        repo: r.get(3)?,
        glob: r.get(4)?,
        expires_at: ts(r.get(5)?),
    })
}

pub(crate) const LEASE_COLUMNS: &str = "id, agent, task, repo, glob, expires_at";

/// Live (unexpired) leases in `repo`.
pub(crate) fn live_leases(cx: &Ctx<'_>, repo: &str) -> Result<Vec<Lease>> {
    Ok(cx
        .db
        .prepare_cached(&format!(
            "SELECT {LEASE_COLUMNS} FROM leases WHERE repo = ?1 AND expires_at > ?2 ORDER BY id"
        ))?
        .query_map(params![repo, cx.now_ms()], lease_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

const ASK_COLUMNS: &str = "id, board, task, agent, kind, question, options, status, answer, \
     answered_via, created_at, answered_at";

fn ask_row(r: &Row<'_>) -> rusqlite::Result<Ask> {
    Ok(Ask {
        id: r.get(0)?,
        board: parse_id(&r.get::<_, String>(1)?)?,
        task: r
            .get::<_, Option<String>>(2)?
            .map(|t| parse_id(&t))
            .transpose()?,
        agent: parse_id(&r.get::<_, String>(3)?)?,
        kind: parse_enum(&r.get::<_, String>(4)?)?,
        question: r.get(5)?,
        options: parse_json(&r.get::<_, String>(6)?)?,
        status: parse_enum(&r.get::<_, String>(7)?)?,
        answer: r.get(8)?,
        answered_via: r.get(9)?,
        created_at: ts(r.get(10)?),
        answered_at: r.get::<_, Option<i64>>(11)?.map(ts),
    })
}

pub(crate) fn load_asks(cx: &Ctx<'_>, filter: &str, extra: &NamedParams<'_>) -> Result<Vec<Ask>> {
    Ok(cx
        .db
        .prepare_cached(&format!(
            "SELECT {ASK_COLUMNS} FROM asks WHERE {filter} ORDER BY id"
        ))?
        .query_map(extra.as_slice(), ask_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn get_ask(cx: &Ctx<'_>, id: i64) -> Result<Ask> {
    load_asks(cx, "id = :id", &vec![(":id", &id)])?
        .into_iter()
        .next()
        .ok_or_else(|| {
            not_found(
                format!("ask {id} does not exist"),
                "check the id; GET /v1/asks lists the asks",
            )
        })
}

fn event_row(r: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        seq: r.get(0)?,
        at: ts(r.get(1)?),
        board: r
            .get::<_, Option<String>>(2)?
            .map(|b| parse_id(&b))
            .transpose()?,
        actor: r.get(3)?,
        event_type: parse_enum(&r.get::<_, String>(4)?)?,
        task: r
            .get::<_, Option<String>>(5)?
            .map(|t| parse_id(&t))
            .transpose()?,
        payload: parse_json(&r.get::<_, String>(6)?)?,
    })
}

/// Events after `since` matching `filter`, oldest first, at most `limit`.
pub(crate) fn load_events(
    cx: &Ctx<'_>,
    filter: &str,
    extra: &NamedParams<'_>,
    since: i64,
    limit: u32,
) -> Result<Vec<Event>> {
    let mut params: NamedParams<'_> = vec![(":since", &since), (":limit", &limit)];
    params.extend(extra.iter().copied());
    Ok(cx
        .db
        .prepare_cached(&format!(
            "SELECT seq, at, board, actor, type, task, payload FROM events \
             WHERE seq > :since AND {filter} ORDER BY seq LIMIT :limit"
        ))?
        .query_map(params.as_slice(), event_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The newest `limit` events matching `filter` with `seq > since`, newest first.
pub(crate) fn latest_events(
    cx: &Ctx<'_>,
    filter: &str,
    extra: &NamedParams<'_>,
    since: i64,
    limit: u32,
) -> Result<Vec<Event>> {
    let mut params: NamedParams<'_> = vec![(":since", &since), (":limit", &limit)];
    params.extend(extra.iter().copied());
    Ok(cx
        .db
        .prepare_cached(&latest_events_sql(filter))?
        .query_map(params.as_slice(), event_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The query of [`latest_events`].
pub(crate) fn latest_events_sql(filter: &str) -> String {
    format!(
        "SELECT seq, at, board, actor, type, task, payload FROM events \
         WHERE seq > :since AND {filter} ORDER BY seq DESC LIMIT :limit"
    )
}
