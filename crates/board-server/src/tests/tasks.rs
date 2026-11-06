use board_proto::TaskStatus;
use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{REPO, T, check};
use crate::ops::tasks::transition_allowed;

const A: &str = "t3:alpha";
const B: &str = "t3:beta";

async fn setup() -> T {
    setup_with(|_| {}).await
}

async fn setup_with(configure: impl FnOnce(&mut crate::ServerConfig)) -> T {
    let t = T::with(configure);
    t.register(A, Some(REPO), Some("main")).await;
    t.register(B, Some(REPO), Some("main")).await;
    t.board("factory", &[REPO]).await;
    t
}

/// Puts a fresh task into `status`, owned by A where an owner applies.
async fn task_in(t: &T, status: TaskStatus) -> String {
    let id = t.task(A, "factory", "Exercise a transition").await;
    match status {
        TaskStatus::Open => {}
        TaskStatus::Doing => assert!(t.claim(A, &id).await.0.is_success()),
        TaskStatus::Blocked => {
            assert!(t.claim(A, &id).await.0.is_success());
            let (s, v) = t
                .patch(
                    A,
                    &id,
                    json!({ "status": "blocked", "blocked_reason": "waiting" }),
                )
                .await;
            assert!(s.is_success(), "{v}");
        }
        TaskStatus::Review => {
            assert!(t.claim(A, &id).await.0.is_success());
            assert!(
                t.patch(A, &id, json!({ "status": "review" }))
                    .await
                    .0
                    .is_success()
            );
        }
        TaskStatus::Done => {
            assert!(t.claim(A, &id).await.0.is_success());
            assert!(t.done(A, &id, check(0)).await.0.is_success());
        }
        TaskStatus::Dropped => {
            assert!(
                t.patch(A, &id, json!({ "status": "dropped" }))
                    .await
                    .0
                    .is_success()
            );
        }
    }
    assert_eq!(t.get_task(&id).await["status"], json!(status));
    id
}

#[tokio::test]
async fn every_status_change_follows_the_table() {
    let t = T::with(|c| c.defaults.max_doing_per_agent = 100);
    t.register(A, Some(REPO), Some("main")).await;
    t.board("factory", &[REPO]).await;
    use TaskStatus::*;
    let all = [Open, Doing, Blocked, Review, Done, Dropped];
    let expected_allowed = [
        (Open, Blocked),
        (Open, Dropped),
        (Doing, Blocked),
        (Doing, Review),
        (Doing, Open),
        (Doing, Dropped),
        (Blocked, Open),
        (Blocked, Doing),
        (Blocked, Dropped),
        (Review, Doing),
        (Review, Dropped),
        (Done, Open),
        (Dropped, Open),
    ];
    for from in all {
        for to in all {
            if from == to {
                continue;
            }
            let allowed = expected_allowed.contains(&(from, to));
            assert_eq!(transition_allowed(from, to), allowed, "{from:?} -> {to:?}");
            let id = task_in(&t, from).await;
            let mut body = json!({ "status": to });
            if to == Blocked {
                body["blocked_reason"] = json!("waiting on review");
            }
            let (status, value) = t.patch(A, &id, body).await;
            if allowed {
                assert!(status.is_success(), "{from:?} -> {to:?}: {value}");
                assert_eq!(value["status"], json!(to));
                if to == Open && matches!(from, Doing | Done | Dropped) {
                    assert_eq!(
                        value["owner"],
                        Value::Null,
                        "{from:?} -> open clears the owner"
                    );
                }
            } else {
                assert_eq!(status.as_u16(), 400, "{from:?} -> {to:?}: {value}");
                assert_eq!(value["code"], "invalid_transition");
                let next = value["next"].as_str().unwrap();
                if (from, to) == (Open, Doing) {
                    assert!(next.contains("task_claim"), "{next}");
                }
                if to == Done {
                    assert!(next.contains("task_done"), "{next}");
                }
            }
        }
    }
}

#[tokio::test]
async fn owner_only_transitions() {
    // A holds several tasks at once here.
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 10).await;
    let id = task_in(&t, TaskStatus::Doing).await;
    let (code, kind, _) = t
        .fail(
            "PATCH",
            &format!("/v1/tasks/{id}"),
            Some(B),
            Some(json!({
                "agent": B, "version": t.get_task(&id).await["version"], "status": "open",
            })),
        )
        .await;
    assert_eq!((code, kind.as_str()), (403, "forbidden"));

    let blocked = task_in(&t, TaskStatus::Blocked).await;
    let (s, v) = t.patch(B, &blocked, json!({ "status": "doing" })).await;
    assert_eq!(s.as_u16(), 403, "{v}");

    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/tasks/{id}/release"),
            Some(B),
            Some(json!({ "agent": B })),
        )
        .await;
    assert_eq!(s.as_u16(), 403, "{v}");
    let released = t
        .ok(
            "POST",
            &format!("/v1/tasks/{id}/release"),
            Some(A),
            Some(json!({ "agent": A })),
        )
        .await;
    assert_eq!(released["status"], "open");
    assert_eq!(released["owner"], Value::Null);
}

