use board_proto::TaskStatus;
use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{REPO, T, check, defaults, stale_after};
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
                if to == Open {
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

const C: &str = "t3:coord";
const U: &str = "claude:user-session";

/// PATCHes `task` to `status` as `agent`, through the user token when `user`.
async fn move_as(t: &T, agent: &str, user: bool, task: &str, status: &str) -> (u16, Value) {
    let version = t.get_task(task).await["version"].clone();
    let mut body = json!({ "agent": agent, "version": version, "status": status });
    if status == "blocked" {
        body["blocked_reason"] = json!("waiting");
    }
    let path = format!("/v1/tasks/{task}");
    let (s, v) = if user {
        let bearer = format!("Bearer {}", t.user_token);
        t.send(
            "PATCH",
            &path,
            &[("authorization", bearer.as_str()), ("x-board-agent", agent)],
            Some(body),
        )
        .await
    } else {
        t.call("PATCH", &path, Some(agent), Some(body)).await
    };
    (s.as_u16(), v)
}

#[tokio::test]
async fn only_the_owner_coordinator_or_user_change_an_owned_tasks_status() {
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 10).await;
    t.register(C, Some(REPO), None).await;
    t.register(U, None, None).await;

    // Another agent may not drop, block, review, resume or reopen A's task.
    for (from, to) in [
        (TaskStatus::Doing, "dropped"),
        (TaskStatus::Doing, "blocked"),
        (TaskStatus::Doing, "review"),
        (TaskStatus::Doing, "open"),
        (TaskStatus::Blocked, "open"),
        (TaskStatus::Blocked, "doing"),
        (TaskStatus::Blocked, "dropped"),
        (TaskStatus::Review, "doing"),
        (TaskStatus::Review, "dropped"),
    ] {
        let id = task_in(&t, from).await;
        let (s, v) = move_as(&t, B, false, &id, to).await;
        assert_eq!(
            (s, v["code"].as_str()),
            (403, Some("forbidden")),
            "{from:?} -> {to}: {v}"
        );
        assert!(v["message"].as_str().unwrap().contains(A), "{v}");
        assert_eq!(t.get_task(&id).await["owner"], A);
    }
    // Before the coordinator is named, C is just another agent.
    let review = task_in(&t, TaskStatus::Review).await;
    assert_eq!(move_as(&t, C, false, &review, "doing").await.0, 403);

    // Anyone may change an unowned or closed task.
    let open = task_in(&t, TaskStatus::Open).await;
    assert_eq!(move_as(&t, B, false, &open, "blocked").await.0, 200);
    let done = task_in(&t, TaskStatus::Done).await;
    let (s, v) = move_as(&t, B, false, &done, "open").await;
    assert_eq!((s, &v["owner"]), (200, &Value::Null), "{v}");

    // The board's coordinator may; moving to doing makes it the owner.
    t.ok(
        "PATCH",
        "/v1/boards/factory",
        None,
        Some(json!({ "autopilot": true })),
    )
    .await;
    t.ok(
        "POST",
        "/v1/boards/factory/coordinator",
        Some(C),
        Some(json!({ "device": "mac", "agent": C })),
    )
    .await;
    let (s, v) = move_as(&t, C, false, &review, "doing").await;
    assert_eq!((s, v["owner"].as_str()), (200, Some(C)), "{v}");
    let detail = t
        .ok("GET", &format!("/v1/tasks/{review}"), None, None)
        .await;
    let moved = detail["history"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        (
            &moved["payload"]["previous_owner"],
            &moved["payload"]["owner"]
        ),
        (&json!(A), &json!(C)),
        "{moved}"
    );
    assert!(
        moved["payload"]["fields"]
            .as_array()
            .unwrap()
            .contains(&json!("owner")),
        "{moved}"
    );
    // A hears that its task moved away.
    let alerts = t
        .ok(
            "GET",
            &format!("/v1/snapshot?agent={A}&kind=alerts"),
            Some(A),
            None,
        )
        .await;
    assert!(
        alerts["text"].as_str().unwrap().contains(&review),
        "{alerts}"
    );

    let doing = task_in(&t, TaskStatus::Doing).await;
    let (s, v) = move_as(&t, C, false, &doing, "dropped").await;
    assert_eq!((s, v["owner"].as_str()), (200, Some(A)), "{v}");
    let blocked = task_in(&t, TaskStatus::Blocked).await;
    let released = t
        .ok(
            "POST",
            &format!("/v1/tasks/{blocked}/release"),
            Some(C),
            Some(json!({ "agent": C })),
        )
        .await;
    assert_eq!(released["owner"], Value::Null);

    // So may the user, acting through a user token.
    let blocked = task_in(&t, TaskStatus::Blocked).await;
    let (s, v) = move_as(&t, U, true, &blocked, "doing").await;
    assert_eq!((s, v["owner"].as_str()), (200, Some(U)), "{v}");
    let doing = task_in(&t, TaskStatus::Doing).await;
    let (s, v) = move_as(&t, U, true, &doing, "review").await;
    assert_eq!((s, v["owner"].as_str()), (200, Some(A)), "{v}");

    // A stale owner's task is taken first, which is logged.
    let doing = task_in(&t, TaskStatus::Doing).await;
    t.advance(stale_after() + TimeDelta::hours(1));
    t.register(B, Some(REPO), None).await;
    let (s, v) = move_as(&t, B, false, &doing, "dropped").await;
    assert_eq!(s, 403, "{v}");
    assert!(v["next"].as_str().unwrap().contains("take=true"), "{v}");
}

