//! Query cost: the plans of the hot queries, the statements a request runs, and a timed
//! snapshot over a large board (ignored by default: run it in a release build).

use std::time::{Duration, Instant};

use board_proto::{AgentId, BoardSlug};
use serde_json::{Value, json};

use super::{REPO, T};
use crate::db::{live_sql, ready_sql};
use crate::ops::asks::BY_TELEGRAM_MESSAGE;
use crate::ops::feedback::{CLUSTER, CLUSTER_ERRORS, CLUSTER_REPORTERS};
use crate::ops::leases::DELEGATION_FAMILY;
use crate::ops::tasks::{
    AWAY_SINCE, DELEGATE_AT_WORK, DELEGATED_BY, ENTERED_DOING, LAST_RUN, TOLD,
};
use crate::snapshot::{
    ALERT_EVENTS, ALERTS_LEFT, RECENT_OFF_BOARD, RECENT_ON_BOARD, RECENT_SCAN, recent_candidates,
};
use crate::store::{BATCH_BLOCKERS, BATCH_CHILD_COUNTS, latest_events_sql};

const ME: &str = "t3:me";
const PEERS: usize = 20;

fn peer(n: usize) -> String {
    format!("t3:peer{n}")
}

/// A board with `tasks` tasks and `events` events, in the shape a busy board has: a
/// quarter of the tasks open, some blocked, some with parents, some owned by live peers;
/// events split between this board, another board and no board (edits and leases).
async fn big_board(t: &T, tasks: u32, events: u32) {
    t.board("app", &[REPO]).await;
    t.board("other", &["github.com/example/other"]).await;
    t.register(ME, Some(REPO), Some("main")).await;
    for n in 0..PEERS {
        let id = peer(n);
        t.ok(
            "POST",
            "/v1/agents",
            Some(&id),
            Some(json!({
                "id": id, "runtime": "claude", "device": "mac", "repo": REPO,
                "branch": "main",
                "native_ids": {"t3_thread": format!("peer{n}"), "t3_parent": "me"},
            })),
        )
        .await;
    }
    // An owner gone stale: its open tasks are not ready.
    t.register("t3:gone", Some(REPO), Some("main")).await;
    let now = t.state.now().timestamp_millis();
    t.exec("UPDATE agents SET last_seen = 0 WHERE id = 't3:gone'");
    t.exec(&format!(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {tasks})
         INSERT INTO tasks (id, board, number, parent, title, body, acceptance, status, priority,
                            owner, auto, created_by, version, done_at, created_at, updated_at)
         SELECT 'app-' || i, 'app', i,
                CASE WHEN i % 3 = 0 AND i > 10 THEN 'app-' || (i / 10) END,
                'Task number ' || i, 'Body of task ' || i, 'check ' || i,
                CASE i % 4 WHEN 0 THEN 'open' WHEN 1 THEN 'done' WHEN 2 THEN 'done'
                     ELSE CASE WHEN i % 40 = 3 THEN 'doing' ELSE 'dropped' END END,
                'p' || (i % 3 + 1),
                CASE WHEN i % 40 = 3 THEN 't3:peer' || (i % {PEERS})
                     WHEN i % 4 = 0 AND i % 12 = 0 THEN 't3:gone' END,
                1, '{ME}', 1,
                CASE WHEN i % 4 IN (1, 2) THEN {now} - i * 1000 END,
                {now} - i * 2000, {now} - i * 1000
         FROM n;
         UPDATE boards SET next_number = {tasks} + 1 WHERE slug = 'app';
         INSERT INTO task_blockers (task, blocker)
         SELECT id, 'app-' || (number - 1) FROM tasks WHERE status = 'open' AND number % 8 = 0;
         INSERT INTO task_blockers (task, blocker)
         SELECT id, 'app-' || (number - 3) FROM tasks WHERE status = 'open' AND number % 16 = 0;
         -- A blocker on another board, ordered by its number among the others.
         INSERT INTO tasks (id, board, number, title, body, acceptance, status, priority, auto,
                            created_by, version, created_at, updated_at)
         VALUES ('other-1', 'other', 1, 'Elsewhere', '', '', 'open', 'p2', 1, '{ME}', 1,
                 {now}, {now});
         UPDATE boards SET next_number = 2 WHERE slug = 'other';
         INSERT INTO task_blockers (task, blocker)
         SELECT id, 'other-1' FROM tasks WHERE status = 'open' AND number % 32 = 0;
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {events})
         INSERT INTO events (at, board, actor, type, task, payload)
         SELECT {now} - ({events} - i) * 100,
                CASE i % 3 WHEN 0 THEN NULL WHEN 1 THEN 'other' ELSE 'app' END,
                't3:peer' || (i % {PEERS}),
                CASE i % 3 WHEN 0 THEN 'files_touched' ELSE 'note_added' END,
                CASE i % 3 WHEN 0 THEN NULL ELSE 'app-' || (i % {tasks} + 1) END,
                CASE i % 3 WHEN 0 THEN json_object('repo', '{REPO}',
                                                   'paths', json_array('src/f' || i || '.rs'))
                     ELSE '{{}}' END
         FROM n;"
    ));
}

