//! Updates for the user: done summaries, coordinators' turn endings and the seen cursor.

use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{T, iso, start};

const COORD: &str = "t3:coord";
const WORKER: &str = "t3:worker";

/// Puts `slug` on autopilot with `COORD` as its coordinator.
async fn coordinate(t: &T, slug: &str) {
    t.ok(
        "PATCH",
        &format!("/v1/boards/{slug}"),
        None,
        Some(json!({ "autopilot": true })),
    )
    .await;
    t.ok(
        "POST",
        &format!("/v1/boards/{slug}/coordinator"),
        Some(COORD),
        Some(json!({ "device": "mac", "agent": COORD })),
    )
    .await;
}

/// A board `factory` coordinated by `COORD`, with `WORKER` registered.
async fn coordinated() -> T {
    let t = T::new();
    t.board("factory", &[]).await;
    t.register(COORD, None, None).await;
    t.register(WORKER, None, None).await;
    coordinate(&t, "factory").await;
    t
}

async fn stop(t: &T, agent: &str, said: Option<&str>) -> (u16, Value) {
    let mut body = json!({ "state": "idle", "paused_until": null });
    if let Some(said) = said {
        body["said"] = json!(said);
    }
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/agents/{agent}/state"),
            Some(agent),
            Some(body),
        )
        .await;
    (s.as_u16(), v)
}

async fn done_with(t: &T, agent: &str, task: &str, summary: Option<&str>) -> (u16, Value) {
    let version = t.get_task(task).await["version"].clone();
    let mut body = json!({
        "agent": agent, "version": version,
        "evidence": [{ "kind": "commit", "value": "4e1c2ab", "exit_code": null }],
    });
    if let Some(summary) = summary {
        body["summary"] = json!(summary);
    }
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/tasks/{task}/done"),
            Some(agent),
            Some(body),
        )
        .await;
    (s.as_u16(), v)
}

async fn finished(t: &T, title: &str, summary: Option<&str>) -> String {
    let id = t.task(WORKER, "factory", title).await;
    assert_eq!(t.claim(WORKER, &id).await.0.as_u16(), 200);
    let (s, v) = done_with(t, WORKER, &id, summary).await;
    assert_eq!(s, 200, "{v}");
    id
}

async fn updates(t: &T, query: &str) -> Value {
    let (s, v) = t.as_user("GET", &format!("/v1/updates{query}"), None).await;
    assert_eq!(s.as_u16(), 200, "{v}");
    v
}

#[tokio::test]
async fn done_stores_the_summary_and_reopening_clears_it() {
    let t = coordinated().await;
    let id = finished(&t, "Add the inbox", Some("Shipped it.\nSee PR #3.")).await;
    assert_eq!(t.get_task(&id).await["summary"], "Shipped it.\nSee PR #3.");

    // The event carries no summary: it is for the user, not for agents' feeds.
    let events = t.ok("GET", "/v1/events?board=factory", None, None).await;
    let done = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "task_done")
        .unwrap();
    assert!(done["payload"].get("summary").is_none(), "{done}");

    let (s, v) = t.patch(WORKER, &id, json!({ "status": "open" })).await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert!(t.get_task(&id).await.get("summary").is_none());
}

