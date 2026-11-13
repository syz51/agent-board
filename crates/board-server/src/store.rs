//! Loading rows into wire types, with the computed fields (`ready`, `owner_stale`, board
//! counts, the live coordinator).

use std::collections::{BTreeMap, HashMap, HashSet};

use board_proto::{
    Agent, AgentId, Ask, Board, BoardSlug, ChildTask, Event, EventType, Evidence, Handoff, Lease,
    Note, StatusCounts, Task, TaskDetail, TaskId, TaskStatus, Timestamp,
};
use rusqlite::types::ToSql;
use rusqlite::{OptionalExtension, Row, named_params, params};

use crate::db::{Ctx, parse_enum, parse_id, parse_json, ready_sql, ts};
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
     t.updated_at, t.not_before, t.summary, \
     (SELECT a.handle FROM agents a WHERE a.id = t.owner), t.rank";

/// List order over alias `t`: priority, then ranked tasks by rank, then the rest by number.
pub(crate) const LIST_ORDER: &str = "t.priority, t.rank IS NULL, t.rank, t.number, t.board";

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
        rank: r.get(18)?,
        blocked_reason: r.get(8)?,
        owner: r
            .get::<_, Option<String>>(9)?
            .map(|o| parse_id(&o))
            .transpose()?,
        owner_stale: false,
        proposal: false,
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
        child_count: 0,
        owner_handle: r.get(17)?,
        snippet: None,
        any_word: false,
    })
}

/// Marks around a found word in the text SQLite's `snippet` returns; stored text holds no
/// control characters.
const FOUND_START: char = '\u{1}';
const FOUND_END: char = '\u{2}';
/// Longest snippet of a search result, in characters.
const SNIPPET_CHARS_MAX: usize = 200;

/// A search result: the task, with the piece of its own text, or else of its notes, that
/// holds a found word. Words found only in the id or title need none: the line shows them.
fn found_task_row(r: &Row<'_>) -> rusqlite::Result<Task> {
    let mut task = task_row(r)?;
    for column in [19, 20] {
        let text: String = r.get(column)?;
        if text.contains(FOUND_START) {
            let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut open = false;
            let mut snippet: String = line
                .chars()
                .take(SNIPPET_CHARS_MAX)
                .map(|c| match c {
                    FOUND_START => {
                        open = true;
                        '['
                    }
                    FOUND_END => {
                        open = false;
                        ']'
                    }
                    c => c,
                })
                .collect();
            // The cut fell inside a found word: close its mark.
            if open {
                snippet.push(']');
            }
            task.snippet = Some(snippet);
            break;
        }
    }
    Ok(task)
}

/// How well a task's search document matches: lower is better. A word in the id or title
/// outweighs one in the task's own text, which outweighs one in its notes. Column weights
/// within one `bm25` do not do that: a score levels off with the word count and falls with
/// the document's length, so a short note would pass a long body. The place of the best
/// column sets a step (a `bm25` of columns that hold no word is 0, of others below 0), far
/// larger than any score, and the score orders the tasks of one step.
pub(crate) const SEARCH_RANK: &str = "(CASE \
     WHEN bm25(task_search, 1.0, 1.0, 0.0, 0.0) < 0 THEN -2000000.0 \
     WHEN bm25(task_search, 0.0, 0.0, 1.0, 0.0) < 0 THEN -1000000.0 \
     ELSE 0.0 END \
     + bm25(task_search, 4.0, 4.0, 2.0, 1.0))";

/// The rank among tasks that hold only some of the words: the words a task holds, rare
/// ones above all, count before the column they are in, so there is no step.
pub(crate) const SEARCH_RANK_ANY: &str = "bm25(task_search, 4.0, 4.0, 2.0, 1.0)";

/// Loads tasks matching `filter` (SQL over alias `t`, may use `:now` and `:cutoff`), in
/// list order ([`LIST_ORDER`]).
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
         ORDER BY {LIST_ORDER} LIMIT :limit"
    );
    query_tasks(cx, &sql, filter, extra, limit, task_row)
}

/// The tasks matching `filter` whose search document matches the FTS5 query bound as
/// `:match`, best match by `rank` first and then in list order, each with its snippet.
pub(crate) fn load_tasks_found(
    cx: &Ctx<'_>,
    rank: &str,
    filter: &str,
    extra: &NamedParams<'_>,
    limit: usize,
) -> Result<Vec<Task>> {
    let sql = format!(
        "SELECT {TASK_COLUMNS}, \
                snippet(task_search, 2, char(1), char(2), '…', 12), \
                snippet(task_search, 3, char(1), char(2), '…', 12) \
         FROM task_search \
         JOIN task_search_docs d ON d.doc = task_search.rowid \
         JOIN tasks t ON t.id = d.task \
         WHERE task_search MATCH :match AND {filter} \
         ORDER BY {rank}, {LIST_ORDER} LIMIT :limit"
    );
    query_tasks(cx, &sql, filter, extra, Some(limit), found_task_row)
}