#[tokio::test]
async fn blocked_needs_a_reason_or_an_open_blocker() {
    let t = setup().await;
    let first = t.task(A, "factory", "First step").await;
    let second = t.task(A, "factory", "Second step").await;
    let (s, v) = t.patch(A, &second, json!({ "status": "blocked" })).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid")),
        "{v}"
    );

    let v = t
        .patch(
            A,
            &second,
            json!({ "status": "blocked", "blockers": [first] }),
        )
        .await
        .1;
    assert_eq!(v["status"], "blocked");
    assert_eq!(v["ready"], false);

    let (s, v) = t.patch(A, &first, json!({ "blocked_reason": "why" })).await;
    assert_eq!(s.as_u16(), 400, "reason without blocked: {v}");
}

#[tokio::test]
async fn done_requires_evidence() {
    // A holds several tasks at once here.
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 10).await;
    let id = task_in(&t, TaskStatus::Doing).await;

    let (s, v) = t.done(A, &id, json!([])).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("evidence_required"))
    );
    let (s, v) = t
        .done(A, &id, json!([{ "kind": "check", "value": "cargo test" }]))
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("evidence_required"))
    );
    assert!(v["next"].as_str().unwrap().contains("exit_code"));

    let (s, v) = t.done(B, &id, check(0)).await;
    assert_eq!(s.as_u16(), 403, "only the owner: {v}");

    let blocked = task_in(&t, TaskStatus::Blocked).await;
    let (s, v) = t.done(A, &blocked, check(0)).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition"))
    );

    for evidence in [
        json!([{ "kind": "commit", "value": "4e1c2ab" }]),
        json!([{ "kind": "pr", "value": "https://example.com/pr/1" }]),
        json!([{ "kind": "artifact", "value": "target/report.html" }]),
    ] {
        let id = task_in(&t, TaskStatus::Review).await;
        let (s, v) = t.done(A, &id, evidence).await;
        assert!(s.is_success(), "{v}");
        assert_eq!(v["status"], "done");
    }
    let (s, v) = t.done(A, &id, check(0)).await;
    assert!(s.is_success(), "{v}");
    let detail = t.ok("GET", &format!("/v1/tasks/{id}"), None, None).await;
    assert_eq!(detail["evidence"][0]["exit_code"], 0);
    assert_eq!(detail["evidence"][0]["agent"], A);
}

#[tokio::test]
async fn claims_follow_ownership_blockers_and_the_doing_limit() {
    let t = setup().await;
    let first = t.task(A, "factory", "First").await;
    let second = t.task(A, "factory", "Second").await;
    let waiting = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Waits", "blockers": [first] }),
        )
        .await;

    let (s, v) = t.claim(A, &first).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("doing"), Some(A))
    );
    // Claiming again is idempotent for the owner.
    assert!(t.claim(A, &first).await.0.is_success());

    let (s, v) = t.claim(B, &first).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (409, Some("claim_refused"))
    );
    assert!(v["message"].as_str().unwrap().contains("owns"), "{v}");
    assert!(v["next"].as_str().unwrap().contains("ready"), "{v}");

    let (s, v) = t.claim(A, &second).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (409, Some("claim_refused"))
    );
    assert!(v["message"].as_str().unwrap().contains(&first), "{v}");

    let (s, v) = t.claim(B, &waiting).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition"))
    );
    assert!(v["message"].as_str().unwrap().contains(&first), "{v}");

    // In review, the first no longer counts against the limit.
    assert!(
        t.patch(A, &first, json!({ "status": "review" }))
            .await
            .0
            .is_success()
    );
    assert!(t.claim(A, &second).await.0.is_success());
    let stats = t.ok("GET", "/v1/stats?days=1", None, None).await;
    assert_eq!(stats["claims_refused"], 2);
}

