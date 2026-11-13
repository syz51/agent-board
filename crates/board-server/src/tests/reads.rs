//! Reads run on the read-only pool: they never wait for the writer, and they see the last
//! commit, not a write in progress.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::{REPO, T};

const ME: &str = "t3:me";

/// Fails when a read waits for the writer instead of answering.
async fn promptly(read: impl Future<Output = Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("the read waited for the writer")
}

#[tokio::test]
async fn reads_answer_while_a_write_holds_the_writer() {
    let t = T::new();
    t.board("app", &[REPO]).await;
    t.register(ME, Some(REPO), Some("main")).await;
    let task = t.task(ME, "app", "Committed title").await;

    // A write transaction holds the writer, with a change it has not committed.
    let (held, wait_held) = mpsc::channel();
    let (release, wait_release) = mpsc::channel::<()>();
    let db = t.state.db().clone();
    let settings = t.state.settings().clone();
    let now = t.state.now();
    let update = format!("UPDATE tasks SET title = 'Uncommitted title' WHERE id = '{task}'");
    let writer = thread::spawn(move || {
        db.write(
            || now,
            &settings,
            |cx| {
                cx.db.execute(&update, [])?;
                held.send(()).unwrap();
                // The test ends the write; it rolls back if the test panics first.
                wait_release
                    .recv()
                    .map_err(|e| crate::Error::Internal(e.to_string()))?;
                Ok(())
            },
        )
    });
    wait_held.recv().unwrap();

    let stats = promptly(t.ok("GET", "/v1/stats", Some(ME), None)).await;
    assert!(stats.is_object(), "{stats}");
    let tasks = promptly(t.ok("GET", "/v1/tasks?board=app", Some(ME), None)).await;
    assert_eq!(tasks[0]["title"], json!("Committed title"));
    let detail = promptly(t.ok("GET", &format!("/v1/tasks/{task}"), Some(ME), None)).await;
    assert_eq!(detail["task"]["title"], json!("Committed title"));
    // The user's look at an agent's snapshot moves nothing, so it reads too.
    let look = promptly(async {
        let (status, body) = t
            .as_user("GET", &format!("/v1/snapshot?agent={ME}&kind=full"), None)
            .await;
        assert!(status.is_success(), "{status} {body}");
        body
    })
    .await;
    assert!(
        look["text"].as_str().unwrap().contains("Committed title"),
        "{look}"
    );

    release.send(()).unwrap();
    writer.join().unwrap().unwrap();
    assert_eq!(t.get_task(&task).await["title"], json!("Uncommitted title"));
}