/// How long a full snapshot that scans alerts from `since=0` takes.
async fn time_full(t: &T) -> Duration {
    let at = Instant::now();
    let snap = t
        .ok("GET", "/v1/snapshot?kind=full&since=0", Some(ME), None)
        .await;
    let took = at.elapsed();
    assert!(!snap["text"].as_str().unwrap().is_empty());
    took
}

/// A full snapshot over 5k tasks and 100k events stays under 50 ms. Timing depends on the
/// machine, so run it on demand in a release build:
/// `cargo test -p board-server --release -- --ignored --nocapture snapshot_stays_fast`.
#[tokio::test]
#[ignore = "timing; run in a release build"]
async fn snapshot_stays_fast_on_a_big_board() {
    time_full_snapshots(100_000, Duration::from_millis(50)).await;
}

/// A year of history: a full snapshot over 1M events stays under 20 ms, as the alert scan
/// reads only the seq range of its window, not each watched type's whole history.
#[tokio::test]
#[ignore = "timing; run in a release build"]
async fn snapshot_stays_fast_over_a_million_events() {
    time_full_snapshots(1_000_000, Duration::from_millis(20)).await;
}

/// Times full snapshots over 5k tasks and `events` events against `budget` (the median).
/// Each one scans alerts from `since=0`, so the whole alert window is read every time.
async fn time_full_snapshots(events: u32, budget: Duration) {
    // On disk in WAL mode, as in production. `BOARD_BENCH_DB` keeps the file for a look at
    // its query plans.
    let dir = tempfile::tempdir().unwrap();
    let path =
        std::env::var_os("BOARD_BENCH_DB").map_or_else(|| dir.path().join("board.db"), Into::into);
    let t = T::over(crate::Db::open(&path).unwrap(), |_| {});
    let built = Instant::now();
    big_board(&t, 5_000, events).await;
    let built = built.elapsed();
    let first = time_full(&t).await;
    let mut runs: Vec<Duration> = Vec::new();
    for _ in 0..30 {
        runs.push(time_full(&t).await);
    }
    runs.sort();
    let median = runs[runs.len() / 2];
    let max = runs[runs.len() - 1];
    println!(
        "{events} events (built in {built:?}): full snapshot first {first:?}, \
         median {median:?}, max {max:?}"
    );
    assert!(median < budget, "median {median:?}");
}

/// The query plan of `sql`, one step per line.
fn plan(t: &T, sql: &str) -> String {
    t.state
        .db()
        .read(t.state.now(), t.state.settings(), |cx| {
            let mut stmt = cx.db.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
            let mut rows = stmt.raw_query();
            let mut steps = Vec::new();
            while let Some(r) = rows.next()? {
                steps.push(r.get::<_, String>(3)?);
            }
            Ok(steps.join("\n"))
        })
        .unwrap()
}

