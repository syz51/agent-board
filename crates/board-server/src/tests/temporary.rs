//! Temporary boards: the flag, the sweep that closes and deletes them, and deleting a board.

use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{REPO, T, check, defaults, stale_after};
use crate::TemporarySweep;

const A: &str = "t3:alpha";
const B: &str = "t3:beta";

fn close_after() -> TimeDelta {
    TimeDelta::seconds(i64::try_from(defaults().temporary_close_after_secs).unwrap())
}

fn delete_after() -> TimeDelta {
    TimeDelta::seconds(i64::try_from(defaults().temporary_delete_after_secs).unwrap())
}

async fn sweep(t: &T) -> TemporarySweep {
    crate::sweep_temporary(&t.state).await
}

fn slugs(list: &[board_proto::BoardSlug]) -> Vec<&str> {
    list.iter().map(|s| s.as_str()).collect()
}

async fn temporary_board(t: &T, slug: &str) -> Value {
    t.ok(
        "POST",
        "/v1/boards",
        None,
        Some(json!({ "slug": slug, "title": format!("Board {slug}"), "repos": [REPO], "temporary": true })),
    )
    .await
}

async fn status(t: &T, slug: &str) -> String {
    t.ok("GET", &format!("/v1/boards/{slug}"), None, None).await["status"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The newest event, as the API returns it.
async fn last_event(t: &T) -> Value {
    let page = t
        .ok("GET", "/v1/events?since=0&limit=1000", None, None)
        .await;
    page["events"].as_array().unwrap().last().unwrap().clone()
}

async fn ask(t: &T, agent: &str, board: &str) -> i64 {
    t.ok(
        "POST",
        "/v1/asks",
        Some(agent),
        Some(json!({ "agent": agent, "board": board, "kind": "decision", "question": "Ship?" })),
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

async fn answer(t: &T, id: i64) {
    let (s, v) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{id}/answer"),
            Some(json!({ "answer": "yes" })),
        )
        .await;
    assert!(s.is_success(), "{v}");
}

#[tokio::test]
async fn the_flag_is_set_at_creation_and_only_the_user_turns_it_on_later() {
    let t = T::new();
    let scratch = temporary_board(&t, "scratch").await;
    assert_eq!(scratch["temporary"], true);
    assert_eq!(last_event(&t).await["payload"]["temporary"], true);
    let kept = t.board("kept", &[REPO]).await;
    assert_eq!(kept["temporary"], false);

    // A device may not schedule a real board for deletion.
    let (code, err, v) = t
        .fail(
            "PATCH",
            "/v1/boards/kept",
            None,
            Some(json!({ "temporary": true })),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");
    assert_eq!(
        t.ok("GET", "/v1/boards/kept", None, None).await["temporary"],
        false
    );

    // The user may; the event names the field.
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/kept",
            Some(json!({ "temporary": true })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["temporary"], true);
    let event = last_event(&t).await;
    assert_eq!(
        (&event["type"], &event["payload"]["fields"]),
        (&json!("board_updated"), &json!(["temporary"]))
    );

    // Keeping a board (turning it off) is open to anyone, and so is restating it.
    let v = t
        .ok(
            "PATCH",
            "/v1/boards/scratch",
            None,
            Some(json!({ "temporary": true })),
        )
        .await;
    assert_eq!(v["temporary"], true);
    let v = t
        .ok(
            "PATCH",
            "/v1/boards/scratch",
            None,
            Some(json!({ "temporary": false })),
        )
        .await;
    assert_eq!(v["temporary"], false);
    let list = t.ok("GET", "/v1/boards", None, None).await;
    let flags: Vec<(&str, bool)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b["slug"].as_str().unwrap(),
                b["temporary"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(flags, [("kept", true), ("scratch", false)]);
}

#[tokio::test]
async fn an_idle_temporary_board_is_closed_then_deleted_with_everything_on_it() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    t.register(B, Some(REPO), None).await;
    temporary_board(&t, "scratch").await;
    t.board("kept", &[REPO]).await;
    t.ok(
        "POST",
        "/v1/boards/scratch/join",
        Some(A),
        Some(json!({ "agent": A })),
    )
    .await;

    // Something of every kind on the board.
    let first = t.task(A, "scratch", "First").await;
    let parent = t.task(A, "scratch", "Parent").await;
    let child = t
        .task_with(
            A,
            json!({ "board": "scratch", "title": "Child", "parent": parent, "blockers": [first] }),
        )
        .await;
    let (s, v) = t.claim(A, &first).await;
    assert!(s.is_success(), "{v}");
    // B is refused while A holds it: a claim refusal is recorded.
    assert_eq!(t.claim(B, &first).await.0.as_u16(), 409);
    t.ok(
        "POST",
        &format!("/v1/tasks/{first}/notes"),
        Some(A),
        Some(json!({ "agent": A, "kind": "finding", "text": "finding: works" })),
    )
    .await;
    t.ok(
        "POST",
        "/v1/leases",
        Some(A),
        Some(json!({ "agent": A, "repo": REPO, "globs": ["src/**"], "task": first })),
    )
    .await;
    let (s, v) = t.done(A, &first, check(0)).await;
    assert!(s.is_success(), "{v}");
    let old_ask = ask(&t, A, "scratch").await;
    answer(&t, old_ask).await;
    // And something on the board that stays.
    let other = t.task(A, "kept", "Stays").await;

    let refusals = "SELECT COUNT(*) FROM claim_refusals WHERE task LIKE 'scratch-%'";
    assert_eq!(t.count(refusals), 1);

    // Idle one second short of the window: nothing happens.
    t.advance(close_after() - TimeDelta::seconds(1));
    assert_eq!(sweep(&t).await, TemporarySweep::default());
    t.advance(TimeDelta::seconds(1));
    let swept = sweep(&t).await;
    assert_eq!(
        (slugs(&swept.closed), slugs(&swept.deleted)),
        (vec!["scratch"], vec![])
    );
    assert_eq!(status(&t, "scratch").await, "closed");
    assert_eq!(status(&t, "kept").await, "active");
    let event = last_event(&t).await;
    assert_eq!(
        (
            &event["type"],
            &event["actor"],
            &event["board"],
            &event["payload"]["fields"]
        ),
        (
            &json!("board_updated"),
            &json!("server"),
            &json!("scratch"),
            &json!(["status"])
        )
    );

    // Closing counts as activity: the delete window starts there.
    t.advance(delete_after() - TimeDelta::seconds(1));
    assert_eq!(sweep(&t).await, TemporarySweep::default());
    t.advance(TimeDelta::seconds(1));
    let swept = sweep(&t).await;
    assert_eq!(
        (slugs(&swept.closed), slugs(&swept.deleted)),
        (vec![], vec!["scratch"])
    );

    let (code, err, _) = t.fail("GET", "/v1/boards/scratch", None, None).await;
    assert_eq!((code, err.as_str()), (404, "not_found"));
    let (code, _, _) = t
        .fail("GET", &format!("/v1/tasks/{child}"), None, None)
        .await;
    assert_eq!(code, 404);
    // Every column in the schema that can name the board or one of its tasks is clear of
    // it, so a table added later without handling in `delete` fails here. What remains:
    // the deletion event and the slug's task numbering.
    let mut checked = Vec::new();
    for (table, column) in board_and_task_columns(&t) {
        let sql = format!(
            "SELECT COUNT(*) FROM {table} WHERE {column} = 'scratch' OR {column} LIKE 'scratch-%'"
        );
        let expected = match (table.as_str(), column.as_str()) {
            ("events", "board") | ("deleted_boards", "slug") => 1,
            _ => 0,
        };
        assert_eq!(t.count(&sql), expected, "{sql}");
        checked.push(format!("{table}.{column}"));
    }
    for column in [
        "asks.board",
        "board_members.board",
        "boards.slug",
        "claim_refusals.task",
        "events.task",
        "evidence.task",
        "leases.task",
        "notes.task",
        "task_blockers.blocker",
        "task_blockers.task",
        "tasks.parent",
    ] {
        assert!(
            checked.iter().any(|c| c == column),
            "{column} in {checked:?}"
        );
    }
    let event = last_event(&t).await;
    assert_eq!(
        (
            &event["type"],
            &event["actor"],
            &event["board"],
            &event["payload"]
        ),
        (
            &json!("board_updated"),
            &json!("server"),
            &json!("scratch"),
            &json!({ "fields": ["deleted"], "title": "Board scratch" })
        )
    );
    // Everything else is untouched; agents stay registered.
    assert_eq!(t.get_task(&other).await["title"], "Stays");
    assert_eq!(status(&t, "kept").await, "active");
    t.ok("GET", &format!("/v1/agents/{A}"), None, None).await;

    // The slug is free again. Its numbering goes on, so an agent still holding scratch-1
    // reaches nothing rather than a new task.
    temporary_board(&t, "scratch").await;
    assert_eq!(t.task(A, "scratch", "Again").await, "scratch-4");
    let (code, _, _) = t
        .fail("GET", &format!("/v1/tasks/{first}"), None, None)
        .await;
    assert_eq!(code, 404);
    // Ask ids are never handed out again either: Telegram buttons and links outlive them.
    let new_ask = ask(&t, A, "kept").await;
    assert!(new_ask > old_ask, "{new_ask} after {old_ask}");
    let (code, _, _) = t
        .fail("GET", &format!("/v1/asks/{old_ask}"), None, None)
        .await;
    assert_eq!(code, 404);
    // Deleted twice, the slug still counts on.
    t.ok(
        "PATCH",
        "/v1/boards/scratch",
        None,
        Some(json!({ "status": "closed" })),
    )
    .await;
    let (s, v) = t.as_user("DELETE", "/v1/boards/scratch", None).await;
    assert_eq!(s.as_u16(), 204, "{v}");
    t.board("scratch", &[REPO]).await;
    assert_eq!(t.task(A, "scratch", "Third").await, "scratch-5");
}

/// Every `(table, column)` in the schema that holds a board slug or a task id: columns
/// named for one, and columns with a foreign key to boards or tasks.
fn board_and_task_columns(t: &T) -> Vec<(String, String)> {
    let tables = t.strings(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    );
    let mut out = Vec::new();
    for table in tables {
        let named = t.strings(&format!(
            "SELECT name FROM pragma_table_info('{table}') \
             WHERE name IN ('board', 'task', 'blocker', 'parent', 'slug') \
             UNION SELECT \"from\" FROM pragma_foreign_key_list('{table}') \
             WHERE \"table\" IN ('boards', 'tasks')"
        ));
        out.extend(named.into_iter().map(|column| (table.clone(), column)));
    }
    out
}

#[tokio::test]
async fn deleting_clears_the_task_of_a_live_lease() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    t.board("scratch", &[REPO]).await;
    let task = t.task(A, "scratch", "Leased").await;
    t.ok(
        "POST",
        "/v1/leases",
        Some(A),
        Some(json!({ "agent": A, "repo": REPO, "globs": ["src/**"], "task": task })),
    )
    .await;
    t.ok(
        "PATCH",
        "/v1/boards/scratch",
        None,
        Some(json!({ "status": "closed" })),
    )
    .await;
    let (s, v) = t.as_user("DELETE", "/v1/boards/scratch", None).await;
    assert_eq!(s.as_u16(), 204, "{v}");
    assert_eq!(
        t.count("SELECT COUNT(*) FROM leases WHERE agent = 't3:alpha' AND task IS NULL"),
        1
    );
}

#[tokio::test]
async fn a_temporary_board_in_use_is_not_closed() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    temporary_board(&t, "noted").await;
    temporary_board(&t, "asked").await;
    temporary_board(&t, "worked").await;
    temporary_board(&t, "paused").await;
    t.ok(
        "PATCH",
        "/v1/boards/paused",
        None,
        Some(json!({ "status": "paused" })),
    )
    .await;
    let noted = t.task(A, "noted", "Noted").await;
    let open_ask = ask(&t, A, "asked").await;
    let worked = t.task(A, "worked", "Worked").await;
    let (s, v) = t.claim(A, &worked).await;
    assert!(s.is_success(), "{v}");

    // A change one hour before the window ends restarts it; the owner keeps heartbeating,
    // which is not a board event.
    t.advance(close_after() - TimeDelta::hours(1));
    t.ok(
        "POST",
        &format!("/v1/tasks/{noted}/notes"),
        Some(A),
        Some(json!({ "agent": A, "kind": "note", "text": "note: still here" })),
    )
    .await;
    t.ok(
        "POST",
        &format!("/v1/agents/{A}/state"),
        Some(A),
        Some(json!({ "state": "running" })),
    )
    .await;
    t.advance(TimeDelta::hours(2));
    let swept = sweep(&t).await;
    // Paused boards are swept like active ones.
    assert_eq!(
        (slugs(&swept.closed), slugs(&swept.deleted)),
        (vec!["paused"], vec![])
    );

    // Long after: the noted board's window ran out, the owner went stale, the ask is
    // still open.
    t.advance(stale_after().max(close_after()));
    let swept = sweep(&t).await;
    assert_eq!(slugs(&swept.closed), vec!["noted", "worked"]);
    assert_eq!(status(&t, "asked").await, "active");

    // Answering is activity: the window starts over from the answer.
    answer(&t, open_ask).await;
    t.advance(close_after() - TimeDelta::seconds(1));
    assert!(sweep(&t).await.closed.is_empty());
    t.advance(TimeDelta::seconds(1));
    assert_eq!(slugs(&sweep(&t).await.closed), vec!["asked"]);
}

#[tokio::test]
async fn a_closed_temporary_board_with_an_open_ask_is_kept() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    temporary_board(&t, "scratch").await;
    t.ok(
        "PATCH",
        "/v1/boards/scratch",
        None,
        Some(json!({ "status": "closed" })),
    )
    .await;
    let id = ask(&t, A, "scratch").await;
    t.advance(delete_after() * 4);
    assert_eq!(sweep(&t).await, TemporarySweep::default());
    assert_eq!(status(&t, "scratch").await, "closed");
    answer(&t, id).await;
    t.advance(delete_after());
    assert_eq!(slugs(&sweep(&t).await.deleted), vec!["scratch"]);
}

#[tokio::test]
async fn boards_that_are_not_temporary_are_never_swept() {
    let t = T::new();
    t.board("open", &[REPO]).await;
    t.board("closed", &[REPO]).await;
    t.ok(
        "PATCH",
        "/v1/boards/closed",
        None,
        Some(json!({ "status": "closed" })),
    )
    .await;
    t.advance((close_after() + delete_after()) * 10);
    assert_eq!(sweep(&t).await, TemporarySweep::default());
    assert_eq!(sweep(&t).await, TemporarySweep::default());
    assert_eq!(status(&t, "open").await, "active");
    assert_eq!(status(&t, "closed").await, "closed");
}

#[tokio::test]
async fn only_the_user_deletes_a_closed_board_without_open_asks() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    t.board("factory", &[REPO]).await;
    t.task(A, "factory", "Something").await;

    let (code, err, _) = t.fail("DELETE", "/v1/boards/factory", None, None).await;
    assert_eq!((code, err.as_str()), (403, "forbidden"));
    let (s, v) = t.as_user("DELETE", "/v1/boards/factory", None).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(
        v["next"].as_str().unwrap().contains("--status closed"),
        "{v}"
    );

    t.ok(
        "PATCH",
        "/v1/boards/factory",
        None,
        Some(json!({ "status": "closed" })),
    )
    .await;
    let id = ask(&t, A, "factory").await;
    let (s, v) = t.as_user("DELETE", "/v1/boards/factory", None).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("1 open asks"),
        "{v}"
    );

    answer(&t, id).await;
    let (s, v) = t.as_user("DELETE", "/v1/boards/factory", None).await;
    assert_eq!(s.as_u16(), 204, "{v}");
    let event = last_event(&t).await;
    assert_eq!(
        (&event["actor"], &event["payload"]["fields"]),
        (&json!("user"), &json!(["deleted"]))
    );
    let (s, v) = t.as_user("DELETE", "/v1/boards/factory", None).await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (404, Some("not_found")));
    let (s, _) = t.as_user("DELETE", "/v1/boards/Not%20A%20Slug", None).await;
    assert_eq!(s.as_u16(), 404);
}