/// An agent's own snapshot holds the writer only to move its cursor: the render, which is
/// most of the work, runs on a read connection while another write is in progress.
#[tokio::test]
async fn an_agents_own_snapshot_renders_while_a_write_holds_the_writer() {
    let t = T::new();
    t.board("app", &[REPO]).await;
    t.register(ME, Some(REPO), Some("main")).await;
    let task = t.task(ME, "app", "Committed title").await;

    let (held, wait_held) = mpsc::channel();
    let (release, wait_release) = mpsc::channel::<()>();
    let db = t.state.db().clone();
    let settings = t.state.settings().clone();
    let now = t.state.now();
    let update = format!("UPDATE tasks SET title = 'Uncommitted title' WHERE id = '{task}'");
    let writer = thread::spawn(move || {
        db.write(
            || now,
            &settings,
            |cx| {
                cx.db.execute(&update, [])?;
                held.send(()).unwrap();
                wait_release
                    .recv()
                    .map_err(|e| crate::Error::Internal(e.to_string()))?;
                Ok(())
            },
        )
    });
    wait_held.recv().unwrap();

    let (reads, writes) = t.state.db().transactions();
    let own = t.ok("GET", "/v1/snapshot?kind=full", Some(ME), None);
    tokio::pin!(own);
    // Two reads begin, the caller's device and the render, and no write: it waits.
    let rendered = async {
        while t.state.db().transactions().0 < reads + 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    tokio::select! {
        own = &mut own => panic!("answered while the writer was held: {own}"),
        done = tokio::time::timeout(Duration::from_secs(5), rendered) => {
            done.expect("the render waited for the writer");
        }
    }
    assert_eq!(t.state.db().transactions().1, writes);

    release.send(()).unwrap();
    writer.join().unwrap().unwrap();
    // It shows the state it rendered, as if it had run before the other write.
    let own = own.await;
    assert!(
        own["text"].as_str().unwrap().contains("Committed title"),
        "{own}"
    );
    assert_eq!(t.state.db().transactions(), (reads + 2, writes + 1));
    let cursor = t.count(&format!("SELECT cursor FROM agents WHERE id = '{ME}'"));
    assert_eq!(cursor, own["seq"].as_i64().unwrap());
}

/// Two snapshots of one agent at once, as from two tool calls: the one that advances
/// second was rendered before the first moved the cursor and the shown counts, so it is
/// rendered again and repeats nothing the first showed.
#[tokio::test]
async fn a_snapshot_rendered_before_the_agents_cursor_moved_is_rendered_again() {
    const PEER: &str = "t3:peer";
    let t = T::new();
    t.board("app", &[REPO]).await;
    t.register(ME, Some(REPO), Some("main")).await;
    t.register(PEER, Some(REPO), Some("main")).await;
    let mine = t.task(ME, "app", "Mine").await;
    assert!(t.claim(ME, &mine).await.0.is_success());
    t.ok("GET", "/v1/snapshot?kind=full", Some(ME), None).await;
    // News for ME: the board's counts change, and a peer leaves a note on its task.
    t.task(PEER, "app", "Another").await;
    t.ok(
        "POST",
        &format!("/v1/tasks/{mine}/notes"),
        Some(PEER),
        Some(json!({ "agent": PEER, "kind": "note", "text": "look at this" })),
    )
    .await;

    let db = t.state.db();
    let request = || crate::snapshot::Request {
        agent: ME.parse().unwrap(),
        kind: board_proto::SnapshotKind::Delta,
        since: None,
        format: board_proto::SnapshotFormat::default(),
    };
    let render = || {
        db.read(t.state.now(), t.state.settings(), |cx| {
            crate::snapshot::snapshot(cx, request())
        })
        .unwrap()
    };
    let advance = |rendered| {
        db.write(
            || t.state.now(),
            t.state.settings(),
            |cx| crate::snapshot::advance(cx, rendered),
        )
        .unwrap()
    };
    let (first, second) = (render(), render());
    assert_eq!(first.snapshot.text, second.snapshot.text);
    let first = advance(first);
    assert!(first.text.contains("changed your task"), "{}", first.text);
    assert!(first.text.contains("counts: open 1"), "{}", first.text);
    let second = advance(second);
    assert_eq!(second.text, "");
    assert_eq!(second.seq, first.seq);
    // Rendered after the cursor moved, a snapshot advances as it is.
    assert_eq!(advance(render()).text, "");
}

#[tokio::test]
async fn a_users_look_leaves_the_agents_cursor_and_an_agents_own_snapshot_moves_it() {
    let t = T::new();
    t.board("app", &[REPO]).await;
    t.register(ME, Some(REPO), Some("main")).await;
    t.task(ME, "app", "Some work").await;
    let cursor = || t.count(&format!("SELECT cursor FROM agents WHERE id = '{ME}'"));
    let before = cursor();
    let (status, _) = t
        .as_user("GET", &format!("/v1/snapshot?agent={ME}&kind=full"), None)
        .await;
    assert!(status.is_success());
    assert_eq!(cursor(), before);
    let own = t.ok("GET", "/v1/snapshot?kind=full", Some(ME), None).await;
    assert!(own["seq"].as_i64().unwrap() > before);
    assert_eq!(cursor(), own["seq"].as_i64().unwrap());
}