#[tokio::test]
async fn hot_queries_search_indexes_instead_of_scanning_or_sorting() {
    let t = T::new();
    big_board(&t, 300, 3_000).await;

    // recent(): each half reads `events_board` backwards; together they needed a sort.
    for filter in [RECENT_ON_BOARD, RECENT_OFF_BOARD] {
        let p = plan(&t, &latest_events_sql(filter));
        assert!(p.contains("USING INDEX events_board"), "{filter}:\n{p}");
        assert!(!p.contains("TEMP B-TREE"), "{filter}:\n{p}");
    }
    let both = plan(
        &t,
        &latest_events_sql("seq <= :seq AND (board = :board OR board IS NULL) AND actor != :me"),
    );
    assert!(both.contains("TEMP B-TREE"), "{both}");

    // Alerts: the seq range between the cursor (or the window's start) and `seq`, not each
    // watched type's whole history along `events_type`.
    for sql in [&*ALERT_EVENTS, &*ALERTS_LEFT] {
        let p = plan(&t, sql);
        assert!(
            p.starts_with("SEARCH events USING INTEGER PRIMARY KEY (rowid>? AND rowid<?)"),
            "{p}"
        );
        assert!(!p.contains("events_type"), "{p}");
    }

    // Blockers of a batch: a primary-key search per task, no scan of task_blockers. Only
    // the batch's own blocker rows are sorted.
    let p = plan(&t, BATCH_BLOCKERS);
    assert!(p.contains("SEARCH b USING COVERING INDEX"), "{p}");
    assert!(!p.contains("SCAN b"), "{p}");

    // A feedback item's duplicates, which every read of the item counts: along the
    // partial index, not a scan of the feedback board's items per item.
    for sql in [CLUSTER, CLUSTER_REPORTERS, CLUSTER_ERRORS] {
        let p = plan(&t, sql);
        assert!(p.contains("USING INDEX feedback_duplicates"), "{sql}:\n{p}");
        assert!(
            !p.contains("SCAN f\n") && !p.ends_with("SCAN f"),
            "{sql}:\n{p}"
        );
    }
    for sql in [CLUSTER_REPORTERS, CLUSTER_ERRORS] {
        let p = plan(&t, sql);
        assert!(
            p.contains("USING INDEX feedback_reports_task"),
            "{sql}:\n{p}"
        );
    }

    // A task's children, as deleting a task or a board looks them up.
    let p = plan(&t, "SELECT id FROM tasks WHERE parent = ?1");
    assert!(p.contains("USING INDEX tasks_parent"), "{p}");

    // Child counts of a batch, as every task read fills them in: along the same index, no
    // scan of tasks.
    let p = plan(&t, BATCH_CHILD_COUNTS);
    assert!(p.contains("USING COVERING INDEX tasks_parent"), "{p}");
    assert!(!p.contains("SCAN tasks"), "{p}");

    // Delegated threads: the expression index, not a scan of agents per step.
    for sql in [DELEGATION_FAMILY, DELEGATE_AT_WORK] {
        let p = plan(&t, sql);
        assert!(p.contains("USING INDEX agents_t3_parent"), "{p}");
    }

    // Who delegated a task's owner, as a stand-in's request checks: up the chain one agent
    // at a time by primary key, no scan of agents.
    let p = plan(&t, DELEGATED_BY);
    assert!(
        p.contains("SEARCH a USING INDEX sqlite_autoindex_agents_1 (id=?)"),
        "{p}"
    );
    assert!(!p.contains("SCAN a"), "{p}");

    // The idle-owner sweep: an owner's own events along `events_actor`, read backwards for
    // its last run; a task's along `events_task`. No scan of events, no sort.
    for (sql, index) in [
        (LAST_RUN, "events_actor"),
        (AWAY_SINCE, "events_actor"),
        (ENTERED_DOING, "events_task"),
        (TOLD, "events_task"),
    ] {
        let p = plan(&t, sql);
        assert!(p.contains(&format!("USING INDEX {index}")), "{sql}:\n{p}");
        assert!(!p.contains("SCAN events"), "{sql}:\n{p}");
        assert!(!p.contains("TEMP B-TREE"), "{sql}:\n{p}");
    }
}

/// A reply from Telegram finds its ask by chat and message along an index, not by scanning
/// every ask.
#[tokio::test]
async fn a_telegram_reply_searches_asks_by_chat_and_message() {
    let t = T::new();
    let p = plan(&t, BY_TELEGRAM_MESSAGE);
    assert!(p.contains("INDEX asks_telegram_message"), "{p}");
    assert!(!p.contains("SCAN"), "{p}");
}

/// touch() finds the caller's device peers along `agents_device`, not by scanning every
/// agent ever registered, once per touched path.
#[tokio::test]
async fn touch_peer_lookups_search_agents_by_device() {
    use crate::ops::leases::{OVERLAP_TOUCHES, PEER_SCANS_DROP, PEER_WROTE};
    let t = T::new();
    big_board(&t, 30, 30).await;
    for sql in [&*PEER_WROTE, &*PEER_SCANS_DROP, &*OVERLAP_TOUCHES] {
        let p = plan(&t, sql);
        assert!(
            p.contains("SEARCH agents USING INDEX agents_device"),
            "{sql}:\n{p}"
        );
        assert!(!p.contains("SCAN agents"), "{sql}:\n{p}");
    }
}

