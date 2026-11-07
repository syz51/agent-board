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
        second["task"]["version"], task["version"],
        "a report leaves the version alone, so the owner's updates do not conflict"
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

    // An item dropped without a resolution takes no more reports: a new one recurs it.
    let body = json!({ "about": "hooks", "text": "Hooks print twice" });
    let dropped = report(&t, A, body.clone()).await;
    let dropped_id = dropped["task"]["id"].as_str().unwrap().to_owned();
    let (s, v) = t
        .patch(A, &dropped_id, json!({ "status": "dropped" }))
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let again = report(&t, B, body).await;
    assert_eq!(again["duplicate"], false);
    assert_eq!(again["task"]["feedback"]["recurs"], dropped_id.as_str());
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
    // A reason fits one evidence value with its prefix.
    let (code, _, v) = t
        .fail(
            "POST",
            &path,
            Some(A),
            Some(json!({ "kind": "declined", "reason": "x".repeat(281) })),
        )
        .await;
    assert_eq!(code, 400);
    assert_eq!(
        v["next"],
        "shorten reason to at most 280 characters, or put the detail in a note on the item"
    );
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
async fn a_fixed_item_names_the_fix_and_a_later_report_recurs_it() {
    let t = setup().await;
    let text = json!({ "about": "tools", "text": "The example tool is missing from PATH" });
    let item = report(&t, A, text.clone()).await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(t.claim(A, &item).await.0.is_success());
    let path = format!("/v1/feedback/{item}/resolve");

    // A reason is required, a task refused, and an unknown kind refused; each says what to send.
    let (code, err, v) = t
        .fail("POST", &path, Some(A), Some(json!({ "kind": "fixed" })))
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid"));
    assert!(v["next"].as_str().unwrap().contains("what changed"), "{v}");
    let (code, _, v) = t
        .fail(
            "POST",
            &path,
            Some(A),
            Some(json!({ "kind": "fixed", "reason": "  " })),
        )
        .await;
    assert_eq!(code, 400, "{v}");
    let (code, _, v) = t
        .fail(
            "POST",
            &path,
            Some(A),
            Some(json!({ "kind": "fixed", "reason": "x", "task": item })),
        )
        .await;
    assert_eq!(code, 400);
    assert!(v["next"].as_str().unwrap().contains("routed"), "{v}");
    let (code, _, v) = t
        .fail(
            "POST",
            &path,
            Some(A),
            Some(json!({ "kind": "mended", "reason": "x" })),
        )
        .await;
    assert_eq!(code, 400);
    assert!(v["next"].as_str().unwrap().contains("fixed"), "{v}");

    let fix = "config.toml:12 now adds the tool's folder to PATH";
    let (s, done) = resolve(
        &t,
        Some(A),
        &item,
        json!({ "kind": "fixed", "reason": fix }),
    )
    .await;
    assert_eq!(s, 200, "{done}");
    assert_eq!(done["status"], "done");
    let resolution = &done["feedback"]["resolution"];
    assert_eq!(resolution["kind"], "fixed");
    assert_eq!(resolution["reason"], fix);
    assert!(resolution.get("task").is_none());
    let detail = t.ok("GET", &format!("/v1/tasks/{item}"), None, None).await;
    assert_eq!(
        detail["evidence"][0]["value"],
        format!("feedback fixed: {fix}")
    );

    // Not live: the same report files a new item that recurs the fixed one.
    let again = report(&t, B, text).await;
    assert_eq!(again["duplicate"], false);
    assert_ne!(again["task"]["id"], item.as_str());
    assert_eq!(again["task"]["feedback"]["recurs"], item.as_str());
}

#[tokio::test]
async fn a_plain_task_on_the_feedback_board_resolves_as_an_item() {
    let t = setup().await;
    t.board("factory", &[]).await;
    let fix = t.task(A, "factory", "Fix the hook").await;
    // Filed with task_add, as clients did before feedback items existed.
    t.board("board-feedback", &[]).await;
    let old = t.task(B, "board-feedback", "Hook blocks git fetch").await;
    assert!(t.get_task(&old).await.get("feedback").is_none());

    assert!(t.claim(A, &old).await.0.is_success());
    let (s, done) = resolve(&t, Some(A), &old, json!({ "kind": "routed", "task": fix })).await;
    assert_eq!(s, 200, "{done}");
    assert_eq!(done["status"], "done");
    let feedback = &done["feedback"];
    assert_eq!(feedback["about"], "other");
    assert_eq!(feedback["source"], "agent");
    assert_eq!(feedback["reports"], 1);
    assert_eq!(feedback["reporters"], json!([B]));
    assert_eq!(feedback["resolution"]["kind"], "routed");
    assert_eq!(feedback["resolution"]["task"], fix.as_str());

    // While the routed task is open, the same report adds to the adopted item.
    let again = report(
        &t,
        C,
        json!({ "about": "other", "text": "Hook blocks git fetch" }),
    )
    .await;
    assert_eq!(again["duplicate"], true, "{again}");
    assert_eq!(again["task"]["id"], old.as_str());
    assert_eq!(again["reports"], 2);
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
    // Over-long text says where the rest goes: a report has no body or evidence.
    let (_, _, v) = t
        .fail(
            "POST",
            "/v1/feedback",
            Some(A),
            Some(json!({ "about": "board", "text": "x".repeat(4001) })),
        )
        .await;
    assert_eq!(
        v["next"],
        "shorten text to at most 4000 characters, or keep to what happened and what it cost, \
         and add the rest as notes on the item"
    );
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
    // A bare carriage return ends the title.
    let filed = report(
        &t,
        A,
        json!({ "about": "hooks", "text": "Hook hangs\rsee log" }),
    )
    .await;
    assert_eq!(filed["task"]["title"], "Hook hangs");
    assert_eq!(filed["task"]["feedback"]["signature"], "hooks:hook hangs");
}