#[tokio::test]
async fn fifty_concurrent_claims_have_one_winner() {
    let t = T::new();
    t.board("factory", &[]).await;
    let agents: Vec<String> = (0..50).map(|i| format!("claude:racer-{i}")).collect();
    for agent in &agents {
        t.register(agent, None, None).await;
    }
    let id = t.task(&agents[0], "factory", "Contested").await;
    let t = std::sync::Arc::new(t);
    let mut handles = Vec::new();
    for agent in agents.clone() {
        let t = t.clone();
        let id = id.clone();
        handles.push(tokio::spawn(async move { t.claim(&agent, &id).await }));
    }
    let mut winners = Vec::new();
    let mut refused = 0;
    for handle in handles {
        let (status, value) = handle.await.unwrap();
        match status.as_u16() {
            200 => winners.push(value["owner"].as_str().unwrap().to_owned()),
            409 => {
                assert_eq!(value["code"], "claim_refused");
                refused += 1;
            }
            other => panic!("unexpected {other}: {value}"),
        }
    }
    assert_eq!(winners.len(), 1);
    assert_eq!(refused, 49);
    let task = t.get_task(&id).await;
    assert_eq!(task["owner"].as_str(), Some(winners[0].as_str()));
    assert_eq!(task["version"], 2);
    assert_eq!(
        t.count("SELECT COUNT(*) FROM events WHERE type = 'task_claimed'"),
        1
    );
}

#[tokio::test]
async fn stale_owners_can_be_taken_over() {
    let t = setup().await;
    let id = t.task(A, "factory", "Long job").await;
    assert!(t.claim(A, &id).await.0.is_success());

    t.advance(TimeDelta::hours(23));
    t.register(B, Some(REPO), Some("main")).await;
    assert_eq!(t.get_task(&id).await["owner_stale"], false);
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/tasks/{id}/take"),
            Some(B),
            Some(json!({ "agent": B })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (409, Some("claim_refused"))
    );
    assert!(v["message"].as_str().unwrap().contains("24h"), "{v}");

    t.advance(TimeDelta::hours(1) + TimeDelta::minutes(1));
    let task = t.get_task(&id).await;
    assert_eq!(task["owner_stale"], true);
    assert_eq!(task["ready"], false, "doing is never ready");
    // A plain claim points at take.
    let (s, v) = t.claim(B, &id).await;
    assert_eq!(s.as_u16(), 400);
    assert!(v["next"].as_str().unwrap().contains("task_take"), "{v}");

    let taken = t
        .ok(
            "POST",
            &format!("/v1/tasks/{id}/take"),
            Some(B),
            Some(json!({ "agent": B })),
        )
        .await;
    assert_eq!(
        (taken["owner"].as_str(), taken["status"].as_str()),
        (Some(B), Some("doing"))
    );
    assert_eq!(taken["owner_stale"], false);
    let detail = t.ok("GET", &format!("/v1/tasks/{id}"), None, None).await;
    let taken_event = detail["history"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "task_taken")
        .unwrap();
    assert_eq!(taken_event["payload"]["from"], A);
    assert_eq!(taken_event["actor"], B);
    let stats = t.ok("GET", "/v1/stats", None, None).await;
    assert_eq!(stats["tasks_taken_stale"], 1);

    // A paused agent stays live until its reset time, so its tasks are not stale.
    let other = t.task(B, "factory", "Paused owner").await;
    t.ok(
        "POST",
        &format!("/v1/tasks/{id}/release"),
        Some(B),
        Some(json!({ "agent": B })),
    )
    .await;
    assert!(t.claim(B, &other).await.0.is_success());
    let until = (t.state.now() + TimeDelta::hours(30)).to_rfc3339();
    t.ok(
        "POST",
        &format!("/v1/agents/{B}/state"),
        Some(B),
        Some(json!({ "state": "paused", "paused_until": until })),
    )
    .await;
    t.advance(TimeDelta::hours(25));
    assert_eq!(t.get_task(&other).await["owner_stale"], false);
    t.advance(TimeDelta::hours(6));
    assert_eq!(t.get_task(&other).await["owner_stale"], true);
}