#[tokio::test]
async fn take_refuses_an_open_task_while_a_blocker_is_open() {
    let t = setup().await;
    let blocker = t.task(A, "factory", "Blocker").await;
    let waiting = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Waiting", "blockers": [blocker] }),
        )
        .await;
    // An open task with a stale owner, as rows from before open cleared the owner.
    t.exec(&format!(
        "UPDATE tasks SET owner = '{A}' WHERE id = '{waiting}'"
    ));
    t.advance(stale_after() + TimeDelta::hours(1));
    t.register(B, Some(REPO), None).await;
    let take = |task: String| {
        let t = &t;
        async move {
            t.call(
                "POST",
                &format!("/v1/tasks/{task}/take"),
                Some(B),
                Some(json!({ "agent": B })),
            )
            .await
        }
    };
    let (s, v) = take(waiting.clone()).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains(&blocker), "{v}");
    assert_eq!(t.get_task(&waiting).await["status"], "open");

    t.exec(&format!(
        "UPDATE tasks SET status = 'done' WHERE id = '{blocker}'"
    ));
    let (s, v) = take(waiting.clone()).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("doing"), Some(B))
    );
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
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 1).await;
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
    // The refusal says which parked state fits which wait.
    let next = v["next"].as_str().unwrap();
    assert!(
        next.contains(&format!(
            "task_update {first} status=review, then task_note {first} naming the PR"
        )) && next.contains("status=blocked with blocked_reason")
            && next.contains(&format!(
                "only a date -> release {first} and set its not_before"
            )),
        "{v}"
    );

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
async fn claim_resumes_a_parked_task_that_is_the_callers_or_nobodys() {
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 10).await;

    // The owner's claim resumes its task in review, as task_update status=doing would.
    let reviewed = task_in(&t, TaskStatus::Review).await;
    let (s, v) = t.claim(B, &reviewed).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (409, Some("claim_refused")),
        "{v}"
    );
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains(&format!("owns {reviewed}")),
        "{v}"
    );
    let before = t.get_task(&reviewed).await;
    let (s, v) = t.claim(A, &reviewed).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("doing"), Some(A)),
        "{v}"
    );
    assert_eq!(v["doing_since"], before["doing_since"], "{v}");
    let events = events_of(&t, &reviewed).await;
    let event = events.last().unwrap();
    assert_eq!(
        event,
        &(
            "task_updated".to_owned(),
            json!({ "fields": ["status"], "from": "review", "to": "doing" })
        ),
        "{events:?}"
    );

    // A blocked task resumes once its blockers are done; until then the claim names them.
    let blocker = t.task(A, "factory", "Blocker").await;
    let waiting = t.task(A, "factory", "Waits").await;
    assert!(t.claim(A, &waiting).await.0.is_success());
    let (s, v) = t
        .patch(
            A,
            &waiting,
            json!({ "status": "blocked", "blockers": [blocker] }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.claim(A, &waiting).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains(&blocker), "{v}");
    // The owner is not sent away to other ready work.
    assert!(!v["next"].as_str().unwrap().contains("ready"), "{v}");
    assert!(t.claim(A, &blocker).await.0.is_success());
    let (s, v) = t.done(A, &blocker, check(0)).await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.claim(A, &waiting).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["status"], "doing", "{v}");

    // A blocked reason may name a wait on the user: refused, quoting it.
    let asked = t.task(A, "factory", "Waits on the user").await;
    assert!(t.claim(A, &asked).await.0.is_success());
    let (s, v) = t
        .patch(
            A,
            &asked,
            json!({ "status": "blocked", "blocked_reason": "waiting on ask #12" }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.claim(A, &asked).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    let next = v["next"].as_str().unwrap();
    assert!(
        next.contains("\"waiting on ask #12\"")
            && next.contains(&format!("task_update {asked} status=doing")),
        "{v}"
    );
    assert_eq!(t.get_task(&asked).await["status"], "blocked", "{v}");

    // Nobody's blocked task resumes for whoever claims it, who becomes its owner.
    let first = t.task(A, "factory", "First").await;
    let unowned = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Unowned", "blockers": [first] }),
        )
        .await;
    let (s, v) = t.patch(A, &unowned, json!({ "status": "blocked" })).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["owner"], Value::Null, "{v}");
    assert!(t.claim(A, &first).await.0.is_success());
    let (s, v) = t.done(A, &first, check(0)).await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.claim(B, &unowned).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("doing"), Some(B)),
        "{v}"
    );
    let events = events_of(&t, &unowned).await;
    let event = events.last().unwrap();
    assert_eq!(
        event,
        &(
            "task_updated".to_owned(),
            json!({
                "fields": ["status", "owner"], "from": "blocked", "to": "doing",
                "owner": B, "previous_owner": null,
            })
        ),
        "{events:?}"
    );

    // Nobody's blocked task that also waits on an open ask, a date, or a verdict stays put.
    let blocker = t.task(A, "factory", "Blocker of the rest").await;
    let parked = |title: &'static str, body: &'static str| {
        let t = &t;
        let blocker = blocker.clone();
        async move {
            let id = t
                .task_with(
                    A,
                    json!({ "board": "factory", "title": title, "body": body,
                            "blockers": [blocker] }),
                )
                .await;
            let (s, v) = t.patch(A, &id, json!({ "status": "blocked" })).await;
            assert!(s.is_success(), "{v}");
            id
        }
    };
    let on_ask = parked("Waits on an ask", "").await;
    let dated = parked("Waits for a date", "").await;
    let proposal = parked("Proposed", "Proposal: cache the builds. Size S.").await;
    let later = (t.state.now() + TimeDelta::days(2)).to_rfc3339();
    let (s, v) = t.patch(A, &dated, json!({ "not_before": later })).await;
    assert!(s.is_success(), "{v}");
    assert!(t.claim(A, &blocker).await.0.is_success());
    let (s, v) = t.done(A, &blocker, check(0)).await;
    assert!(s.is_success(), "{v}");
    let ask = t
        .ok(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "task": on_ask, "kind": "decision",
                "question": "Go ahead?", "options": ["yes", "no"],
            })),
        )
        .await;
    for (id, says) in [
        (&on_ask, format!("ask #{}", ask["id"])),
        (&dated, "waits until".to_owned()),
        (&proposal, "proposal not yet accepted".to_owned()),
    ] {
        let (s, v) = t.claim(B, id).await;
        assert_eq!(
            (s.as_u16(), v["code"].as_str()),
            (400, Some("invalid_transition")),
            "{v}"
        );
        assert!(v["message"].as_str().unwrap().contains(&says), "{v}");
        assert_eq!(t.get_task(id).await["status"], "blocked", "{v}");
    }

    // Resumes are not claims from ready.
    let stats = t.ok("GET", "/v1/stats?days=1", None, None).await;
    assert_eq!(stats["claims_refused"], 1, "{stats}");
}

