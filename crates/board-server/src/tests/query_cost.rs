//! Query cost: the plans of the snapshot's hot queries, and a timed snapshot over a large
//! board (ignored by default: run it in a release build).

use std::time::{Duration, Instant};

use board_proto::{AgentId, BoardSlug};
use serde_json::{Value, json};

use super::{REPO, T};
use crate::db::{live_sql, ready_sql};
use crate::ops::leases::DELEGATION_FAMILY;
use crate::snapshot::{RECENT_OFF_BOARD, RECENT_ON_BOARD, RECENT_SCAN, recent_candidates};
use crate::store::{BATCH_BLOCKERS, latest_events_sql};

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

async fn full(t: &T) -> String {
    t.ok("GET", "/v1/snapshot?kind=full", Some(ME), None).await["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A full snapshot over 5k tasks and 100k events stays under 50 ms. Timing depends on the
/// machine, so run it on demand in a release build:
/// `cargo test -p board-server --release -- --ignored --nocapture snapshot_stays_fast`.
#[tokio::test]
#[ignore = "timing; run in a release build"]
async fn snapshot_stays_fast_on_a_big_board() {
    // On disk in WAL mode, as in production. `BOARD_BENCH_DB` keeps the file for a look at
    // its query plans.
    let dir = tempfile::tempdir().unwrap();
    let path =
        std::env::var_os("BOARD_BENCH_DB").map_or_else(|| dir.path().join("board.db"), Into::into);
    let t = T::over(crate::Db::open(&path).unwrap(), |_| {});
    big_board(&t, 5_000, 100_000).await;
    let first = Instant::now();
    full(&t).await;
    let first = first.elapsed();
    let mut runs: Vec<Duration> = Vec::new();
    for _ in 0..30 {
        let at = Instant::now();
        full(&t).await;
        runs.push(at.elapsed());
    }
    runs.sort();
    let median = runs[runs.len() / 2];
    let max = runs[runs.len() - 1];
    println!("full snapshot: first {first:?}, median {median:?}, max {max:?}");
    assert!(median < Duration::from_millis(50), "median {median:?}");
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

    // Blockers of a batch: a primary-key search per task, no scan of task_blockers. Only
    // the batch's own blocker rows are sorted.
    let p = plan(&t, BATCH_BLOCKERS);
    assert!(p.contains("SEARCH b USING COVERING INDEX"), "{p}");
    assert!(!p.contains("SCAN b"), "{p}");

    // A task's children, as deleting a task or a board looks them up.
    let p = plan(&t, "SELECT id FROM tasks WHERE parent = ?1");
    assert!(p.contains("USING INDEX tasks_parent"), "{p}");

    // Delegated threads: the expression index, not a scan of agents per step.
    let p = plan(&t, DELEGATION_FAMILY);
    assert!(p.contains("USING INDEX agents_t3_parent"), "{p}");
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