#[tokio::test]
async fn ready_is_computed_from_blockers_and_owners() {
    let t = setup().await;
    let blocker = t.task(A, "factory", "Blocker").await;
    let waiting = t
        .task_with(A, json!({ "board": "factory", "title": "Waiting", "blockers": [blocker], "priority": "p1" }))
        .await;
    assert_eq!(t.get_task(&waiting).await["ready"], false);
    assert_eq!(t.get_task(&blocker).await["ready"], true);
    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    assert_eq!(board["counts"]["ready"], 1);
    assert_eq!(board["counts"]["open"], 2);

    assert!(t.claim(A, &blocker).await.0.is_success());
    assert!(t.done(A, &blocker, check(0)).await.0.is_success());
    assert_eq!(t.get_task(&waiting).await["ready"], true);
    let ready = t
        .ok("GET", "/v1/tasks?board=factory&ready=true", None, None)
        .await;
    assert_eq!(ready.as_array().unwrap().len(), 1);
    assert_eq!(ready[0]["id"], waiting.as_str());

    // An open task whose owner is live is not ready; once the owner is stale it is.
    assert!(t.claim(B, &waiting).await.0.is_success());
    t.patch(
        B,
        &waiting,
        json!({ "status": "blocked", "blocked_reason": "need input" }),
    )
    .await;
    let reopened = t.patch(B, &waiting, json!({ "status": "open" })).await.1;
    assert_eq!(reopened["owner"], B);
    assert_eq!(reopened["ready"], false);
    t.advance(TimeDelta::hours(25));
    t.register(A, Some(REPO), Some("main")).await;
    assert_eq!(t.get_task(&waiting).await["ready"], true);

    let mine = t.ok("GET", "/v1/tasks?mine=true", Some(B), None).await;
    assert_eq!(mine.as_array().unwrap().len(), 1);
    let (s, _) = t.call("GET", "/v1/tasks?mine=true", None, None).await;
    assert_eq!(s.as_u16(), 400);
}

#[tokio::test]
async fn versions_must_match() {
    let t = setup().await;
    let id = t.task(A, "factory", "Versioned").await;
    let (s, v) = t
        .patch(A, &id, json!({ "title": "Versioned twice", "version": 1 }))
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["version"], 2);
    let (s, v) = t
        .patch(A, &id, json!({ "title": "Stale write", "version": 1 }))
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (409, Some("version_conflict"))
    );
    assert!(v["next"].as_str().unwrap().contains("version 2"), "{v}");
    assert_eq!(t.get_task(&id).await["title"], "Versioned twice");

    assert!(t.claim(A, &id).await.0.is_success());
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/tasks/{id}/done"),
            Some(A),
            Some(json!({ "agent": A, "version": 2, "evidence": check(0) })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (409, Some("version_conflict"))
    );
}

#[tokio::test]
async fn blockers_stay_on_the_board_and_acyclic() {
    let t = setup().await;
    t.board("other", &[]).await;
    let a = t.task(A, "factory", "A").await;
    let b = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "B", "blockers": [a] }),
        )
        .await;
    let c = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "C", "blockers": [b] }),
        )
        .await;
    let foreign = t.task(A, "other", "Elsewhere").await;

    let (s, v) = t.patch(A, &a, json!({ "blockers": [c] })).await;
    assert_eq!(s.as_u16(), 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains("cycle"), "{v}");
    let (s, v) = t.patch(A, &a, json!({ "blockers": [a] })).await;
    assert_eq!(s.as_u16(), 400, "{v}");
    let (s, v) = t.patch(A, &a, json!({ "blockers": [foreign] })).await;
    assert_eq!(s.as_u16(), 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains("board"), "{v}");
    let (s, v) = t
        .call(
            "POST",
            "/v1/tasks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "title": "Ghost", "blockers": ["factory-99"],
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 400, "{v}");

    let detail = t.ok("GET", &format!("/v1/tasks/{a}"), None, None).await;
    assert_eq!(detail["blocks"], json!([b]));
    let v = t.patch(A, &c, json!({ "blockers": [a, b] })).await.1;
    assert_eq!(v["blockers"], json!([a, b]));
}

#[tokio::test]
async fn titles_and_text_are_checked() {
    let t = setup().await;
    for title in ["", "two\nlines", &"x".repeat(73)] {
        let (s, v) = t
            .call(
                "POST",
                "/v1/tasks",
                Some(A),
                Some(json!({ "agent": A, "board": "factory", "title": title })),
            )
            .await;
        assert_eq!(
            (s.as_u16(), v["code"].as_str()),
            (400, Some("invalid")),
            "{title:?}"
        );
    }
    let id = t.task(A, "factory", "Note me").await;
    let (s, _) = t
        .call(
            "POST",
            &format!("/v1/tasks/{id}/notes"),
            Some(A),
            Some(json!({
                "agent": A, "kind": "finding", "text": "x".repeat(281),
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 400);
    let note = t
        .ok(
            "POST",
            &format!("/v1/tasks/{id}/notes"),
            Some(A),
            Some(json!({
                "agent": A, "kind": "finding", "text": "finding: the parser drops CRLF",
            })),
        )
        .await;
    assert_eq!(note["kind"], "finding");
    let (s, _) = t.call("GET", "/v1/tasks/nope", None, None).await;
    assert_eq!(s.as_u16(), 404);
    let (s, _) = t.call("GET", "/v1/tasks/factory-404", None, None).await;
    assert_eq!(s.as_u16(), 404);
}