#[tokio::test]
async fn claim_of_a_parked_task_keeps_the_doing_limit_and_stale_owners() {
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 1).await;
    let reviewed = task_in(&t, TaskStatus::Review).await;
    let busy = t.task(A, "factory", "Busy").await;
    assert!(t.claim(A, &busy).await.0.is_success());

    // At the doing limit, the owner's resume is refused like a claim.
    let (s, v) = t.claim(A, &reviewed).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (409, Some("claim_refused")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains(&busy), "{v}");
    assert!(
        v["next"]
            .as_str()
            .unwrap()
            .contains(&format!("then claim {reviewed}")),
        "{v}"
    );
    assert_eq!(t.get_task(&reviewed).await["status"], "review", "{v}");

    // A stale owner's parked task is taken, not claimed.
    t.advance(stale_after() + TimeDelta::hours(1));
    t.register(B, Some(REPO), Some("main")).await;
    let (s, v) = t.claim(B, &reviewed).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(v["next"].as_str().unwrap().contains("take=true"), "{v}");
    assert!(v["message"].as_str().unwrap().contains("stale"), "{v}");
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

    t.advance(stale_after() - TimeDelta::hours(1));
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
    let after = format!("after {}h unseen", stale_after().num_hours());
    assert!(v["message"].as_str().unwrap().contains(&after), "{v}");

    t.advance(TimeDelta::hours(1) + TimeDelta::minutes(1));
    let task = t.get_task(&id).await;
    assert_eq!(task["owner_stale"], true);
    assert_eq!(task["ready"], false, "doing is never ready");
    // A plain claim points at take.
    let (s, v) = t.claim(B, &id).await;
    assert_eq!(s.as_u16(), 400);
    assert!(v["next"].as_str().unwrap().contains("take=true"), "{v}");

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
    let until = (t.state.now() + stale_after() + TimeDelta::hours(6)).to_rfc3339();
    t.ok(
        "POST",
        &format!("/v1/agents/{B}/state"),
        Some(B),
        Some(json!({ "state": "paused", "paused_until": until })),
    )
    .await;
    t.advance(stale_after() + TimeDelta::hours(1));
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

    // Moving a blocked task back to open clears its owner, so it is ready again.
    assert!(t.claim(B, &waiting).await.0.is_success());
    t.patch(
        B,
        &waiting,
        json!({ "status": "blocked", "blocked_reason": "need input" }),
    )
    .await;
    let reopened = t.patch(B, &waiting, json!({ "status": "open" })).await.1;
    assert_eq!(reopened["owner"], json!(null));
    assert_eq!(reopened["ready"], true);

    // An open task whose owner is live (a row from before that rule) is not ready; once
    // the owner is stale it is.
    t.exec(&format!(
        "UPDATE tasks SET owner = '{B}' WHERE id = '{waiting}'"
    ));
    assert_eq!(t.get_task(&waiting).await["ready"], false);
    t.advance(stale_after() + TimeDelta::hours(1));
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
async fn blockers_exist_and_stay_acyclic_across_boards() {
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
    // A blocker on another board is accepted, and a cycle through it is still refused.
    let (s, v) = t.patch(A, &a, json!({ "blockers": [foreign] })).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["blockers"], json!([foreign]));
    let (s, v) = t.patch(A, &foreign, json!({ "blockers": [c] })).await;
    assert_eq!(s.as_u16(), 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains("cycle"), "{v}");
    let (s, v) = t
        .call(
            "POST",
            "/v1/tasks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "title": "Ghost", "blockers": ["other-99"],
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 400, "{v}");
    assert!(v["next"].as_str().unwrap().contains("board=other"), "{v}");
    // A mistyped board is named as missing rather than listed.
    let (s, v) = t.patch(A, &a, json!({ "blockers": ["facotry-1"] })).await;
    assert_eq!(s.as_u16(), 400, "{v}");
    assert!(
        v["next"].as_str().unwrap().contains("no board facotry"),
        "{v}"
    );
    t.patch(A, &a, json!({ "blockers": [] })).await;
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
    // The same set in another order changes nothing.
    let version = v["version"].clone();
    let v = t.patch(A, &c, json!({ "blockers": [b, a] })).await.1;
    assert_eq!((&v["version"], &v["blockers"]), (&version, &json!([a, b])));
}

#[tokio::test]
async fn titles_and_text_are_checked() {
    let t = setup().await;
    for title in ["", "two\nlines", &"x".repeat(defaults().title_chars + 1)] {
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
                "agent": A, "kind": "finding", "text": "x".repeat(defaults().note_chars + 1),
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 400);
    // Notes run past 280 characters with evidence in them; 500 is the default limit.
    assert_eq!(defaults().note_chars, 500);
    t.ok(
        "POST",
        &format!("/v1/tasks/{id}/notes"),
        Some(A),
        Some(json!({ "agent": A, "kind": "finding", "text": "x".repeat(450) })),
    )
    .await;
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

#[tokio::test]
async fn task_history_shows_the_newest_events_oldest_first() {
    let t = setup().await;
    let id = t.task(A, "factory", "Busy task").await;
    for n in 0..210 {
        t.ok(
            "POST",
            &format!("/v1/tasks/{id}/notes"),
            Some(A),
            Some(json!({ "agent": A, "kind": "note", "text": format!("step {n}") })),
        )
        .await;
    }
    t.patch(A, &id, json!({ "title": "Busy task, renamed" }))
        .await;
    let detail = t.ok("GET", &format!("/v1/tasks/{id}"), None, None).await;
    let history = detail["history"].as_array().unwrap();
    assert_eq!(history.len(), crate::store::TASK_HISTORY_LIMIT as usize);
    let seqs: Vec<i64> = history.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "ascending");
    assert_eq!(
        history.last().unwrap()["type"],
        "task_updated",
        "newest kept"
    );
    // Cut: the oldest event, task_created, is gone.
    assert_ne!(history[0]["type"], "task_created");
}