#[tokio::test]
async fn three_identical_agent_errors_from_two_agents_file_one_signal_item() {
    let t = setup().await;
    let missing = |n: u32| format!("/v1/tasks/factory-{n}");
    t.fail("GET", &missing(1), Some(A), None).await;
    t.fail("GET", &missing(2), Some(A), None).await;
    // Without the agent header, and version conflicts, are not recorded.
    t.fail("GET", &missing(3), None, None).await;
    // Nor is anything from a caller without a valid token, whatever agent it names.
    for (bearer, agent) in [("Bearer nope", B), ("", C)] {
        let mut headers = vec![("x-board-agent", agent)];
        if !bearer.is_empty() {
            headers.push(("authorization", bearer));
        }
        let (status, _) = t.send("GET", &missing(3), &headers, None).await;
        assert_eq!(status.as_u16(), 401);
        let (status, _) = t.send("GET", "/v1/nowhere", &headers, None).await;
        assert!(status.is_client_error());
    }
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
    let id = item["id"].as_str().unwrap().to_owned();
    let item = t.get_task(&id).await;
    assert_eq!(item["feedback"]["reports"], 4);
    assert_eq!(t.count("SELECT COUNT(*) FROM feedback"), 1);
    assert_eq!(
        t.count("SELECT COUNT(*) FROM feedback_reports"),
        4,
        "every counted error is a report"
    );

    // Expected answers are not friction: probing for an agent or a handoff.
    for agent in [A, B, C] {
        let probe = format!("/v1/agents/{}", agent.replace(':', "%3A"));
        t.fail("GET", &format!("{probe}/handoff"), Some(agent), None)
            .await;
        t.fail("GET", "/v1/agents/t3%3Anobody", Some(agent), None)
            .await;
    }
    assert_eq!(t.count("SELECT COUNT(*) FROM feedback"), 1);

    // Once its fix is done, errors from before the fix do not file a new item; three new
    // ones from two agents do, and it recurs the old one.
    let work = t.task(A, "factory", "Fix the lookup").await;
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/feedback/{id}/resolve"),
            Some(json!({ "kind": "routed", "task": work })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    assert!(t.claim(A, &work).await.0.is_success());
    assert!(t.done(A, &work, check(0)).await.0.is_success());
    t.advance(TimeDelta::minutes(1));
    t.fail("GET", &missing(6), Some(A), None).await;
    assert_eq!(t.count("SELECT COUNT(*) FROM feedback"), 1);
    t.fail("GET", &missing(7), Some(B), None).await;
    t.fail("GET", &missing(8), Some(A), None).await;
    let items = t
        .ok(
            "GET",
            "/v1/tasks?board=board-feedback&status=open",
            None,
            None,
        )
        .await;
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["feedback"]["recurs"], id.as_str());
    assert_eq!(items[0]["feedback"]["reports"], 3);

    // Old records are pruned by the sweeper.
    t.advance(TimeDelta::days(8));
    t.state
        .write(|cx| crate::ops::feedback::prune_signals(cx))
        .await
        .unwrap();
    assert_eq!(t.count("SELECT COUNT(*) FROM api_errors"), 0);
}

#[tokio::test]
async fn refused_claims_are_counted_but_are_not_signals() {
    let t = setup().await;
    t.board("factory", &[]).await;
    // Each agent wins one race and loses two, so three refusals come from each signature's
    // two or more agents.
    for (n, winner) in [A, B, C].into_iter().enumerate() {
        let task = t.task(A, "factory", &format!("Race {n}")).await;
        assert!(t.claim(winner, &task).await.0.is_success());
        for loser in [A, B, C].into_iter().filter(|a| *a != winner) {
            let (s, v) = t.claim(loser, &task).await;
            assert_eq!(
                (s.as_u16(), v["code"].as_str()),
                (409, Some("claim_refused"))
            );
        }
    }
    assert_eq!(t.count("SELECT COUNT(*) FROM api_errors"), 0);
    assert_eq!(t.count("SELECT COUNT(*) FROM feedback"), 0);
    let stats = t.ok("GET", "/v1/stats", None, None).await;
    assert_eq!(stats["claims_refused"], 6);

    // A claim past the doing limit is not a race: it is still recorded.
    let task = t.task(A, "factory", "One too many").await;
    let (s, v) = t.claim(A, &task).await;
    assert_eq!(s.as_u16(), 409, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .starts_with("you already have"),
        "{v}"
    );
    assert_eq!(t.count("SELECT COUNT(*) FROM api_errors"), 1);
}

#[tokio::test]
async fn stats_count_feedback() {
    let t = setup().await;
    let item = report(&t, A, json!({ "about": "board", "text": "One" })).await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    report(&t, B, json!({ "about": "board", "text": "One" })).await;
    let two = report(&t, B, json!({ "about": "board", "text": "Two" })).await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (id, body) in [
        (&item, json!({ "kind": "declined", "reason": "no" })),
        (
            &two,
            json!({ "kind": "fixed", "reason": "raised the limit" }),
        ),
    ] {
        let (s, _) = t
            .as_user("POST", &format!("/v1/feedback/{id}/resolve"), Some(body))
            .await;
        assert_eq!(s.as_u16(), 200);
    }
    let stats = t.ok("GET", "/v1/stats", None, None).await;
    assert_eq!(
        stats["feedback"],
        json!({
            "reported": 3, "filed": 2,
            "resolved": { "routed": 0, "duplicate": 0, "forwarded": 0, "declined": 1, "fixed": 1 },
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