#[tokio::test]
async fn done_without_a_summary_works_as_before() {
    let t = coordinated().await;
    let id = t.task(WORKER, "factory", "Old client").await;
    t.claim(WORKER, &id).await;
    // `T::done` sends the body an older client sends: no summary field.
    let (s, v) = t
        .done(
            WORKER,
            &id,
            json!([{ "kind": "check", "value": "cargo test", "exit_code": 0 }]),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert!(v.get("summary").is_none(), "{v}");
}

#[tokio::test]
async fn a_summary_must_fit() {
    let t = coordinated().await;
    let id = t.task(WORKER, "factory", "Too long").await;
    t.claim(WORKER, &id).await;
    let (s, v) = done_with(&t, WORKER, &id, Some(&"x".repeat(801))).await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    assert_eq!(v["message"], "summary is 801 characters; the limit is 800");
    assert_eq!(
        v["next"],
        "shorten summary to at most 800 characters, or put the detail in a note on the task"
    );
    let (s, v) = done_with(&t, WORKER, &id, Some("  ")).await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let (s, v) = done_with(&t, WORKER, &id, Some(&"x".repeat(800))).await;
    assert_eq!(s, 200, "{v}");
}

#[tokio::test]
async fn a_coordinators_turn_ending_is_kept_per_board_and_a_workers_is_dropped() {
    let t = coordinated().await;
    t.board("second", &[]).await;
    coordinate(&t, "second").await;

    let (s, v) = stop(&t, COORD, Some("Done with factory-1; the PR is open.")).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "idle");
    let (s, _) = stop(&t, WORKER, Some("I fixed the test.")).await;
    assert_eq!(s, 200);

    let said = updates(&t, "").await["said"].clone();
    let boards: Vec<&str> = said
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["board"].as_str().unwrap())
        .collect();
    assert_eq!(boards.len(), 2, "{said}");
    assert!(boards.contains(&"factory") && boards.contains(&"second"));
    assert!(said.as_array().unwrap().iter().all(|s| s["agent"] == COORD));
    assert_eq!(said[0]["text"], "Done with factory-1; the PR is open.");

    let mine = t
        .as_user("GET", &format!("/v1/agents/{COORD}/said?limit=5"), None)
        .await;
    assert_eq!(mine.0.as_u16(), 200);
    assert_eq!(mine.1.as_array().unwrap().len(), 2);
    let worker = t
        .as_user("GET", &format!("/v1/agents/{WORKER}/said"), None)
        .await;
    assert_eq!(worker.1, json!([]));
}

#[tokio::test]
async fn turn_endings_are_cleaned_and_cut_never_refused_and_the_newest_fifty_are_kept() {
    let t = coordinated().await;
    // Advisory text never fails the state change: it is cleaned, cut, or left out.
    let (s, v) = stop(&t, COORD, Some("\u{1b}[1mbell\u{1b}[0m \u{7}")).await;
    assert_eq!(s, 200, "{v}");
    let (s, _) = stop(&t, COORD, Some(&"x".repeat(5000))).await;
    assert_eq!(s, 200);
    let (s, _) = stop(&t, COORD, Some(" \n\u{7} ")).await;
    assert_eq!(s, 200);
    let kept = t.strings("SELECT text FROM said ORDER BY id");
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert_eq!(kept[0], "bell");
    assert_eq!(kept[1].chars().count(), 4000);
    assert!(kept[1].ends_with("x…"));

    for n in 0..55 {
        t.advance(TimeDelta::seconds(1));
        let (s, v) = stop(&t, COORD, Some(&format!("turn {n}\n\twith a tab"))).await;
        assert_eq!(s, 200, "{v}");
    }
    let kept = t.strings("SELECT text FROM said WHERE agent = 't3:coord' ORDER BY id");
    assert_eq!(kept.len(), 50);
    assert_eq!(kept[0], "turn 5\n\twith a tab");
    let (_, list) = t
        .as_user("GET", &format!("/v1/agents/{COORD}/said?limit=50"), None)
        .await;
    assert_eq!(list[0]["text"], "turn 54\n\twith a tab");
    let (s, _) = t
        .as_user("GET", &format!("/v1/agents/{COORD}/said?limit=51"), None)
        .await;
    assert_eq!(s.as_u16(), 400);
}