fn announce_not_before(t: &T) -> usize {
    t.state
        .db()
        .write(
            || t.state.now(),
            t.state.settings(),
            crate::ops::tasks::announce_not_before,
        )
        .unwrap()
}

#[tokio::test]
async fn a_task_waits_until_its_not_before() {
    let t = setup().await;
    let at = super::start() + TimeDelta::days(2);
    let waiting = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Later", "not_before": super::iso(at) }),
        )
        .await;
    let task = t.get_task(&waiting).await;
    assert_eq!(
        (task["ready"].as_bool(), task["not_before"].as_str()),
        (Some(false), Some(super::iso(at).as_str()))
    );
    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    assert_eq!(board["counts"]["ready"], 0);
    assert_eq!(
        t.ok("GET", "/v1/tasks?board=factory&ready=true", None, None)
            .await,
        json!([])
    );
    assert_eq!(announce_not_before(&t), 0, "not yet");

    // Ready as soon as the clock passes it, before the sweeper runs.
    t.advance(TimeDelta::days(2));
    assert_eq!(t.get_task(&waiting).await["ready"], true);
    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    assert_eq!(board["counts"]["ready"], 1);

    // The sweeper announces it once with an event by `server` that wakes the coordinator,
    // and leaves the task as it is.
    let since = t.count("SELECT MAX(seq) FROM events");
    let before = t.get_task(&waiting).await;
    assert_eq!(announce_not_before(&t), 1);
    assert_eq!(announce_not_before(&t), 0, "once");
    assert_eq!(t.get_task(&waiting).await, before);
    let page = t
        .ok("GET", &format!("/v1/events?since={since}"), None, None)
        .await;
    assert_eq!(page["events"].as_array().unwrap().len(), 1, "{page}");
    let event = &page["events"][0];
    assert_eq!(
        (
            event["type"].as_str(),
            event["actor"].as_str(),
            event["task"].as_str()
        ),
        (Some("task_updated"), Some("server"), Some(waiting.as_str()))
    );
    assert_eq!(
        event["payload"],
        json!({ "fields": [], "not_before_passed": super::iso(at) })
    );
}

#[tokio::test]
async fn a_new_not_before_is_announced_again_and_closed_tasks_are_not() {
    let t = setup().await;
    let at = super::start() + TimeDelta::hours(1);
    let id = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Twice", "not_before": super::iso(at) }),
        )
        .await;
    t.advance(TimeDelta::hours(2));
    assert_eq!(announce_not_before(&t), 1);

    // Moved later: waits again, and is announced again once that passes.
    let later = super::start() + TimeDelta::hours(5);
    t.patch(A, &id, json!({ "not_before": super::iso(later) }))
        .await;
    assert_eq!(t.get_task(&id).await["ready"], false);
    assert_eq!(announce_not_before(&t), 0);
    t.advance(TimeDelta::hours(4));
    assert_eq!(announce_not_before(&t), 1);

    // A task no longer open becomes due silently.
    let closed = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Closed", "not_before": super::iso(super::start() + TimeDelta::hours(7)) }),
        )
        .await;
    t.exec(&format!(
        "UPDATE tasks SET status = 'blocked' WHERE id = '{closed}'"
    ));
    t.advance(TimeDelta::hours(2));
    let since = t.count("SELECT MAX(seq) FROM events");
    assert_eq!(announce_not_before(&t), 0);
    assert_eq!(t.count("SELECT MAX(seq) FROM events"), since);
}

#[tokio::test]
async fn claim_and_take_refuse_a_task_before_its_not_before() {
    let t = setup().await;
    let at = super::start() + TimeDelta::days(1);
    let id = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Later", "not_before": super::iso(at) }),
        )
        .await;
    let (s, v) = t.claim(A, &id).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("waits until"),
        "{v}"
    );
    assert!(v["next"].as_str().unwrap().contains("not_before"), "{v}");

    t.register(B, Some(REPO), None).await;
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/tasks/{id}/take"),
            Some(B),
            Some(json!({ "agent": B })),
        )
        .await;
    assert_eq!(s.as_u16(), 400, "{v}");
    assert_eq!(t.get_task(&id).await["status"], "open");

    t.advance(TimeDelta::days(1));
    let (s, v) = t.claim(A, &id).await;
    assert!(s.is_success(), "{v}");
}