/// The two-query recent() picks the same events, in the same order, as the one query it
/// replaced.
#[tokio::test]
async fn recent_candidates_match_the_single_query() {
    let t = T::new();
    big_board(&t, 300, 3_000).await;
    // Some events by the agent itself, which recent() leaves out.
    t.exec("UPDATE events SET actor = 't3:me' WHERE seq % 7 = 0");
    let me: AgentId = ME.parse().unwrap();
    let board: BoardSlug = "app".parse().unwrap();
    let max = t.count("SELECT max(seq) FROM events");
    for seq in [max, max / 2, 150, 5] {
        let (got, want) = t
            .state
            .db()
            .read(t.state.now(), t.state.settings(), |cx| {
                let got: Vec<i64> = recent_candidates(cx, &me, &board, seq)?
                    .iter()
                    .map(|e| e.seq)
                    .collect();
                let want: Vec<i64> = cx
                    .db
                    .prepare(
                        "SELECT seq FROM events WHERE seq > 0 AND seq <= ?1 \
                         AND (board = ?2 OR board IS NULL) AND actor != ?3 \
                         ORDER BY seq DESC LIMIT ?4",
                    )?
                    .query_map(rusqlite::params![seq, "app", ME, RECENT_SCAN], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok((got, want))
            })
            .unwrap();
        assert!(!want.is_empty() || seq < 5, "seq {seq}");
        assert_eq!(got, want, "seq {seq}");
    }
}

async fn list(t: &T, query: &str) -> Vec<Value> {
    t.ok("GET", &format!("/v1/tasks?{query}"), Some(ME), None)
        .await
        .as_array()
        .unwrap()
        .clone()
}

fn ids(tasks: &[Value]) -> Vec<&str> {
    tasks.iter().map(|t| t["id"].as_str().unwrap()).collect()
}

/// Tasks loaded in one batch carry the blockers, staleness and readiness each one's own
/// queries give.
#[tokio::test]
async fn batched_tasks_match_their_own_queries() {
    let t = T::new();
    big_board(&t, 600, 100).await;
    let tasks = list(&t, "board=app&limit=1000").await;
    assert_eq!(tasks.len(), 600);
    let now = t.state.now().timestamp_millis();
    let cutoff = now - t.state.settings().stale_ms();
    let (mut blocked, mut stale, mut ready) = (0, 0, 0);
    for task in &tasks {
        let id = task["id"].as_str().unwrap();
        let blockers = t.strings(&format!(
            "SELECT b.blocker FROM task_blockers b JOIN tasks bt ON bt.id = b.blocker \
             WHERE b.task = '{id}' ORDER BY bt.number, bt.board"
        ));
        assert_eq!(task["blockers"], json!(blockers), "{id}");
        let one = |test: String| {
            let sql = format!("SELECT count(*) FROM tasks t WHERE t.id = '{id}' AND {test}")
                .replace(":now", &now.to_string())
                .replace(":cutoff", &cutoff.to_string());
            t.count(&sql) == 1
        };
        let owner_stale = one(format!(
            "t.owner IS NOT NULL AND NOT {}",
            live_sql("t.owner")
        ));
        assert_eq!(task["owner_stale"], json!(owner_stale), "{id}");
        assert_eq!(task["ready"], json!(one(ready_sql("t"))), "{id}");
        blocked += usize::from(!blockers.is_empty());
        stale += usize::from(owner_stale);
        ready += usize::from(task["ready"] == json!(true));
    }
    // The fixture covers every case.
    assert!(
        blocked > 10 && stale > 10 && ready > 10,
        "{blocked} {stale} {ready}"
    );
    // A blocker on another board sorts by its number.
    assert_eq!(
        t.get_task("app-32").await["blockers"],
        json!(["other-1", "app-29", "app-31"])
    );
}

/// `ready` and `limit` filtered in SQL give the tasks the full list filtered after loading
/// gives, in the same order.
#[tokio::test]
async fn ready_filter_and_limit_match_the_full_list() {
    let t = T::new();
    big_board(&t, 600, 100).await;
    let all = list(&t, "board=app&limit=1000").await;
    for ready in [true, false] {
        let want: Vec<&str> = ids(&all)
            .into_iter()
            .zip(&all)
            .filter(|(_, t)| t["ready"] == json!(ready))
            .map(|(id, _)| id)
            .collect();
        for limit in [1, 7, 1000] {
            let got = list(&t, &format!("board=app&ready={ready}&limit={limit}")).await;
            let n = limit.min(want.len());
            assert_eq!(ids(&got), want[..n], "ready={ready} limit={limit}");
        }
    }
    let open = list(&t, "board=app&status=open&ready=false&limit=1000").await;
    assert!(!open.is_empty());
    assert!(
        open.iter()
            .all(|t| t["status"] == json!("open") && t["ready"] == json!(false))
    );
}

/// What `request` answers, and the SQL of each statement the database ran for it.
async fn statements<R>(t: &T, request: impl Future<Output = R>) -> (R, Vec<String>) {
    t.state.db().record_statements();
    let response = request.await;
    (response, t.state.db().recorded_statements())
}

/// How many of the statements in `run` contain `part`.
fn running(run: &[String], part: &str) -> usize {
    run.iter().filter(|sql| sql.contains(part)).count()
}

/// A board `slug` with `tasks` tasks in every state its counts tell apart: each status, and
/// open tasks that are ready (auto or not), owned by a live agent, blocked or not yet due.
async fn counted_board(t: &T, slug: &str, tasks: u32) {
    t.board(slug, &[&format!("github.com/example/{slug}")])
        .await;
    let now = t.state.now().timestamp_millis();
    t.exec(&format!(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {tasks})
         INSERT INTO tasks (id, board, number, title, body, acceptance, status, priority, owner,
                            auto, not_before, created_by, version, done_at, created_at, updated_at)
         SELECT '{slug}-' || i, '{slug}', i, 'Task ' || i, '', '',
                CASE i % 7 WHEN 0 THEN 'doing' WHEN 1 THEN 'blocked' WHEN 2 THEN 'review'
                           WHEN 3 THEN 'done' WHEN 4 THEN 'dropped' ELSE 'open' END,
                'p2',
                CASE WHEN i % 7 = 0 OR i % 14 = 6 THEN '{ME}' WHEN i % 14 = 5 THEN 't3:gone' END,
                i % 3 > 0,
                CASE WHEN i % 21 = 5 THEN {now} + 60000 END,
                '{ME}', 1, CASE WHEN i % 7 = 3 THEN {now} END, {now}, {now}
         FROM n;
         UPDATE boards SET next_number = {tasks} + 1 WHERE slug = '{slug}';
         INSERT INTO task_blockers (task, blocker)
         SELECT id, '{slug}-1' FROM tasks
         WHERE board = '{slug}' AND status = 'open' AND number % 4 = 0;"
    ));
}