#[tokio::test]
async fn ending_a_session_keeps_its_last_turn_ending() {
    let t = coordinated().await;
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/agents/{COORD}/state"),
            Some(COORD),
            Some(json!({ "state": "ended", "paused_until": null, "said": "Signing off." })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert_eq!(updates(&t, "").await["said"][0]["text"], "Signing off.");
    // Ending keeps the coordinator lease (T3 closes idle sessions), so the session that
    // resumes it still coordinates and its turn ending is kept.
    stop(&t, COORD, Some("Back again.")).await;
    assert_eq!(updates(&t, "").await["said"].as_array().unwrap().len(), 2);
    // Once the lease has expired, it coordinates nothing, and its endings are not kept.
    t.advance(TimeDelta::minutes(46));
    stop(&t, COORD, Some("Later.")).await;
    assert_eq!(updates(&t, "").await["said"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn updates_are_for_the_user_only() {
    let t = coordinated().await;
    for path in ["/v1/updates".to_owned(), format!("/v1/agents/{COORD}/said")] {
        let (status, code, _) = t.fail("GET", &path, Some(COORD), None).await;
        assert_eq!((status, code.as_str()), (403, "forbidden"), "{path}");
    }
    let (status, code, _) = t
        .fail(
            "PUT",
            "/v1/updates/seen",
            Some(COORD),
            Some(json!({ "at": iso(start()) })),
        )
        .await;
    assert_eq!((status, code.as_str()), (403, "forbidden"));
}

#[tokio::test]
async fn updates_list_what_happened_since_the_user_last_looked() {
    let t = coordinated().await;
    let before = updates(&t, "").await;
    assert_eq!(
        (before["seen_at"].clone(), before["since"].clone()),
        (Value::Null, Value::Null)
    );
    assert_eq!(before["done"], json!([]));

    t.advance(TimeDelta::seconds(10));
    let first = finished(&t, "First", Some("First is in.")).await;
    stop(&t, COORD, Some("First is done.")).await;
    let seen = t.state.now();

    t.advance(TimeDelta::seconds(10));
    let second = finished(&t, "Second", None).await;
    stop(&t, COORD, Some("Second is done.")).await;

    let all = updates(&t, "").await;
    let done: Vec<&str> = all["done"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["task"].as_str().unwrap())
        .collect();
    assert_eq!(done, [second.as_str(), first.as_str()]);
    assert_eq!(all["done"][1]["summary"], "First is in.");
    assert_eq!(all["done"][0]["summary"], Value::Null);
    assert_eq!(all["done"][0]["owner"], WORKER);
    assert_eq!(all["done"][0]["evidence"][0]["value"], "4e1c2ab");
    assert_eq!(all["more"], false);

    // The page marks what it showed; the cursor never moves back or past now.
    let (s, v) = t
        .as_user("PUT", "/v1/updates/seen", Some(json!({ "at": iso(seen) })))
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert_eq!(v["seen_at"], iso(seen));
    let (_, v) = t
        .as_user(
            "PUT",
            "/v1/updates/seen",
            Some(json!({ "at": iso(seen - TimeDelta::seconds(5)) })),
        )
        .await;
    assert_eq!(v["seen_at"], iso(seen));

    let unseen = updates(&t, "").await;
    assert_eq!(unseen["seen_at"], iso(seen));
    assert_eq!(unseen["done"].as_array().unwrap().len(), 1);
    assert_eq!(unseen["done"][0]["task"], second.as_str());
    assert_eq!(unseen["said"].as_array().unwrap().len(), 1);
    assert_eq!(unseen["said"][0]["text"], "Second is done.");

    // `since` overrides the cursor.
    let since = format!("?since={}", iso(start()).replace('+', "%2B"));
    assert_eq!(
        updates(&t, &since).await["done"].as_array().unwrap().len(),
        2
    );

    // A reopened task is no longer listed.
    t.patch(WORKER, &second, json!({ "status": "open" })).await;
    assert_eq!(updates(&t, "").await["done"], json!([]));

    let (_, v) = t
        .as_user(
            "PUT",
            "/v1/updates/seen",
            Some(json!({ "at": iso(t.state.now() + TimeDelta::hours(1)) })),
        )
        .await;
    assert_eq!(v["seen_at"], iso(t.state.now()));
}

#[tokio::test]
async fn updates_filter_by_board_and_say_when_cut() {
    let t = coordinated().await;
    t.board("other", &[]).await;
    let other = t.task(WORKER, "other", "Elsewhere").await;
    t.claim(WORKER, &other).await;
    assert_eq!(done_with(&t, WORKER, &other, None).await.0, 200);
    for n in 0..101 {
        t.advance(TimeDelta::milliseconds(10));
        let id = t.task(WORKER, "factory", &format!("Task {n}")).await;
        // Done straight from doing: one claim at a time stays within the doing limit.
        t.claim(WORKER, &id).await;
        assert_eq!(done_with(&t, WORKER, &id, None).await.0, 200);
    }
    let factory = updates(&t, "?board=factory").await;
    assert_eq!(factory["done"].as_array().unwrap().len(), 100);
    assert_eq!(factory["more"], true);
    assert!(
        factory["done"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["board"] == "factory")
    );
    let only = updates(&t, "?board=other").await;
    assert_eq!(only["done"].as_array().unwrap().len(), 1);
    assert_eq!(only["more"], false);
}

#[tokio::test]
async fn deleting_a_board_deletes_its_turn_endings() {
    let t = coordinated().await;
    stop(&t, COORD, Some("Nothing left.")).await;
    t.ok(
        "DELETE",
        "/v1/boards/factory/coordinator",
        Some(COORD),
        None,
    )
    .await;
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/factory",
            Some(json!({ "status": "closed" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let (s, v) = t.as_user("DELETE", "/v1/boards/factory", None).await;
    assert_eq!(s.as_u16(), 204, "{v}");
    assert!(t.strings("SELECT text FROM said").is_empty());
}

#[tokio::test]
async fn summaries_and_turn_endings_stay_out_of_agents_context() {
    let t = coordinated().await;
    for agent in [COORD, WORKER] {
        for kind in ["delta", "alerts"] {
            t.ok(
                "GET",
                &format!("/v1/snapshot?kind={kind}"),
                Some(agent),
                None,
            )
            .await;
        }
    }
    let id = t.task(COORD, "factory", "Leaky").await;
    t.claim(WORKER, &id).await;
    assert_eq!(
        done_with(&t, WORKER, &id, Some("SUMMARY-MARK")).await.0,
        200
    );
    assert_eq!(stop(&t, COORD, Some("SAID-MARK")).await.0, 200);

    for agent in [COORD, WORKER] {
        for kind in ["full", "delta", "alerts"] {
            let v = t
                .ok(
                    "GET",
                    &format!("/v1/snapshot?kind={kind}"),
                    Some(agent),
                    None,
                )
                .await;
            let text = v["text"].as_str().unwrap();
            assert!(
                !text.contains("SUMMARY-MARK") && !text.contains("SAID-MARK"),
                "{agent} {kind}: {text}"
            );
        }
    }
    let events = t
        .ok("GET", "/v1/events?board=factory", None, None)
        .await
        .to_string();
    assert!(!events.contains("SUMMARY-MARK") && !events.contains("SAID-MARK"));
}

#[tokio::test]
async fn a_kept_turn_ending_announces_itself_even_when_already_idle() {
    let t = coordinated().await;
    stop(&t, COORD, None).await;
    let count = || {
        t.strings(
            "SELECT type FROM events WHERE type = 'agent_state_changed' AND actor = 't3:coord'",
        )
        .len()
    };
    let before = count();
    stop(&t, COORD, None).await;
    assert_eq!(count(), before, "idle to idle without words is no change");
    stop(&t, COORD, Some("Still here.")).await;
    assert_eq!(
        count(),
        before + 1,
        "a kept turn ending refreshes the user's views"
    );
    // A worker's words are not kept, so they announce nothing.
    stop(&t, WORKER, None).await;
    let worker = || {
        t.strings(
            "SELECT type FROM events WHERE type = 'agent_state_changed' AND actor = 't3:worker'",
        )
        .len()
    };
    let quiet = worker();
    stop(&t, WORKER, Some("Done here.")).await;
    assert_eq!(worker(), quiet);
}

#[tokio::test]
async fn a_lapsed_coordinator_lease_keeps_nothing() {
    let t = coordinated().await;
    t.advance(TimeDelta::hours(2));
    // The heartbeat no longer renews a lapsed lease, and nothing is kept.
    stop(&t, COORD, Some("Talking to the user now.")).await;
    assert!(t.strings("SELECT text FROM said").is_empty());
}

#[tokio::test]
async fn resolved_feedback_items_are_not_finished_work() {
    let t = coordinated().await;
    let item = t
        .ok(
            "POST",
            "/v1/feedback",
            Some(WORKER),
            Some(json!({ "about": "board", "text": "Hooks print twice" })),
        )
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (s, v) = t
        .as_user(
            "POST",
            &format!("/v1/feedback/{item}/resolve"),
            Some(json!({ "kind": "declined", "reason": "works as designed" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert_eq!(t.get_task(&item).await["status"], "done");
    let real = finished(&t, "Real work", Some("Shipped.")).await;
    let done = updates(&t, "").await["done"].clone();
    assert_eq!(done.as_array().unwrap().len(), 1, "{done}");
    assert_eq!(done[0]["task"], real.as_str());
}