#[tokio::test]
async fn not_before_is_set_and_cleared_by_update() {
    let t = setup().await;
    let id = t.task(A, "factory", "Hold").await;
    // Sub-millisecond digits are dropped, as the column stores milliseconds.
    let at = super::start() + TimeDelta::days(14);
    let precise = at + TimeDelta::microseconds(250);
    let (s, task) = t
        .patch(
            A,
            &id,
            json!({ "not_before": precise.to_rfc3339_opts(chrono::SecondsFormat::Micros, true) }),
        )
        .await;
    assert!(s.is_success(), "{task}");
    assert_eq!(
        (task["ready"].as_bool(), task["not_before"].as_str()),
        (Some(false), Some(super::iso(at).as_str()))
    );
    assert_eq!(t.get_task(&id).await["not_before"], task["not_before"]);

    // Absent leaves it alone.
    let (_, task) = t.patch(A, &id, json!({ "priority": "p1" })).await;
    assert_eq!(task["not_before"].as_str(), Some(super::iso(at).as_str()));

    // Null clears it; so does a time already passed.
    let (_, task) = t.patch(A, &id, json!({ "not_before": null })).await;
    assert!(
        task.get("not_before").is_none() && task["ready"] == true,
        "{task}"
    );
    t.patch(A, &id, json!({ "not_before": super::iso(at) }))
        .await;
    let past = super::start() - TimeDelta::hours(1);
    let (_, task) = t
        .patch(A, &id, json!({ "not_before": super::iso(past) }))
        .await;
    assert!(task.get("not_before").is_none(), "{task}");

    // Created with a passed time, it does not wait.
    let now = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Now", "not_before": super::iso(past) }),
        )
        .await;
    assert_eq!(t.get_task(&now).await["ready"], true);
}

/// The events recorded for `task`, oldest first, as `(type, payload)`.
async fn events_of(t: &T, task: &str) -> Vec<(String, Value)> {
    let page = t
        .ok("GET", "/v1/events?since=0&limit=1000", None, None)
        .await;
    page["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["task"] == task)
        .map(|e| (e["type"].as_str().unwrap().to_owned(), e["payload"].clone()))
        .collect()
}

#[tokio::test]
async fn a_task_created_with_claim_starts_in_doing_owned_by_its_creator() {
    let t = setup().await;
    let v = t
        .ok(
            "POST",
            "/v1/tasks",
            Some(A),
            Some(json!({ "agent": A, "board": "factory", "title": "Mine", "claim": true })),
        )
        .await;
    let id = v["id"].as_str().unwrap().to_owned();
    assert_eq!(
        (&v["status"], &v["owner"], &v["ready"], &v["version"]),
        (&json!("doing"), &json!(A), &json!(false), &json!(1))
    );
    // Never ready, so nothing announces it to a coordinator; created then claimed.
    let ready = t.ok("GET", "/v1/tasks?ready=true", None, None).await;
    assert!(ready.as_array().unwrap().is_empty(), "{ready}");
    let events = events_of(&t, &id).await;
    let types: Vec<&str> = events.iter().map(|(ty, _)| ty.as_str()).collect();
    assert_eq!(types, ["task_created", "task_claimed"]);
    assert_eq!(events[0].1["claimed"], true);
    assert_eq!(events[1].1, json!({ "previous_owner": null }));
    // It is the owner's like any claimed task: B may not move it, A may finish it.
    let (s, _) = t.patch(B, &id, json!({ "status": "review" })).await;
    assert_eq!(s.as_u16(), 403);
    let (s, v) = t.done(A, &id, check(0)).await;
    assert!(s.is_success(), "{v}");
    let stats = t.ok("GET", "/v1/stats?days=1", None, None).await;
    assert_eq!(stats["doing_minutes_median"].as_f64(), Some(0.0));

    // Without claim, nothing changes: open, no owner, ready.
    let plain = t
        .ok(
            "POST",
            "/v1/tasks",
            Some(A),
            Some(json!({ "agent": A, "board": "factory", "title": "Plain", "claim": false })),
        )
        .await;
    assert_eq!(
        (&plain["status"], &plain["owner"], &plain["ready"]),
        (&json!("open"), &Value::Null, &json!(true))
    );
}

#[tokio::test]
async fn claim_on_create_is_refused_at_the_doing_limit_and_creates_nothing() {
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 1).await;
    let first = t.task(A, "factory", "First").await;
    assert!(t.claim(A, &first).await.0.is_success());
    let before = t.count("SELECT COUNT(*) FROM tasks");
    let seq = t.ok("GET", "/healthz", None, None).await["seq"].clone();

    let body = json!({ "agent": A, "board": "factory", "title": "Second", "claim": true });
    let (code, err, v) = t
        .fail("POST", "/v1/tasks", Some(A), Some(body.clone()))
        .await;
    assert_eq!((code, err.as_str()), (409, "claim_refused"), "{v}");
    assert!(v["message"].as_str().unwrap().contains(&first), "{v}");
    let next = v["next"].as_str().unwrap();
    assert!(next.contains("park it by what it waits on"), "{v}");
    assert!(next.contains(&first), "{v}");
    // Nothing was written: no task, no event, and the number is not used up.
    assert_eq!(t.count("SELECT COUNT(*) FROM tasks"), before);
    assert_eq!(t.ok("GET", "/healthz", None, None).await["seq"], seq);

    // Once the first is in review, the same request succeeds with the next number.
    assert!(
        t.patch(A, &first, json!({ "status": "review" }))
            .await
            .0
            .is_success()
    );
    let v = t.ok("POST", "/v1/tasks", Some(A), Some(body)).await;
    assert_eq!(
        (&v["id"], &v["status"]),
        (&json!("factory-2"), &json!("doing"))
    );
}