/// The list of boards runs the same statements however many boards there are, and gives
/// each board the row, the counts and the open asks its own read gives.
#[tokio::test]
async fn listing_boards_costs_the_same_however_many_there_are() {
    use crate::store::{ALL_OPEN_ASKS, ALL_STATUS_COUNTS, all_ready_counts_sql};
    let t = T::new();
    big_board(&t, 60, 60).await;
    let (_, few) = statements(&t, t.ok("GET", "/v1/boards", Some(ME), None)).await;
    for n in 0..6 {
        let slug = format!("extra{n}");
        counted_board(&t, &slug, 20 + 9 * n).await;
        for ask in 0..n % 3 {
            t.ok(
                "POST",
                "/v1/asks",
                Some(ME),
                Some(json!({
                    "agent": ME, "board": slug, "kind": "decision",
                    "question": format!("Question {ask}?"), "options": ["yes", "no"],
                })),
            )
            .await;
        }
    }
    t.board("empty", &["github.com/example/empty"]).await;
    let (listed, many) = statements(&t, t.ok("GET", "/v1/boards", Some(ME), None)).await;
    assert_eq!(many.len(), few.len(), "{many:#?}");

    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 9);
    let (mut ready, mut held, mut asked) = (0, 0, 0);
    for board in listed {
        let slug = board["slug"].as_str().unwrap();
        let own = t
            .ok("GET", &format!("/v1/boards/{slug}"), Some(ME), None)
            .await;
        assert_eq!(*board, own, "{slug}");
        let counts = &board["counts"];
        ready += usize::from(counts["ready"].as_u64() > Some(0));
        held += usize::from(counts["ready"].as_u64() > counts["ready_auto"].as_u64());
        asked += usize::from(board["open_asks"].as_u64() > Some(0));
    }
    // The fixture covers every case: ready tasks, some of them not auto, open asks, and a
    // board with no task, whose ready counts are zero and not unknown.
    assert!(ready > 5 && held > 3 && asked > 2, "{ready} {held} {asked}");
    let empty = listed.iter().find(|b| b["slug"] == "empty").unwrap();
    assert_eq!(
        empty["counts"],
        json!({
            "open": 0, "doing": 0, "blocked": 0, "review": 0, "done": 0, "dropped": 0,
            "ready": 0, "ready_auto": 0,
        })
    );

    // Each board's open tasks are searched along `tasks_board`, as one board's count does,
    // and the other two counts read an index in its own order: no scan of tasks, no sort.
    let p = plan(&t, &all_ready_counts_sql());
    assert!(
        p.contains("SEARCH t USING INDEX tasks_board (board=? AND status=?)"),
        "{p}"
    );
    assert!(!p.contains("SCAN t") && !p.contains("TEMP B-TREE"), "{p}");
    let p = plan(&t, ALL_STATUS_COUNTS);
    assert_eq!(p, "SCAN tasks USING COVERING INDEX tasks_board");
    let p = plan(&t, ALL_OPEN_ASKS);
    assert_eq!(p, "SEARCH asks USING COVERING INDEX asks_status (status=?)");
}