fn query_tasks(
    cx: &Ctx<'_>,
    sql: &str,
    filter: &str,
    extra: &NamedParams<'_>,
    limit: Option<usize>,
    row: fn(&Row<'_>) -> rusqlite::Result<Task>,
) -> Result<Vec<Task>> {
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
    let mut stmt = cx.db.prepare_cached(sql)?;
    let tasks = stmt
        .query_map(params.as_slice(), row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    complete_tasks(cx, tasks)
}

/// The blockers of a batch of tasks, in blocker order. Every batch query binds its ids as
/// one JSON array read with `json_each`, so a batch is one statement whatever its size.
pub(crate) const BATCH_BLOCKERS: &str = "SELECT b.task, b.blocker, bt.status \
     FROM task_blockers b JOIN tasks bt ON bt.id = b.blocker \
     WHERE b.task IN (SELECT value FROM json_each(?1)) ORDER BY bt.number, bt.board";

/// How many children each of a batch of tasks has, along the `tasks_parent` index.
pub(crate) const BATCH_CHILD_COUNTS: &str = "SELECT parent, COUNT(*) FROM tasks \
     WHERE parent IN (SELECT value FROM json_each(?1)) GROUP BY parent";

/// Fills in blockers, child counts, `owner_stale`, `ready` and feedback, with one query for
/// each across all of `tasks`.
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

    let child_counts: HashMap<String, u32> = cx
        .db
        .prepare_cached(BATCH_CHILD_COUNTS)?
        .query_map([&ids], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let with_feedback: HashSet<String> = cx
        .db
        .prepare_cached(
            "SELECT task FROM feedback WHERE task IN (SELECT value FROM json_each(?1))",
        )?
        .query_map([&ids], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    for task in &mut tasks {
        task.child_count = child_counts.get(task.id.as_str()).copied().unwrap_or(0);
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
        task.proposal = board_proto::is_pending_proposal(
            task.status,
            task.auto,
            &task.body,
            with_feedback.contains(task.id.as_str()),
        );
        task.ready = task.status == TaskStatus::Open
            && all_done
            && !owner_live
            && !waiting
            && !task.proposal;
        if with_feedback.contains(task.id.as_str()) {
            task.feedback = crate::ops::feedback::load(cx, &task.id)?;
        }
    }
    Ok(tasks)
}

/// Ids as one JSON array, for a batch query.
pub(crate) fn json_ids<'a>(ids: impl Iterator<Item = &'a str>) -> String {
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
    // Children with their statuses, loaded and completed as one batch; by id number.
    let mut child_tasks = load_tasks(cx, "t.parent = :parent", &vec![(":parent", &id.as_str())])?;
    child_tasks.sort_by_key(|t| t.id.number());
    let children = child_tasks.iter().map(|t| t.id.clone()).collect();
    let child_tasks = child_tasks
        .into_iter()
        .map(|t| ChildTask {
            id: t.id,
            title: t.title,
            status: t.status,
            ready: t.ready,
            owner: t.owner,
            owner_stale: t.owner_stale,
        })
        .collect();
    // The newest events, oldest first; a cut history no longer starts with task_created.
    let mut history = latest_events(
        cx,
        "task = :task",
        &vec![(":task", &id.as_str())],
        0,
        TASK_HISTORY_LIMIT,
    )?;
    history.reverse();
    let named: HashSet<&str> = task
        .owner
        .iter()
        .chain([&task.created_by])
        .chain(notes.iter().map(|n| &n.agent))
        .chain(evidence.iter().map(|e| &e.agent))
        .map(AgentId::as_str)
        .chain(history.iter().map(|e| e.actor.as_str()))
        .chain(
            task.feedback
                .iter()
                .filter_map(|f| f.resolution.as_ref())
                .map(|r| r.by.as_str()),
        )
        .collect();
    let mut handles = BTreeMap::new();
    for id in named
        .into_iter()
        .filter(|id| !matches!(*id, "user" | "server"))
    {
        let handle = handle_of(cx, id)?;
        if handle != id {
            handles.insert(id.to_owned(), handle);
        }
    }
    Ok(TaskDetail {
        task,
        notes,
        evidence,
        blocks,
        children,
        child_tasks,
        history,
        handles,
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
    let ready_auto;
    (counts.ready, ready_auto) = cx.db.query_row(
        &format!(
            "SELECT COUNT(*), COALESCE(SUM(t.auto = 1), 0) FROM tasks t \
             WHERE t.board = :board AND {}",
            ready_sql("t")
        ),
        named_params! {
            ":board": board.as_str(),
            ":now": cx.now_ms(),
            ":cutoff": cx.stale_cutoff(),
        },
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    counts.ready_auto = Some(ready_auto);
    Ok(counts)
}

pub(crate) fn find_board(cx: &Ctx<'_>, slug: &BoardSlug) -> Result<Option<Board>> {
    let row = cx
        .db
        .query_row(
            "SELECT slug, title, goal, status, repos, autopilot, home_devices, coordinator, \
             coordinator_expires_at, created_at, updated_at, temporary, parallel_tasks, dispatch, \
             usage_stop_at FROM boards WHERE slug = ?1",
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
                    parallel_tasks: r.get(12)?,
                    usage_stop_at: r.get(14)?,
                    coordinator: match (coordinator, expires) {
                        (Some(c), Some(e)) if e > cx.now_ms() => Some(parse_id(&c)?),
                        _ => None,
                    },
                    dispatch: parse_enum(&r.get::<_, String>(13)?)?,
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

/// The agent `id` names: itself, or the agent whose short handle it is. Clients print
/// handles, so an agent may pass one back where an id is expected.
pub(crate) fn agent_by_id_or_handle(cx: &Ctx<'_>, id: &AgentId) -> Result<AgentId> {
    let by_handle: Option<String> = cx
        .db
        .query_row(
            "SELECT id FROM agents WHERE handle = ?1 \
             AND NOT EXISTS (SELECT 1 FROM agents WHERE id = ?1)",
            [id.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match by_handle {
        Some(found) => parse_id(&found)?,
        None => id.clone(),
    })
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
    handoff.last_change_at = Some(last_change(cx, agent, handoff.updated_at)?);
    Ok(Some(handoff))
}

/// How long file edits may follow a handoff before it is stale: a handoff is written at
/// the end of a step, and the edits of the next step do not make it wrong at once.
pub(crate) fn handoff_files_grace() -> chrono::TimeDelta {
    chrono::TimeDelta::minutes(30)
}

/// When the agent last made a change its handoff must cover, among those after the
/// handoff: a claim, release, take or completion of a task, an update of a task that is
/// or was its own, or a file touch more than [`handoff_files_grace`] after the handoff.
/// A note, a new task, a lease, an ask or an update of another agent's task adds to the
/// board and leaves the handoff as true as it was. Without such a change, the handoff's
/// own time. Reads only the agent's events newer than the handoff.
fn last_change(cx: &Ctx<'_>, agent: &AgentId, handoff_at: Timestamp) -> Result<Timestamp> {
    let mut stmt = cx.db.prepare_cached(
        "SELECT e.at, e.type, \
                json_extract(e.payload, '$.previous_owner') = ?1 \
                OR EXISTS (SELECT 1 FROM tasks t WHERE t.id = e.task AND t.owner = ?1) \
         FROM events e WHERE e.actor = ?1 ORDER BY e.seq DESC",
    )?;
    let mut rows = stmt.query([agent.as_str()])?;
    let touch_from = handoff_at + handoff_files_grace();
    while let Some(row) = rows.next()? {
        let at = ts(row.get(0)?);
        if at <= handoff_at {
            break;
        }
        let covers = match parse_enum::<EventType>(&row.get::<_, String>(1)?)? {
            EventType::TaskClaimed
            | EventType::TaskReleased
            | EventType::TaskTaken
            | EventType::TaskDone => true,
            EventType::TaskUpdated => row.get::<_, Option<bool>>(2)?.unwrap_or(false),
            EventType::FilesTouched => at > touch_from,
            _ => false,
        };
        if covers {
            return Ok(at);
        }
    }
    Ok(handoff_at)
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

/// The newest `limit` asks that pass `filter`, oldest first like [`load_asks`].
pub(crate) fn load_newest_asks(
    cx: &Ctx<'_>,
    filter: &str,
    extra: &NamedParams<'_>,
    limit: u32,
) -> Result<Vec<Ask>> {
    let mut params = extra.clone();
    params.push((":limit", &limit));
    let mut asks = cx
        .db
        .prepare_cached(&format!(
            "SELECT {ASK_COLUMNS} FROM asks WHERE {filter} ORDER BY id DESC LIMIT :limit"
        ))?
        .query_map(params.as_slice(), ask_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    asks.reverse();
    Ok(asks)
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

/// The columns [`event_row`] reads, in its order.
pub(crate) const EVENT_COLUMNS: &str = "seq, at, board, actor, type, task, payload";

pub(crate) fn event_row(r: &Row<'_>) -> rusqlite::Result<Event> {
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
            "SELECT {EVENT_COLUMNS} FROM events \
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
        "SELECT {EVENT_COLUMNS} FROM events \
         WHERE seq > :since AND {filter} ORDER BY seq DESC LIMIT :limit"
    )
}