#[tokio::test]
async fn claim_on_create_needs_its_blockers_done_and_no_wait() {
    let t = setup().await;
    t.board("other", &[]).await;
    let blocker = t.task(B, "other", "Elsewhere").await;
    let create = |extra: Value| {
        let mut body = json!({ "agent": A, "board": "factory", "title": "Mine", "claim": true });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    };
    let (code, err, v) = t
        .fail(
            "POST",
            "/v1/tasks",
            Some(A),
            Some(create(json!({ "blockers": [blocker] }))),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid_transition"), "{v}");
    assert!(v["message"].as_str().unwrap().contains(&blocker), "{v}");
    assert!(v["next"].as_str().unwrap().contains("without claim"), "{v}");
    let later = super::iso(crate::clock::Clock::now(&t.clock) + TimeDelta::hours(1));
    let (code, err, v) = t
        .fail(
            "POST",
            "/v1/tasks",
            Some(A),
            Some(create(json!({ "not_before": later }))),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid_transition"), "{v}");
    assert_eq!(
        t.count("SELECT COUNT(*) FROM tasks WHERE board = 'factory'"),
        0
    );

    // A done blocker, even on another board, does not hold it.
    assert!(t.claim(B, &blocker).await.0.is_success());
    assert!(t.done(B, &blocker, check(0)).await.0.is_success());
    let v = t
        .ok(
            "POST",
            "/v1/tasks",
            Some(A),
            Some(create(json!({ "blockers": [blocker] }))),
        )
        .await;
    assert_eq!(
        (&v["status"], &v["blockers"]),
        (&json!("doing"), &json!([blocker]))
    );
}

#[tokio::test]
async fn a_blocker_on_another_board_holds_the_task_until_it_is_done() {
    let t = setup().await;
    t.board("other", &[]).await;
    let blocker = t.task(B, "other", "Elsewhere").await;
    let waiting = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Waits", "blockers": [blocker] }),
        )
        .await;
    let task = t.get_task(&waiting).await;
    assert_eq!(
        (&task["ready"], &task["blockers"]),
        (&json!(false), &json!([blocker]))
    );
    let detail = t
        .ok("GET", &format!("/v1/tasks/{blocker}"), None, None)
        .await;
    assert_eq!(detail["blocks"], json!([waiting]));
    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    assert_eq!(board["counts"]["ready"], 0);
    let (s, v) = t.claim(A, &waiting).await;
    assert_eq!(s.as_u16(), 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains(&blocker), "{v}");

    // A task blocked on it waits too, with no reason needed.
    let owned = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Owned", "claim": true }),
        )
        .await;
    let (s, v) = t
        .patch(
            A,
            &owned,
            json!({ "status": "blocked", "blockers": [blocker] }),
        )
        .await;
    assert!(s.is_success(), "{v}");

    assert!(t.claim(B, &blocker).await.0.is_success());
    assert!(t.done(B, &blocker, check(0)).await.0.is_success());
    assert_eq!(t.get_task(&waiting).await["ready"], true);
    let ready = t
        .ok("GET", "/v1/tasks?board=factory&ready=true", None, None)
        .await;
    assert_eq!(ready[0]["id"], json!(waiting));
    // The owner's snapshot says its blocker cleared, though the event is on another board.
    let snap = t
        .ok(
            "GET",
            &format!("/v1/snapshot?agent={A}&kind=alerts"),
            Some(A),
            None,
        )
        .await;
    let text = snap["text"].as_str().unwrap();
    assert!(
        text.contains(&format!("{blocker} done: your blocker is cleared")),
        "{text}"
    );
}

/// PATCHes `task` with `body` as `agent`, through the user token when `user`.
async fn patch_as(t: &T, agent: &str, user: bool, task: &str, mut body: Value) -> (u16, Value) {
    body["agent"] = json!(agent);
    body["version"] = t.get_task(task).await["version"].clone();
    let path = format!("/v1/tasks/{task}");
    let (s, v) = if user {
        let bearer = format!("Bearer {}", t.user_token);
        t.send(
            "PATCH",
            &path,
            &[("authorization", bearer.as_str()), ("x-board-agent", agent)],
            Some(body),
        )
        .await
    } else {
        t.call("PATCH", &path, Some(agent), Some(body)).await
    };
    (s.as_u16(), v)
}