/// The list of agents reads every agent's boards in one query, and gives each agent the
/// boards its own read gives, in the same order.
#[tokio::test]
async fn listing_agents_reads_their_boards_in_one_query() {
    use crate::store::BATCH_AGENT_BOARDS;
    let t = T::new();
    big_board(&t, 30, 30).await;
    t.board("zeta", &["github.com/example/zeta"]).await;
    // An agent on no repository is on no board.
    t.register("t3:nowhere", None, None).await;
    for n in 0..PEERS {
        // No board, one, two or three, joined in an order that is not the listed one.
        for slug in &["zeta", "app", "other"][..n % 4] {
            let agent = peer(n);
            t.ok(
                "POST",
                &format!("/v1/boards/{slug}/join"),
                Some(&agent),
                Some(json!({ "agent": agent })),
            )
            .await;
        }
    }
    let (agents, run) = statements(&t, t.ok("GET", "/v1/agents", Some(ME), None)).await;
    assert_eq!(running(&run, "FROM board_members"), 1, "{run:#?}");

    let agents = agents.as_array().unwrap();
    assert_eq!(agents.len(), PEERS + 3);
    let mut sizes = [0; 4];
    for agent in agents {
        let id = agent["id"].as_str().unwrap();
        let own = t
            .ok("GET", &format!("/v1/agents/{id}"), Some(ME), None)
            .await;
        assert_eq!(*agent, own, "{id}");
        let boards = t.strings(&format!(
            "SELECT board FROM board_members WHERE agent = '{id}' ORDER BY board"
        ));
        assert_eq!(agent["boards"], json!(boards), "{id}");
        sizes[boards.len()] += 1;
    }
    assert!(sizes.iter().all(|&n| n > 0), "{sizes:?}");

    // A search per agent along `board_members_agent`, no scan of the memberships.
    let p = plan(&t, BATCH_AGENT_BOARDS);
    assert!(
        p.contains("SEARCH board_members USING INDEX board_members_agent (agent=?)"),
        "{p}"
    );
    assert!(!p.contains("SCAN board_members"), "{p}");
}

/// A request that only needs a board to exist, or reads its settings, does not count the
/// board's tasks and asks; one that answers with the board counts them once.
#[tokio::test]
async fn a_request_that_shows_no_board_counts_counts_nothing() {
    let t = T::new();
    big_board(&t, 60, 60).await;
    // The three statements that count a board, as `get_board` runs them.
    let counted = |run: &[String]| {
        [
            running(run, "FROM tasks WHERE board = ?1 GROUP BY status"),
            running(run, "COALESCE(SUM(t.auto = 1), 0) FROM tasks t"),
            running(run, "FROM asks WHERE board = ?1 AND status = 'open'"),
        ]
    };
    let create = json!({ "agent": ME, "board": "app", "title": "One more" });
    let (_, run) = statements(&t, t.ok("POST", "/v1/tasks", Some(ME), Some(create))).await;
    assert_eq!(counted(&run), [0, 0, 0], "{run:#?}");
    let ask = json!({
        "agent": ME, "board": "app", "kind": "decision", "question": "Which one?",
        "options": ["yes", "no"],
    });
    let (_, run) = statements(&t, t.ok("POST", "/v1/asks", Some(ME), Some(ask))).await;
    assert_eq!(counted(&run), [0, 0, 0], "{run:#?}");
    let join = json!({ "agent": peer(0) });
    let (board, run) = statements(
        &t,
        t.ok("POST", "/v1/boards/other/join", Some(&peer(0)), Some(join)),
    )
    .await;
    assert_eq!(counted(&run), [1, 1, 1], "{run:#?}");
    assert_eq!(board["counts"]["open"], json!(1));
}

