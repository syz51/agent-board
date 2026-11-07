//! Feedback items: reports, dedupe, recurrence, resolution and the server's error signals.

use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{T, check};

const A: &str = "t3:alpha";
const B: &str = "t3:beta";
const C: &str = "t3:gamma";

async fn setup() -> T {
    let t = T::new();
    for agent in [A, B, C] {
        t.register(agent, None, None).await;
    }
    t
}

async fn report(t: &T, agent: &str, body: Value) -> Value {
    t.ok("POST", "/v1/feedback", Some(agent), Some(body)).await
}

async fn resolve(t: &T, agent: Option<&str>, item: &str, body: Value) -> (u16, Value) {
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/feedback/{item}/resolve"),
            agent,
            Some(body),
        )
        .await;
    (s.as_u16(), v)
}

async fn notes(t: &T, task: &str) -> Vec<String> {
    let detail = t.ok("GET", &format!("/v1/tasks/{task}"), None, None).await;
    detail["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["text"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn two_reports_with_the_same_signature_are_one_item() {
    let t = setup().await;
    let first = report(
        &t,
        A,
        json!({ "about": "Board", "text": "Claims of factory-12 take 30s\nSeen twice today.", "activity": "build" }),
    )
    .await;
    assert_eq!(first["duplicate"], false);
    assert_eq!(first["reports"], 1);
    let task = &first["task"];
    let id = task["id"].as_str().unwrap().to_owned();
    assert_eq!(
        id, "board-feedback-1",
        "files on the feedback board, created on use"
    );
    assert_eq!(task["title"], "Claims of factory-12 take 30s");
    assert_eq!(
        task["body"],
        "Claims of factory-12 take 30s\nSeen twice today."
    );
    assert_eq!(task["created_by"], A);
    assert_eq!(task["auto"], false, "the new board has autopilot off");
    let fb = &task["feedback"];
    assert_eq!(fb["about"], "board");
    assert_eq!(fb["activity"], "build");
    assert_eq!(fb["source"], "agent");
    assert_eq!(fb["signature"], "board:claims of ID take Ns");
    assert_eq!(fb["reporters"], json!([A]));
    assert!(fb.get("resolution").is_none() && fb.get("recurs").is_none());

    // Another id and number normalize to the same signature.
    let second = report(
        &t,
        B,
        json!({ "about": "board", "text": "Claims of other-3 take 45s" }),
    )
    .await;
    assert_eq!(second["duplicate"], true);
    assert_eq!(second["reports"], 2);
    assert_eq!(second["task"]["id"], id.as_str());
    assert_eq!(second["task"]["feedback"]["reports"], 2);
    assert_eq!(second["task"]["feedback"]["reporters"], json!([A, B]));
    assert_eq!(
        second["task"]["version"].as_i64(),
        Some(task["version"].as_i64().unwrap() + 1)
    );
    assert_eq!(t.count("SELECT COUNT(*) FROM tasks"), 1);
    assert_eq!(notes(&t, &id).await.len(), 1, "the first repeat is noted");

    // Notes come at most once an hour; reports always count.
    let third = report(
        &t,
        A,
        json!({ "about": "board", "text": "Claims of factory-1 take 9s" }),
    )
    .await;
    assert_eq!(third["reports"], 3);
    assert_eq!(third["task"]["feedback"]["reporters"], json!([B, A]));
    assert_eq!(notes(&t, &id).await.len(), 1);
    t.advance(TimeDelta::minutes(61));
    report(
        &t,
        C,
        json!({ "about": "board", "text": "Claims of factory-1 take 9s" }),
    )
    .await;
    let noted = notes(&t, &id).await;
    assert_eq!(noted.len(), 2);
    assert!(
        noted[1].starts_with("reported again (4 reports)"),
        "{noted:?}"
    );

    // An explicit signature dedupes reports with other titles; another one does not.
    let sig = report(
        &t,
        A,
        json!({ "about": "hooks", "text": "Hook is slow", "signature": "hooks:slow" }),
    )
    .await;
    let again = report(
        &t,
        B,
        json!({ "about": "hooks", "text": "Stop hook took 3 seconds", "signature": "hooks:slow" }),
    )
    .await;
    assert_eq!(again["task"]["id"], sig["task"]["id"]);
    assert_eq!(again["reports"], 2);
    let other = report(&t, B, json!({ "about": "other", "text": "Something else" })).await;
    assert_eq!(other["duplicate"], false);
}

#[tokio::test]
async fn a_report_after_the_routed_task_is_done_opens_a_new_item_naming_the_old_one() {
    let t = setup().await;
    t.board("factory", &[]).await;
    let first = report(
        &t,
        A,
        json!({ "about": "skills:rust", "text": "Clippy flags unknown" }),
    )
    .await;
    let item = first["task"]["id"].as_str().unwrap().to_owned();
    let work = t
        .task(B, "factory", "Teach the rust skill clippy flags")
        .await;

    assert!(t.claim(C, &item).await.0.is_success());
    let (s, resolved) = resolve(
        &t,
        Some(C),
        &item,
        json!({ "kind": "routed", "task": work }),
    )
    .await;
    assert_eq!(s, 200, "{resolved}");
    assert_eq!(resolved["status"], "done");
    let resolution = &resolved["feedback"]["resolution"];
    assert_eq!(resolution["kind"], "routed");
    assert_eq!(resolution["task"], work.as_str());
    assert_eq!(resolution["by"], C);

    // While the routed task is open, reports add to the item and note the routed task.
    let again = report(
        &t,
        B,
        json!({ "about": "skills:rust", "text": "Clippy flags unknown" }),
    )
    .await;
    assert_eq!(again["duplicate"], true);
    assert_eq!(again["task"]["id"], item.as_str());
    assert_eq!(
        again["task"]["status"], "done",
        "a report does not reopen the item"
    );
    let noted = notes(&t, &item).await;
    assert!(
        noted[0].contains(&format!("routed to {work} (open)")),
        "{noted:?}"
    );

    // Once the routed task is done, a report opens a new item that recurs the old one.
    assert!(t.claim(B, &work).await.0.is_success());
    assert!(t.done(B, &work, check(0)).await.0.is_success());
    let recurred = report(
        &t,
        A,
        json!({ "about": "skills:rust", "text": "Clippy flags unknown" }),
    )
    .await;
    assert_eq!(recurred["duplicate"], false);
    assert_eq!(recurred["reports"], 1);
    let new_item = recurred["task"]["id"].as_str().unwrap();
    assert_ne!(new_item, item);
    assert_eq!(recurred["task"]["feedback"]["recurs"], item.as_str());
    // The next report goes to the new item.
    let next = report(
        &t,
        B,
        json!({ "about": "skills:rust", "text": "Clippy flags unknown" }),
    )
    .await;
    assert_eq!(next["task"]["id"], new_item);

    // A declined item stays live; a duplicate passes reports to its original.
    let declined = report(
        &t,
        A,
        json!({ "about": "t3", "text": "Threads load slowly" }),
    )
    .await;
    let declined_id = declined["task"]["id"].as_str().unwrap().to_owned();
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/feedback/{declined_id}/resolve"),
            Some(json!({ "kind": "declined", "reason": "upstream behaviour" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    let more = report(
        &t,
        B,
        json!({ "about": "t3", "text": "Threads load slowly" }),
    )
    .await;
    assert_eq!(more["task"]["id"], declined_id.as_str());
    let dup = report(
        &t,
        C,
        json!({ "about": "t3", "text": "Thread list is slow" }),
    )
    .await;
    let dup_id = dup["task"]["id"].as_str().unwrap().to_owned();
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/feedback/{dup_id}/resolve"),
            Some(json!({ "kind": "duplicate", "task": declined_id })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    let to_original = report(
        &t,
        A,
        json!({ "about": "t3", "text": "Thread list is slow" }),
    )
    .await;
    assert_eq!(to_original["task"]["id"], declined_id.as_str());
    assert_eq!(to_original["reports"], 3);
}

#[tokio::test]
async fn resolve_is_for_the_owner_or_the_user() {
    let t = setup().await;
    let item = report(
        &t,
        A,
        json!({ "about": "storage", "text": "storagectl put hangs" }),
    )
    .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("/v1/feedback/{item}/resolve");
    let declined = json!({ "kind": "declined", "reason": "works as designed" });

    // Unowned: an agent must claim first.
    let (code, err, v) = t.fail("POST", &path, Some(B), Some(declined.clone())).await;
    assert_eq!((code, err.as_str()), (403, "forbidden"));
    assert!(v["next"].as_str().unwrap().contains("task_claim"), "{v}");
    assert!(t.claim(A, &item).await.0.is_success());
    let (code, err, v) = t.fail("POST", &path, Some(B), Some(declined.clone())).await;
    assert_eq!((code, err.as_str()), (403, "forbidden"));
    assert!(v["message"].as_str().unwrap().contains(A), "{v}");

    // Validation.
    let (code, _, _) = t
        .fail("POST", &path, Some(A), Some(json!({ "kind": "routed" })))
        .await;
    assert_eq!(code, 400);
    let (code, _, _) = t
        .fail("POST", &path, Some(A), Some(json!({ "kind": "declined" })))
        .await;
    assert_eq!(code, 400);
    let (code, _, _) = t
        .fail(
            "POST",
            &path,
            Some(A),
            Some(json!({ "kind": "routed", "task": "nowhere-4" })),
        )
        .await;
    assert_eq!(code, 400);

    let (s, done) = resolve(&t, Some(A), &item, declined.clone()).await;
    assert_eq!(s, 200, "{done}");
    assert_eq!(done["status"], "done");
    let resolution = &done["feedback"]["resolution"];
    assert_eq!(resolution["kind"], "declined");
    assert_eq!(resolution["reason"], "works as designed");
    assert_eq!(resolution["by"], A);
    let detail = t.ok("GET", &format!("/v1/tasks/{item}"), None, None).await;
    assert_eq!(detail["evidence"][0]["kind"], "artifact");
    assert_eq!(
        detail["evidence"][0]["value"],
        "feedback declined: works as designed"
    );
    let history: Vec<&Value> = detail["history"].as_array().unwrap().iter().collect();
    let done_event = history.iter().find(|e| e["type"] == "task_done").unwrap();
    assert_eq!(done_event["payload"]["resolution"]["kind"], "declined");

    // A resolved item must be reopened before it is resolved again; reopening clears it.
    let (code, err, _) = t.fail("POST", &path, Some(A), Some(declined.clone())).await;
    assert_eq!((code, err.as_str()), (400, "invalid_transition"));
    assert!(
        t.patch(A, &item, json!({ "status": "open" }))
            .await
            .0
            .is_success()
    );
    assert!(
        t.get_task(&item).await["feedback"]
            .get("resolution")
            .is_none()
    );

    // The user resolves without owning it.
    let (s, v) = t
        .as_user(
            "POST",
            &path,
            Some(json!({ "kind": "forwarded", "reason": "filed upstream as issue 12" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert_eq!(v["feedback"]["resolution"]["by"], "user");

    // Other tasks are not feedback items.
    t.board("factory", &[]).await;
    let plain = t.task(A, "factory", "Plain").await;
    let (code, _, _) = t
        .fail(
            "POST",
            &format!("/v1/feedback/{plain}/resolve"),
            Some(A),
            Some(declined),
        )
        .await;
    assert_eq!(code, 400);
}

#[tokio::test]
async fn reports_are_validated() {
    let t = setup().await;
    for body in [
        json!({ "about": "Bad Slug", "text": "x" }),
        json!({ "about": "a".repeat(41), "text": "x" }),
        json!({ "about": "board", "text": "  " }),
        json!({ "about": "board", "text": "x", "source": "signal" }),
        json!({ "about": "board", "text": "x", "signature": "s".repeat(121) }),
        json!({ "about": "board", "text": "x".repeat(4001) }),
        json!({ "about": "board", "text": "x", "activity": "napping" }),
    ] {
        let (code, _, v) = t
            .fail("POST", "/v1/feedback", Some(A), Some(body.clone()))
            .await;
        assert_eq!(code, 400, "{body}: {v}");
    }
    let (code, _, _) = t
        .fail(
            "POST",
            "/v1/feedback",
            None,
            Some(json!({ "about": "board", "text": "x" })),
        )
        .await;
    assert_eq!(code, 400, "the agent header is required");
    let long = format!("{}\nrest", "word ".repeat(30));
    let filed = report(
        &t,
        A,
        json!({ "about": "board", "text": long, "source": "capture" }),
    )
    .await;
    let title = filed["task"]["title"].as_str().unwrap();
    assert_eq!(title.chars().count(), 72);
    assert!(title.ends_with('…'));
    assert_eq!(filed["task"]["feedback"]["source"], "capture");
}

#[tokio::test]
async fn three_identical_agent_errors_from_two_agents_file_one_signal_item() {
    let t = setup().await;
    let missing = |n: u32| format!("/v1/tasks/factory-{n}");
    t.fail("GET", &missing(1), Some(A), None).await;
    t.fail("GET", &missing(2), Some(A), None).await;
    // Without the agent header, and version conflicts, are not recorded.
    t.fail("GET", &missing(3), None, None).await;
    t.board("factory", &[]).await;
    let task = t.task(A, "factory", "Versioned").await;
    for _ in 0..3 {
        let (s, _) = t
            .patch(A, &task, json!({ "title": "New", "version": 99 }))
            .await;
        assert_eq!(s.as_u16(), 409);
    }
    assert_eq!(t.count("SELECT COUNT(*) FROM api_errors"), 2);
    assert_eq!(
        t.count("SELECT COUNT(*) FROM feedback"),
        0,
        "one agent is not enough"
    );

    t.fail("GET", &missing(4), Some(B), None).await;
    let items = t
        .ok("GET", "/v1/tasks?board=board-feedback", None, None)
        .await;
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    let item = &items[0];
    let fb = &item["feedback"];
    assert_eq!(fb["source"], "signal");
    assert_eq!(fb["about"], "board");
    assert_eq!(fb["reports"], 3);
    assert_eq!(fb["reporters"], json!([A, B]));
    let signature = fb["signature"].as_str().unwrap();
    assert!(signature.starts_with("signal:not_found:"), "{signature}");
    assert_eq!(signature.len(), "signal:not_found:".len() + 12);
    let body = item["body"].as_str().unwrap();
    for part in [
        "3 requests from 2 agents within 7 days",
        "Route: GET /v1/tasks/{id}",
        "task ID does not exist",
        A,
    ] {
        assert!(body.contains(part), "{part} missing from {body}");
    }
    assert!(
        item["title"]
            .as_str()
            .unwrap()
            .starts_with("not_found from GET /v1/tasks/{id}")
    );

    // Later occurrences add to the one item.
    t.fail("GET", &missing(5), Some(C), None).await;
    let item = t.get_task(item["id"].as_str().unwrap()).await;
    assert_eq!(item["feedback"]["reports"], 4);
    assert_eq!(t.count("SELECT COUNT(*) FROM feedback"), 1);

    // Old records are pruned by the sweeper.
    t.advance(TimeDelta::days(8));
    t.state
        .write(|cx| crate::ops::feedback::prune_signals(cx))
        .await
        .unwrap();
    assert_eq!(t.count("SELECT COUNT(*) FROM api_errors"), 0);
}

#[tokio::test]
async fn stats_count_feedback() {
    let t = setup().await;
    let item = report(&t, A, json!({ "about": "board", "text": "One" })).await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    report(&t, B, json!({ "about": "board", "text": "One" })).await;
    report(&t, B, json!({ "about": "board", "text": "Two" })).await;
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/feedback/{item}/resolve"),
            Some(json!({ "kind": "declined", "reason": "no" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    let stats = t.ok("GET", "/v1/stats", None, None).await;
    assert_eq!(
        stats["feedback"],
        json!({
            "reported": 3, "filed": 2,
            "resolved": { "routed": 0, "duplicate": 0, "forwarded": 0, "declined": 1 },
        })
    );
    assert_eq!(
        stats["done_without_check_evidence"], 0,
        "a resolution is not a task done without a check"
    );
}

#[tokio::test]
async fn deleting_the_feedback_board_deletes_its_items() {
    let t = setup().await;
    report(&t, A, json!({ "about": "board", "text": "One" })).await;
    let (s, _) = t
        .as_user(
            "PATCH",
            "/v1/boards/board-feedback",
            Some(json!({ "status": "closed" })),
        )
        .await;
    assert!(s.is_success());
    let (s, _) = t.as_user("DELETE", "/v1/boards/board-feedback", None).await;
    assert_eq!(s.as_u16(), 204);
    assert_eq!(t.count("SELECT COUNT(*) FROM feedback"), 0);
    assert_eq!(t.count("SELECT COUNT(*) FROM feedback_reports"), 0);
    let again = report(&t, A, json!({ "about": "board", "text": "One" })).await;
    assert_eq!(again["duplicate"], false);
    assert_eq!(again["task"]["id"], "board-feedback-2", "numbers continue");
}