#[tokio::test]
async fn an_owner_hands_a_task_to_the_agent_that_finishes_it() {
    let t = setup().await;
    let id = task_in(&t, TaskStatus::Doing).await;

    let (s, v) = patch_as(&t, A, false, &id, json!({ "owner": B })).await;
    assert_eq!(
        (s, v["owner"].as_str(), v["status"].as_str()),
        (200, Some(B), Some("doing")),
        "{v}"
    );
    let (kind, payload) = events_of(&t, &id).await.pop().unwrap();
    assert_eq!(kind, "task_updated");
    assert_eq!(
        payload,
        json!({ "fields": ["owner"], "owner": B, "previous_owner": A })
    );

    // The old owner can no longer finish it; the new one can.
    assert_eq!(t.done(A, &id, check(0)).await.0.as_u16(), 403);
    let (s, v) = t.done(B, &id, check(0)).await;
    assert_eq!(
        (s.as_u16(), v["status"].as_str()),
        (200, Some("done")),
        "{v}"
    );

    // Handing over to the owner it already has changes nothing.
    let review = task_in(&t, TaskStatus::Review).await;
    let version = t.get_task(&review).await["version"].clone();
    let (s, v) = patch_as(&t, A, false, &review, json!({ "owner": A })).await;
    assert_eq!((s, &v["version"]), (200, &version), "{v}");

    // A task in doing counts against the new owner's doing limit; review does not.
    let busy = task_in(&t, TaskStatus::Doing).await;
    let (s, v) = patch_as(&t, A, false, &busy, json!({ "owner": B })).await;
    assert_eq!(s, 200, "{v}");
    let second = t.task(A, "factory", "Another job").await;
    assert!(t.claim(A, &second).await.0.is_success());
    let (s, v) = patch_as(&t, A, false, &second, json!({ "owner": B })).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    assert!(v["message"].as_str().unwrap().contains(&busy), "{v}");
    let (s, v) = patch_as(&t, A, false, &review, json!({ "owner": B })).await;
    assert_eq!((s, v["owner"].as_str()), (200, Some(B)), "{v}");

    // Resuming a blocked task can name the agent that will work on it.
    assert_eq!(
        patch_as(&t, A, false, &second, json!({ "status": "review" }))
            .await
            .0,
        200
    );
    let blocked = task_in(&t, TaskStatus::Blocked).await;
    let (s, v) = patch_as(
        &t,
        A,
        false,
        &blocked,
        json!({ "status": "doing", "owner": B }),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    assert!(t.done(B, &busy, check(0)).await.0.is_success());
    let (s, v) = patch_as(
        &t,
        A,
        false,
        &blocked,
        json!({ "status": "doing", "owner": B }),
    )
    .await;
    assert_eq!(
        (s, v["owner"].as_str(), v["status"].as_str()),
        (200, Some(B), Some("doing")),
        "{v}"
    );
}

#[tokio::test]
async fn only_the_owner_coordinator_or_user_hand_a_task_over() {
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 10).await;
    t.register(C, Some(REPO), None).await;
    t.register(U, None, None).await;
    let id = task_in(&t, TaskStatus::Review).await;

    // Another agent may neither give itself A's task nor take it from a live owner.
    let (s, v) = patch_as(&t, B, false, &id, json!({ "owner": B })).await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    assert!(v["message"].as_str().unwrap().contains(A), "{v}");
    assert!(v["next"].as_str().unwrap().contains("task_note"), "{v}");
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
        (409, Some("claim_refused")),
        "{v}"
    );
    // Before the coordinator is named, C is just another agent.
    assert_eq!(
        patch_as(&t, C, false, &id, json!({ "owner": C })).await.0,
        403
    );
    assert_eq!(t.get_task(&id).await["owner"], A);

    // The board's coordinator takes back a task its child left, keeping its status.
    t.ok(
        "PATCH",
        "/v1/boards/factory",
        None,
        Some(json!({ "autopilot": true })),
    )
    .await;
    t.ok(
        "POST",
        "/v1/boards/factory/coordinator",
        Some(C),
        Some(json!({ "device": "mac", "agent": C })),
    )
    .await;
    let (s, v) = patch_as(&t, C, false, &id, json!({ "owner": C })).await;
    assert_eq!(
        (s, v["owner"].as_str(), v["status"].as_str()),
        (200, Some(C), Some("review")),
        "{v}"
    );
    assert!(t.done(C, &id, check(0)).await.0.is_success());

    // The user may hand any owned task over.
    let doing = task_in(&t, TaskStatus::Doing).await;
    let (s, v) = patch_as(&t, U, true, &doing, json!({ "owner": B })).await;
    assert_eq!((s, v["owner"].as_str()), (200, Some(B)), "{v}");

    // Only to a registered agent, and not for open or closed tasks.
    let (s, v) = patch_as(&t, B, false, &doing, json!({ "owner": "t3:nobody" })).await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");
    let open = task_in(&t, TaskStatus::Open).await;
    let (s, v) = patch_as(&t, C, false, &open, json!({ "owner": B })).await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    assert!(v["next"].as_str().unwrap().contains("task_claim"), "{v}");
    let (s, v) = patch_as(&t, B, false, &open, json!({ "owner": B })).await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    let (s, v) = patch_as(
        &t,
        C,
        false,
        &doing,
        json!({ "status": "open", "owner": C }),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let (s, v) = patch_as(&t, C, false, &id, json!({ "owner": B })).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert_eq!(t.get_task(&doing).await["owner"], B);
}

#[tokio::test]
async fn a_handover_from_a_stale_owner_says_to_take_it_first() {
    let t = setup().await;
    let id = task_in(&t, TaskStatus::Review).await;
    t.advance(stale_after() + TimeDelta::minutes(1));
    t.register(B, Some(REPO), Some("main")).await;
    let (s, v) = patch_as(&t, B, false, &id, json!({ "owner": B })).await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    assert!(v["next"].as_str().unwrap().contains("take=true"), "{v}");
}

#[tokio::test]
async fn a_handover_needs_an_owned_task_and_a_live_target() {
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 10).await;
    t.register(U, None, None).await;

    // An unowned task is not handed over through a status change either.
    let open = task_in(&t, TaskStatus::Open).await;
    let (s, v) = patch_as(
        &t,
        U,
        true,
        &open,
        json!({ "status": "blocked", "blocked_reason": "x", "owner": B }),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let task = t.get_task(&open).await;
    assert_eq!(
        (&task["status"], &task["owner"]),
        (&json!("open"), &Value::Null)
    );

    // Naming the current owner while releasing does not keep it.
    let doing = task_in(&t, TaskStatus::Doing).await;
    let (s, v) = patch_as(
        &t,
        A,
        false,
        &doing,
        json!({ "status": "open", "owner": A }),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    assert!(
        v["next"].as_str().unwrap().contains("leave owner out"),
        "{v}"
    );
    // Dropping and handing over at once says what the task is moving to.
    let (s, v) = patch_as(
        &t,
        A,
        false,
        &doing,
        json!({ "status": "dropped", "owner": B }),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("moves to dropped"),
        "{v}"
    );
    // A status change the table refuses is reported as such.
    let review = task_in(&t, TaskStatus::Review).await;
    let (s, v) = patch_as(
        &t,
        A,
        false,
        &review,
        json!({ "status": "open", "owner": B }),
    )
    .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(
        v["next"].as_str().unwrap().contains("doing, dropped"),
        "{v}"
    );

    // A target whose session ended, or that is stale, would never finish it.
    t.register("t3:gone", Some(REPO), None).await;
    t.ok(
        "POST",
        "/v1/agents/t3:gone/state",
        Some("t3:gone"),
        Some(json!({ "state": "ended" })),
    )
    .await;
    let (s, v) = patch_as(&t, A, false, &doing, json!({ "owner": "t3:gone" })).await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    assert!(v["message"].as_str().unwrap().contains("not live"), "{v}");
    assert_eq!(t.get_task(&doing).await["owner"], A);
}

#[tokio::test]
async fn the_previous_owner_is_told_to_stop_and_peers_see_the_handover() {
    let t = setup().await;
    t.register(U, None, None).await;
    let id = task_in(&t, TaskStatus::Doing).await;
    t.ok("GET", &format!("/v1/snapshot?agent={A}"), Some(A), None)
        .await;

    let (s, v) = patch_as(&t, U, true, &id, json!({ "owner": B })).await;
    assert_eq!(s, 200, "{v}");
    let alerts = t
        .ok(
            "GET",
            &format!("/v1/snapshot?agent={A}&kind=alerts"),
            Some(A),
            None,
        )
        .await;
    let text = alerts["text"].as_str().unwrap();
    assert!(text.contains(&format!("{id} was handed to")), "{text}");
    assert!(text.contains("stop work on it"), "{text}");

    let (s, v) = patch_as(&t, B, false, &id, json!({ "owner": A })).await;
    assert_eq!(s, 200, "{v}");
    let full = t
        .ok("GET", &format!("/v1/snapshot?agent={A}"), Some(A), None)
        .await;
    let text = full["text"].as_str().unwrap();
    assert!(text.contains(&format!("handed {id} to {A}")), "{text}");
    // Getting it back is a change to the task, not an order to stop.
    assert!(!text.contains("stop work on it"), "{text}");
}

/// Board counts split the ready work a coordinator may start (`ready_auto`) from the ready
/// tasks that wait on the user.
#[tokio::test]
async fn board_counts_split_ready_auto_work_from_ready_work_for_the_user() {
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    for (title, auto) in [
        ("Start me", true),
        ("Also start me", true),
        ("Decide", false),
    ] {
        t.task_with(
            A,
            json!({ "board": "factory", "title": title, "auto": auto }),
        )
        .await;
    }
    // Not ready, so counted in neither: an auto task that waits on another, a claimed one,
    // and a pending proposal.
    let gate = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Gate", "auto": false }),
        )
        .await;
    t.task_with(
        A,
        json!({ "board": "factory", "title": "Later", "auto": true, "blockers": [gate] }),
    )
    .await;
    let doing = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Doing", "auto": true }),
        )
        .await;
    assert!(t.claim(A, &doing).await.0.is_success());
    t.task_with(
        A,
        json!({ "board": "factory", "title": "Cache builds",
                "body": "Proposal: cache the builds. Size S." }),
    )
    .await;

    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    let counts = &board["counts"];
    assert_eq!(
        (&counts["ready"], &counts["ready_auto"]),
        (&json!(4), &json!(2)),
        "{board}"
    );
    let listed = t.ok("GET", "/v1/boards", None, None).await;
    assert_eq!(listed[0]["counts"], *counts, "{listed}");
}