/// Stats sum lease conflicts and touch overlaps in one scan of the window's lease events,
/// for the total and for every board, and give each board what its own scan gives.
#[tokio::test]
async fn stats_scan_lease_events_once_for_every_board() {
    use crate::ops::stats::CONFLICTS_BY_REPO;
    let t = T::new();
    big_board(&t, 30, 300).await;
    for n in 0..5 {
        counted_board(&t, &format!("extra{n}"), 10).await;
    }
    // A board on two repositories, one of them shared with `app`.
    t.board("both", &[REPO, "github.com/example/extra0"]).await;
    let now = t.state.now().timestamp_millis();
    let lease = |kind: &str, field: &str, repo: &str, n: u32| {
        t.exec(&format!(
            "INSERT INTO events (at, actor, type, payload) \
             VALUES ({now}, '{ME}', '{kind}', json_object('repo', {repo}, '{field}', {n}))"
        ));
    };
    lease("lease_created", "conflicts", &format!("'{REPO}'"), 3);
    lease("files_touched", "overlaps", &format!("'{REPO}'"), 4);
    lease(
        "lease_created",
        "conflicts",
        "'github.com/example/extra0'",
        5,
    );
    lease(
        "files_touched",
        "overlaps",
        "'github.com/example/extra3'",
        6,
    );
    // On no board's repository: one names none, one names a number.
    lease("lease_created", "conflicts", "NULL", 7);
    lease("files_touched", "overlaps", "8", 8);
    // Outside the window.
    t.exec(&format!(
        "INSERT INTO events (at, actor, type, payload) VALUES \
         (0, '{ME}', 'lease_created', json_object('repo', '{REPO}', 'conflicts', 100))"
    ));

    let (stats, run) = statements(&t, t.ok("GET", "/v1/stats?days=7", Some(ME), None)).await;
    assert_eq!(running(&run, "'$.overlaps'"), 1, "{run:#?}");
    assert_eq!(running(&run, "'$.conflicts'"), 1, "{run:#?}");
    assert_eq!(stats["lease_conflicts"], json!(33));
    let boards = stats["boards"].as_array().unwrap();
    assert_eq!(boards.len(), 8, "{boards:?}");
    let from = now - 7 * 24 * 3_600_000;
    for board in boards {
        let slug = board["board"].as_str().unwrap();
        // The scan each board ran for itself before they shared one.
        let own = t.count(&format!(
            "SELECT COALESCE(SUM(json_extract(payload, CASE type WHEN 'lease_created' \
             THEN '$.conflicts' ELSE '$.overlaps' END)), 0) FROM events \
             WHERE type IN ('lease_created', 'files_touched') AND at >= {from} \
             AND json_extract(payload, '$.repo') IN (SELECT value FROM json_each( \
                 (SELECT repos FROM boards WHERE slug = '{slug}')))"
        ));
        assert_eq!(board["lease_conflicts"], json!(own), "{slug}");
    }
    let conflicts = |slug: &str| {
        let board = boards.iter().find(|b| b["board"] == slug).unwrap();
        board["lease_conflicts"].as_u64().unwrap()
    };
    assert_eq!(
        ["app", "other", "extra0", "extra3", "both"].map(conflicts),
        [7, 0, 5, 6, 12]
    );

    // The window's lease events along `events_type`, never a scan of every event.
    let p = plan(&t, CONFLICTS_BY_REPO);
    assert!(
        p.contains("SEARCH events USING INDEX events_type (type=? AND at>?)"),
        "{p}"
    );
    assert!(!p.contains("SCAN events"), "{p}");
}