#[tokio::test]
async fn time_the_server_was_down_does_not_count_as_idle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("board.db");
    let t = T::over(crate::Db::open(&path).unwrap(), |_| {});
    temporary_board(&t, "scratch").await;
    temporary_board(&t, "closed").await;
    t.ok(
        "PATCH",
        "/v1/boards/closed",
        None,
        Some(json!({ "status": "closed" })),
    )
    .await;
    t.advance(TimeDelta::hours(1));
    t.state.shutdown();

    // Down far longer than both windows; the first sweep after the restart, at once, finds
    // nothing idle.
    t.advance((close_after() + delete_after()) * 3);
    let up = t.restart(crate::Db::open(&path).unwrap());
    assert_eq!(sweep(&up).await, TemporarySweep::default());

    // Idle time counts from the restart.
    up.advance(close_after() - TimeDelta::seconds(1));
    assert_eq!(sweep(&up).await, TemporarySweep::default());
    up.advance(TimeDelta::seconds(1));
    assert_eq!(slugs(&sweep(&up).await.closed), vec!["scratch"]);
    up.advance(delete_after() - close_after());
    assert_eq!(slugs(&sweep(&up).await.deleted), vec!["closed"]);
}

#[tokio::test]
async fn a_board_that_fails_to_sweep_does_not_hold_up_the_others() {
    let t = T::new();
    for slug in ["alpha", "broken", "zulu"] {
        temporary_board(&t, slug).await;
    }
    t.exec(
        "CREATE TRIGGER refuse BEFORE UPDATE ON boards WHEN old.slug = 'broken' \
         BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    );
    t.advance(close_after());
    let swept = sweep(&t).await;
    assert_eq!(
        (slugs(&swept.closed), slugs(&swept.failed)),
        (vec!["alpha", "zulu"], vec!["broken"])
    );
    assert_eq!(status(&t, "broken").await, "active");
    // Tried again on the next sweep.
    t.exec("DROP TRIGGER refuse");
    let swept = sweep(&t).await;
    assert_eq!(
        (slugs(&swept.closed), slugs(&swept.failed)),
        (vec!["broken"], vec![])
    );
}

#[tokio::test]
async fn a_board_used_after_it_was_found_due_is_kept() {
    use crate::ops::boards::{Swept, sweep_temporary, temporary_due};
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    temporary_board(&t, "scratch").await;
    temporary_board(&t, "closed").await;
    let task = t.task(A, "scratch", "Busy").await;
    t.ok(
        "PATCH",
        "/v1/boards/closed",
        None,
        Some(json!({ "status": "closed" })),
    )
    .await;
    t.advance(delete_after());
    let since = t.state.started().timestamp_millis();
    let due = t
        .state
        .read(move |cx| temporary_due(cx, since))
        .await
        .unwrap();
    assert_eq!(slugs(&due), vec!["closed", "scratch"]);

    // Between finding them due and sweeping each: one is claimed, the other reopened.
    let (s, v) = t.claim(A, &task).await;
    assert!(s.is_success(), "{v}");
    t.ok(
        "PATCH",
        "/v1/boards/closed",
        None,
        Some(json!({ "status": "active" })),
    )
    .await;
    for slug in due {
        let swept = t
            .state
            .write(move |cx| sweep_temporary(cx, &slug, since))
            .await
            .unwrap();
        assert_eq!(swept, None::<Swept>);
    }
    assert_eq!(status(&t, "scratch").await, "active");
    assert_eq!(status(&t, "closed").await, "active");
}

#[tokio::test]
async fn the_sweeper_task_closes_idle_temporary_boards() {
    let t = T::new();
    temporary_board(&t, "scratch").await;
    t.advance(close_after());
    let task = crate::spawn_sweeper(t.state.clone());
    // The first tick runs at once.
    for _ in 0..200 {
        if status(&t, "scratch").await == "closed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(status(&t, "scratch").await, "closed");
    t.state.shutdown();
    task.await.unwrap();
}

async fn close_and_delete(t: &T, slug: &str) {
    t.ok(
        "PATCH",
        &format!("/v1/boards/{slug}"),
        None,
        Some(json!({ "status": "closed" })),
    )
    .await;
    let (s, v) = t
        .as_user("DELETE", &format!("/v1/boards/{slug}"), None)
        .await;
    assert_eq!(s.as_u16(), 204, "{v}");
}

#[tokio::test]
async fn deleting_either_board_removes_a_link_across_boards() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    t.board("kept", &[REPO]).await;

    // The blocker's board goes: the task elsewhere loses the blocker and is announced, on
    // its own board, so a coordinator hears it became ready.
    temporary_board(&t, "scratch").await;
    let blocker = t.task(A, "scratch", "Blocker").await;
    let local = t.task(A, "kept", "Local").await;
    let waiting = t
        .task_with(
            A,
            json!({ "board": "kept", "title": "Waits", "blockers": [blocker, local] }),
        )
        .await;
    let version = t.get_task(&waiting).await["version"].as_i64().unwrap();
    close_and_delete(&t, "scratch").await;
    let task = t.get_task(&waiting).await;
    assert_eq!(
        (&task["blockers"], task["version"].as_i64()),
        (&json!([local]), Some(version + 1))
    );
    let event = last_event(&t).await;
    assert_eq!(
        (
            &event["type"],
            &event["board"],
            &event["task"],
            &event["payload"]
        ),
        (
            &json!("task_updated"),
            &json!("kept"),
            &json!(waiting),
            &json!({
                "fields": ["blockers"], "blockers_removed": [blocker], "board_deleted": "scratch",
            })
        )
    );
    let (s, v) = t.patch(A, &waiting, json!({ "blockers": [] })).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(t.get_task(&waiting).await["ready"], true);

    // The waiting task's board goes: the blocker no longer blocks anything.
    temporary_board(&t, "scratch2").await;
    let dependent = t
        .task_with(
            A,
            json!({ "board": "scratch2", "title": "Waits", "blockers": [local] }),
        )
        .await;
    let detail = t.ok("GET", &format!("/v1/tasks/{local}"), None, None).await;
    assert_eq!(detail["blocks"], json!([dependent]));
    close_and_delete(&t, "scratch2").await;
    let detail = t.ok("GET", &format!("/v1/tasks/{local}"), None, None).await;
    assert_eq!(detail["blocks"], json!([]));
    assert_eq!(
        t.count("SELECT COUNT(*) FROM task_blockers WHERE task LIKE 'scratch%' OR blocker LIKE 'scratch%'"),
        0
    );
    // Only the deletion event; the kept task is not announced again.
    assert_eq!(
        last_event(&t).await["payload"]["fields"],
        json!(["deleted"])
    );

    // A finished task that waited on the deleted board loses the link unannounced.
    temporary_board(&t, "scratch3").await;
    let old = t.task(A, "scratch3", "Old").await;
    let finished = t
        .task_with(
            A,
            json!({ "board": "kept", "title": "Finished", "blockers": [old] }),
        )
        .await;
    t.patch(A, &finished, json!({ "status": "dropped" })).await;
    let version = t.get_task(&finished).await["version"].clone();
    close_and_delete(&t, "scratch3").await;
    let task = t.get_task(&finished).await;
    assert_eq!(
        (&task["blockers"], &task["version"]),
        (&json!([]), &version)
    );
    assert_eq!(
        last_event(&t).await["payload"]["fields"],
        json!(["deleted"])
    );
}

#[tokio::test]
async fn a_temporary_board_another_board_waits_on_is_not_swept() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    t.board("kept", &[REPO]).await;
    temporary_board(&t, "scratch").await;
    let blocker = t.task(A, "scratch", "Prerequisite").await;
    let waiting = t
        .task_with(
            A,
            json!({ "board": "kept", "title": "Waits", "blockers": [blocker] }),
        )
        .await;
    t.advance(close_after() + delete_after());
    assert_eq!(sweep(&t).await, TemporarySweep::default());
    assert_eq!(status(&t, "scratch").await, "active");

    // Once the waiting task is dropped, nothing waits on it: closed, then deleted.
    t.patch(A, &waiting, json!({ "status": "dropped" })).await;
    t.advance(close_after());
    assert_eq!(slugs(&sweep(&t).await.closed), ["scratch"]);
    t.advance(delete_after());
    assert_eq!(slugs(&sweep(&t).await.deleted), ["scratch"]);
}
