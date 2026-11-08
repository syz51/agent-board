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