/// dotfiles-48: a pending proposal was claimed and fixed with no verdict. A proposal is
/// stored not auto on every board, is not ready, and cannot be claimed until accepted.
#[tokio::test]
async fn a_pending_proposal_waits_for_a_verdict_before_it_is_work() {
    let t = T::new();
    t.register(A, None, None).await;
    t.register(B, None, None).await;
    t.board("factory", &[]).await;
    t.ok(
        "PATCH",
        "/v1/boards/factory",
        None,
        Some(json!({ "autopilot": true })),
    )
    .await;
    let proposal = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Cache builds", "auto": true,
                    "body": "\nProposal: cache the builds. Size S." }),
        )
        .await;
    let work = t.task(A, "factory", "Ordinary work").await;
    let task = t.get_task(&proposal).await;
    assert_eq!(
        (&task["auto"], &task["proposal"], &task["ready"]),
        (&json!(false), &json!(true), &json!(false)),
        "{task}"
    );
    assert_eq!(t.get_task(&work).await["proposal"], false);

    // Not ready: not listed, not counted.
    let ready = t
        .ok("GET", "/v1/tasks?board=factory&ready=true", None, None)
        .await;
    let ids: Vec<&str> = ready
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [work.as_str()]);
    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    assert_eq!(board["counts"]["ready"], 1, "{board}");
    assert_eq!(board["counts"]["ready_auto"], 1, "{board}");
    // The proposal filter lists it alone, or everything else.
    let only = t
        .ok("GET", "/v1/tasks?board=factory&proposal=true", None, None)
        .await;
    assert_eq!(only.as_array().unwrap().len(), 1);
    assert_eq!(only[0]["id"], proposal.as_str());
    let others = t
        .ok("GET", "/v1/tasks?board=factory&proposal=false", None, None)
        .await;
    assert_eq!(others.as_array().unwrap().len(), 1);
    assert_eq!(others[0]["id"], work.as_str());

    // Claiming it is refused like other work that is not ready yet, and the hint does not
    // tell the claimant, who may be the proposer, to accept it itself.
    let (s, v) = t.claim(A, &proposal).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    let next = v["next"].as_str().unwrap();
    assert!(next.contains("pick other ready work"), "{v}");
    assert!(next.contains("did not file it"), "{v}");
    assert!(!next.contains("auto=true"), "{v}");
    // Nor can it reach doing through blocked.
    let (s, v) = t
        .patch(
            A,
            &proposal,
            json!({ "status": "blocked", "blocked_reason": "parked" }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.patch(A, &proposal, json!({ "status": "doing" })).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    let (s, v) = t.patch(A, &proposal, json!({ "status": "open" })).await;
    assert!(s.is_success(), "{v}");

    // An accept sets auto: then it is ready work and claimable.
    let (s, v) = t.patch(B, &proposal, json!({ "auto": true })).await;
    assert!(s.is_success(), "{v}");
    let task = t.get_task(&proposal).await;
    assert_eq!(
        (&task["proposal"], &task["ready"]),
        (&json!(false), &json!(true))
    );
    assert!(t.claim(B, &proposal).await.0.is_success());
}

#[tokio::test]
async fn a_proposal_cannot_be_created_claimed() {
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let (s, v) = t
        .call(
            "POST",
            "/v1/tasks",
            Some(A),
            Some(json!({ "agent": A, "board": "factory", "title": "Mine",
                         "body": "Proposal: do it myself", "claim": true })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid")),
        "{v}"
    );
    assert!(v["next"].as_str().unwrap().contains("without claim"), "{v}");
}
