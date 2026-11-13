use board_proto::TaskStatus;
use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{REPO, T, check, defaults, stale_after};
use crate::ops::tasks::{status_str, transition_allowed};

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
        (Blocked, Review),
        (Blocked, Dropped),
        (Review, Doing),
        (Review, Blocked),
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
                } else if from != Open || to != Doing {
                    // The hint lists exactly the moves the table allows.
                    let targets: Vec<&str> = all
                        .iter()
                        .filter(|t| expected_allowed.contains(&(from, **t)))
                        .map(|t| status_str(*t))
                        .collect();
                    assert!(next.ends_with(&targets.join(", ")), "{next}");
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

    // A stale owner's task is taken first, which is logged. Nobody runs the board now, and
    // the owner has left, so a drop with a reason needs no take.
    let doing = task_in(&t, TaskStatus::Doing).await;
    t.advance(stale_after() + TimeDelta::hours(1));
    t.register(B, Some(REPO), None).await;
    let (s, v) = move_as(&t, B, false, &doing, "review").await;
    assert_eq!(s, 403, "{v}");
    let next = v["next"].as_str().unwrap();
    assert!(next.starts_with("the owner is stale: take it first"), "{v}");
    assert!(next.contains("reason=<why>"), "{v}");
    let (s, v) = move_as(&t, B, false, &doing, "dropped").await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
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
async fn a_blank_blocked_reason_counts_as_absent() {
    let t = setup().await;
    // Resuming with a blank reason, as agents send it: the move out of blocked clears it.
    for blank in ["", "  "] {
        let id = task_in(&t, TaskStatus::Blocked).await;
        let (s, v) = t
            .patch(
                A,
                &id,
                json!({ "status": "doing", "blocked_reason": blank }),
            )
            .await;
        assert!(s.is_success(), "{v}");
        assert_eq!(
            (&v["status"], &v["blocked_reason"]),
            (&json!("doing"), &Value::Null),
            "{v}"
        );
        // Blank on a task in doing is ignored, not refused as a reason off blocked.
        let (s, v) = t.patch(A, &id, json!({ "blocked_reason": blank })).await;
        assert!(s.is_success(), "{v}");
        let (s, v) = t.done(A, &id, check(0)).await;
        assert!(s.is_success(), "{v}");
    }
    // A task that stays blocked keeps its reason: a blank one sent with other changes,
    // as a tool filling every field does, does not erase it.
    let id = task_in(&t, TaskStatus::Blocked).await;
    let other = t.task(A, "factory", "Other step").await;
    let (s, v) = t
        .patch(A, &id, json!({ "blockers": [other], "blocked_reason": "" }))
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["blocked_reason"], "waiting", "{v}");
    // Blocked with a blank reason and nothing to wait on is still refused, for that.
    let id = task_in(&t, TaskStatus::Doing).await;
    let (s, v) = t
        .patch(A, &id, json!({ "status": "blocked", "blocked_reason": "" }))
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid")),
        "{v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("needs a reason"),
        "{v}"
    );
}

#[tokio::test]
async fn moving_to_doing_waits_on_blockers_and_not_before_as_a_claim_does() {
    let t = setup().await;
    let first = t.task(A, "factory", "First step").await;
    let second = t.task(A, "factory", "Second step").await;
    assert!(t.claim(A, &second).await.0.is_success());
    let (s, v) = t
        .patch(
            A,
            &second,
            json!({ "status": "blocked", "blockers": [first] }),
        )
        .await;
    assert!(s.is_success(), "{v}");

    // A blocker not done refuses the move.
    let (s, v) = t.patch(A, &second, json!({ "status": "doing" })).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains(&format!("{first} (open)")),
        "{v}"
    );
    assert!(
        v["next"]
            .as_str()
            .unwrap()
            .contains(&format!("task_update {second} status=doing blockers=")),
        "{v}"
    );
    assert_eq!(t.get_task(&second).await["status"], "blocked");

    // The same request may drop the blocker it no longer waits on.
    let (s, v) = t
        .patch(A, &second, json!({ "status": "doing", "blockers": [] }))
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["status"], "doing", "{v}");

    // A not_before still ahead refuses it too, until it passes or the request clears it.
    let at = super::iso(super::start() + TimeDelta::days(1));
    let (s, v) = t
        .patch(
            A,
            &second,
            json!({ "status": "blocked", "blocked_reason": "later", "not_before": at }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.patch(A, &second, json!({ "status": "doing" })).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("waits until"),
        "{v}"
    );
    assert!(
        v["next"].as_str().unwrap().contains(&format!(
            "task_update {second} status=doing not_before=none"
        )),
        "{v}"
    );
    let (s, v) = t
        .patch(A, &second, json!({ "status": "doing", "not_before": null }))
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["status"], "doing", "{v}");

    // Review to doing waits the same way, on a blocker and on a date; one refusal names
    // both, and so does its next step.
    let (s, v) = t
        .patch(
            A,
            &second,
            json!({ "status": "review", "blockers": [first], "not_before": at }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.patch(A, &second, json!({ "status": "doing" })).await;
    assert_eq!(s.as_u16(), 400, "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("{first} (open)")) && message.contains("until"),
        "{v}"
    );
    assert!(
        v["next"].as_str().unwrap().contains(&format!(
            "task_update {second} status=doing blockers=[the rest] not_before=none"
        )),
        "{v}"
    );

    // The wait is checked before the doing limit, as a claim does: an agent at its limit
    // is not told to park its task in doing for a move that would be refused anyway.
    let busy = t.task(A, "factory", "Busy").await;
    assert!(t.claim(A, &busy).await.0.is_success());
    let (s, v) = t.patch(A, &second, json!({ "status": "doing" })).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    let (s, v) = t.patch(A, &busy, json!({ "status": "review" })).await;
    assert!(s.is_success(), "{v}");

    // Once the blocker is done and the date passed, the move goes through.
    assert!(t.claim(B, &first).await.0.is_success());
    assert!(t.done(B, &first, check(0)).await.0.is_success());
    t.advance(TimeDelta::days(1));
    let (s, v) = t.patch(A, &second, json!({ "status": "doing" })).await;
    assert!(s.is_success(), "{v}");
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

    let open = task_in(&t, TaskStatus::Open).await;
    let (s, v) = t.done(A, &open, check(0)).await;
    assert_eq!(s.as_u16(), 403, "nobody owns an open task: {v}");

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
async fn a_task_done_in_its_blockers_pr_is_claimed_while_the_blocker_is_in_review() {
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 10).await;
    let review = |title: &'static str| {
        let t = &t;
        async move {
            let id = t.task(A, "factory", title).await;
            assert!(t.claim(A, &id).await.0.is_success());
            let (s, v) = t.patch(A, &id, json!({ "status": "review" })).await;
            assert!(s.is_success(), "{v}");
            id
        }
    };
    let waiting_on = |blocker: &str, parent: Option<&str>| {
        let t = &t;
        let mut body = json!({ "board": "factory", "title": "Folded in", "blockers": [blocker] });
        if let Some(parent) = parent {
            body["parent"] = json!(parent);
        }
        async move { t.task_with(A, body).await }
    };

    // A grandchild waiting on its grandparent in review, A's: another agent may claim it
    // and move it to review alongside, yet it is not ready, so no coordinator starts it.
    let parent = review("Parent").await;
    let child = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Child", "parent": parent }),
        )
        .await;
    let grandchild = waiting_on(&parent, Some(&child)).await;
    assert_eq!(t.get_task(&grandchild).await["ready"], false);
    let (s, v) = t.claim(B, &grandchild).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("doing"), Some(B))
    );
    let (s, v) = t.patch(B, &grandchild, json!({ "status": "review" })).await;
    assert!(s.is_success(), "{v}");
    // Its claim resumes it from review too.
    let (s, v) = t.claim(B, &grandchild).await;
    assert_eq!(
        (s.is_success(), v["status"].as_str()),
        (true, Some("doing")),
        "{v}"
    );

    // A blocker in review that the claiming agent owns, with no parent link.
    let own = review("Own").await;
    let follows = waiting_on(&own, None).await;
    let (s, v) = t.claim(A, &follows).await;
    assert!(s.is_success(), "{v}");

    // The same blocker refuses another agent, naming it and the rule.
    let other = waiting_on(&own, None).await;
    let (s, v) = t.claim(B, &other).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains(&own), "{v}");
    assert!(
        v["next"]
            .as_str()
            .unwrap()
            .contains("a blocker in review that is an ancestor of the task or yours"),
        "{v}"
    );
    // So does an ancestor that is not in review yet.
    let busy = t.task(A, "factory", "Busy parent").await;
    assert!(t.claim(A, &busy).await.0.is_success());
    let early = waiting_on(&busy, Some(&busy)).await;
    let (s, v) = t.claim(B, &early).await;
    assert_eq!(s.as_u16(), 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains(&busy), "{v}");
    // And a task created claimed checks the same way.
    let (s, v) = t
        .call(
            "POST",
            "/v1/tasks",
            Some(A),
            Some(
                json!({ "agent": A, "board": "factory", "title": "Filed and folded",
                         "parent": parent, "blockers": [parent], "claim": true }),
            ),
        )
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["status"], "doing");

    // A move to doing checks the owner the request leaves: B's blocked task waiting on A's
    // blocker in review is refused for B, and allowed when handed to A in the same request.
    let handed = t.task(A, "factory", "Handed over").await;
    assert!(t.claim(B, &handed).await.0.is_success());
    let (s, v) = t
        .patch(
            B,
            &handed,
            json!({ "status": "blocked", "blockers": [own] }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.patch(B, &handed, json!({ "status": "doing" })).await;
    assert_eq!(s.as_u16(), 400, "{v}");
    assert!(
        v["next"]
            .as_str()
            .unwrap()
            .contains("a blocker in review that is an ancestor of the task or yours"),
        "{v}"
    );
    let (s, v) = t
        .patch(B, &handed, json!({ "status": "doing", "owner": A }))
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("doing"), Some(A))
    );

    // A take of an open task with a stale owner follows the same rule.
    let taken = waiting_on(&parent, Some(&parent)).await;
    t.exec(&format!(
        "UPDATE tasks SET owner = '{A}' WHERE id = '{taken}'"
    ));
    t.advance(stale_after() + TimeDelta::hours(1));
    t.register(B, Some(REPO), None).await;
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/tasks/{taken}/take"),
            Some(B),
            Some(json!({ "agent": B })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("doing"), Some(B))
    );
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

    // A reason someone else set on the owner's task may name a wait on the user: the
    // owner's claim is refused, quoting it, and the task keeps the reason.
    let told = t.task(A, "factory", "Told to wait").await;
    assert!(t.claim(A, &told).await.0.is_success());
    let (s, v) = patch_as(
        &t,
        B,
        true,
        &told,
        json!({ "status": "blocked", "blocked_reason": "the user picks the host" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = t.claim(A, &told).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    let next = v["next"].as_str().unwrap();
    assert!(
        next.contains("\"the user picks the host\"")
            && next.contains(&format!("task_update {told} status=doing"))
            && next.contains(&format!("task_done {told}"))
            && !next.contains("status=open"),
        "{v}"
    );
    let kept = t.get_task(&told).await;
    assert_eq!(
        (kept["status"].as_str(), kept["blocked_reason"].as_str()),
        (Some("blocked"), Some("the user picks the host")),
        "{kept}"
    );

    // The owner set this reason itself and judges when it no longer holds: its claim
    // clears the reason and resumes the task, as task_update status=doing does.
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
    // While its ask is open, the task waits on the user: refused, naming the ask.
    let ask = t
        .ok(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "task": asked, "kind": "decision",
                "question": "Go ahead?", "options": ["yes", "no"],
            })),
        )
        .await;
    let (s, v) = t.claim(A, &asked).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains(&format!("waits on ask #{}", ask["id"])),
        "{v}"
    );
    assert_eq!(t.get_task(&asked).await["status"], "blocked", "{v}");
    // Once the ask is closed, the same claim resumes the task.
    t.ok(
        "POST",
        &format!("/v1/asks/{}/withdraw", ask["id"]),
        Some(A),
        Some(json!({ "agent": A })),
    )
    .await;
    let (s, v) = t.claim(A, &asked).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("doing"), Some(A)),
        "{v}"
    );
    assert_eq!(v["blocked_reason"], Value::Null, "{v}");
    let events = events_of(&t, &asked).await;
    assert_eq!(
        events.last().unwrap(),
        &(
            "task_updated".to_owned(),
            json!({
                "fields": ["status", "blocked_reason"], "from": "blocked", "to": "doing"
            })
        ),
        "{events:?}"
    );

    // The owner's own reason does not skip the other checks: a blocker that is not done
    // still refuses the claim, and the reason stays.
    let gate = t.task(A, "factory", "Gate").await;
    let both = t.task(A, "factory", "Reason and blocker").await;
    assert!(t.claim(A, &both).await.0.is_success());
    let (s, v) = t
        .patch(
            A,
            &both,
            json!({ "status": "blocked", "blocked_reason": "needs the gate", "blockers": [gate] }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.claim(A, &both).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains(&gate), "{v}");
    let kept = t.get_task(&both).await;
    assert_eq!(
        (kept["status"].as_str(), kept["blocked_reason"].as_str()),
        (Some("blocked"), Some("needs the gate")),
        "{kept}"
    );

    // Nobody's task blocked with a reason: the hint names clearing it, then claiming.
    let held = t.task(A, "factory", "Held for the user").await;
    let (s, v) = t
        .patch(
            A,
            &held,
            json!({ "status": "blocked", "blocked_reason": "waiting on the user" }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["owner"], Value::Null, "{v}");
    let (s, v) = t.claim(B, &held).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    let next = v["next"].as_str().unwrap();
    assert!(
        next.contains(&format!(
            "task_update {held} status=open, then task_claim {held}"
        )) && !next.contains("task_done"),
        "{v}"
    );
    // The step it names works: open clears the reason, and the claim takes it.
    let (s, v) = t.patch(B, &held, json!({ "status": "open" })).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["blocked_reason"], Value::Null, "{v}");
    let (s, v) = t.claim(B, &held).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("doing"), Some(B)),
        "{v}"
    );

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

/// `POST /done` as `agent` with `take`.
async fn done_take(t: &T, agent: &str, task: &str, evidence: Value) -> (u16, Value) {
    let version = t.get_task(task).await["version"].clone();
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/tasks/{task}/done"),
            Some(agent),
            Some(json!({ "agent": agent, "version": version, "evidence": evidence, "take": true })),
        )
        .await;
    (s.as_u16(), v)
}

#[tokio::test]
async fn done_with_take_closes_a_stale_owners_review_task_outside_the_doing_limit() {
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 1).await;
    let reviewed = task_in(&t, TaskStatus::Review).await;
    let parked = task_in(&t, TaskStatus::Blocked).await;
    let closed = task_in(&t, TaskStatus::Done).await;
    // A task that B created, so B stands in for its owner once the owner's session ends.
    let created = t.task(B, "factory", "Created by the closer").await;
    assert!(t.claim(A, &created).await.0.is_success());
    let (s, v) = t.patch(A, &created, json!({ "status": "review" })).await;
    assert!(s.is_success(), "{v}");
    let working = task_in(&t, TaskStatus::Doing).await;
    let pr = json!([{ "kind": "pr", "value": "https://example.com/pull/1" }]);

    // While the owner is live, take does not pass it by.
    let (s, v) = done_take(&t, B, &reviewed, pr.clone()).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("take refused"),
        "{v}"
    );

    end_session(&t, A).await;
    t.advance(stale_after() + TimeDelta::hours(1));
    t.register(B, Some(REPO), Some("main")).await;
    // A coordinator runs the board, so only staleness lets B close A's task.
    t.register(C, Some(REPO), Some("main")).await;
    let lease_until = (t.state.now() + TimeDelta::days(1)).timestamp_millis();
    t.exec(&format!(
        "UPDATE boards SET coordinator = '{C}', coordinator_device = 'mac', \
         coordinator_expires_at = {lease_until} WHERE slug = 'factory'"
    ));
    let umbrella = t.task(B, "factory", "Umbrella").await;
    assert!(t.claim(B, &umbrella).await.0.is_success());

    // A claim that starts work keeps the doing limit, also as a take.
    let (s, v) = take(&t, B, &reviewed).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    assert!(v["message"].as_str().unwrap().contains(&umbrella), "{v}");
    // The refusal names the one-step close, for a task in review only.
    let one_step = format!("task_done {reviewed} take=true");
    assert!(v["next"].as_str().unwrap().contains(&one_step), "{v}");
    let (s, v) = take(&t, B, &working).await;
    assert_eq!(s, 409, "{v}");
    assert!(!v["next"].as_str().unwrap().contains("take=true"), "{v}");

    // Without take, done is the owner's; with it, the closer shows the work landed.
    let (s, v) = t.done(B, &reviewed, pr.clone()).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (403, Some("forbidden")),
        "{v}"
    );
    assert!(v["next"].as_str().unwrap().contains(&one_step), "{v}");
    let commit = json!([{ "kind": "commit", "value": "abc1234" }]);
    for evidence in [commit.clone(), check(1)] {
        let (s, v) = done_take(&t, B, &reviewed, evidence).await;
        assert_eq!(
            (s, v["code"].as_str()),
            (400, Some("evidence_required")),
            "{v}"
        );
    }
    assert_eq!(t.get_task(&reviewed).await["status"], "review");

    // Only a task in review closes this way: one in progress is taken first.
    let (s, v) = done_take(&t, B, &working, pr.clone()).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(v["next"].as_str().unwrap().contains("take=true"), "{v}");
    assert_eq!(t.get_task(&working).await["owner"], A);
    for (task, status) in [(&parked, "blocked"), (&closed, "done")] {
        let (s, v) = done_take(&t, B, task, pr.clone()).await;
        assert_eq!(
            (s, v["code"].as_str()),
            (400, Some("invalid_transition")),
            "{v}"
        );
        let now = t.get_task(task).await;
        assert_eq!(
            (now["status"].as_str(), now["owner"].as_str()),
            (Some(status), Some(A))
        );
    }

    // A stand-in closes the task as its owner may, stale or not: no take is logged, the
    // owner stays, and any evidence does.
    let (s, v) = done_take(&t, B, &created, commit).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("done"), Some(A)),
        "{v}"
    );
    let events = events_of(&t, &created).await;
    assert!(events.iter().all(|(k, _)| k != "task_taken"), "{events:?}");

    // One call closes it, with the closer as owner, while its own task stays in doing.
    let (s, v) = done_take(&t, B, &reviewed, pr.clone()).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("done"), Some(B)),
        "{v}"
    );
    assert_eq!(t.get_task(&umbrella).await["status"], "doing");
    let events = events_of(&t, &reviewed).await;
    let kinds: Vec<&str> = events
        .iter()
        .rev()
        .take(2)
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(kinds, ["task_done", "task_taken"], "{events:?}");
    assert_eq!(
        events[events.len() - 2].1,
        json!({ "from": A, "status": "review" }),
        "{events:?}"
    );

    // On the caller's own task, take changes nothing.
    let (s, v) = done_take(&t, B, &umbrella, check(0)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["status"], "done", "{v}");
}

#[tokio::test]
async fn done_with_take_closes_a_left_owners_review_task_on_a_board_nobody_runs() {
    let t = setup_with(|c| c.defaults.max_doing_per_agent = 3).await;
    let pr = json!([{ "kind": "pr", "value": "https://example.com/pull/1" }]);
    let refused = |s: u16, v: &Value| {
        assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    };
    let closed_by_b = |s: u16, v: &Value| {
        assert_eq!(s, 200, "{v}");
        assert_eq!(
            (v["status"].as_str(), v["owner"].as_str()),
            (Some("done"), Some(B)),
            "{v}"
        );
    };
    let ended = task_in(&t, TaskStatus::Review).await;
    let unseen = task_in(&t, TaskStatus::Review).await;
    let working = task_in(&t, TaskStatus::Doing).await;
    let umbrella = t.task(B, "factory", "Umbrella").await;
    assert!(t.claim(B, &umbrella).await.0.is_success());

    // The owner was seen a moment ago and its session runs: the task stays its own.
    let (s, v) = done_take(&t, B, &ended, pr.clone()).await;
    refused(s, &v);
    assert!(v["next"].as_str().unwrap().contains("1h unseen"), "{v}");

    // An hour unseen, with no end hook: the owner left, long before it is stale.
    t.advance(TimeDelta::minutes(61));
    t.register(B, Some(REPO), Some("main")).await;
    assert_eq!(t.get_task(&unseen).await["owner_stale"], false);
    // Done without take names the one-step close.
    let (s, v) = t.done(B, &unseen, pr.clone()).await;
    assert_eq!(s.as_u16(), 403, "{v}");
    let one_step = format!("task_done {unseen} take=true");
    assert!(v["next"].as_str().unwrap().contains(&one_step), "{v}");
    // The closer still shows that the work landed, and a task in progress is not closed.
    let commit = json!([{ "kind": "commit", "value": "abc1234" }]);
    let (s, v) = done_take(&t, B, &unseen, commit).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (400, Some("evidence_required")),
        "{v}"
    );
    // A take that starts work keeps the wait until the owner is stale.
    let (s, v) = take(&t, B, &unseen).await;
    refused(s, &v);
    let (s, v) = done_take(&t, B, &unseen, pr.clone()).await;
    closed_by_b(s, &v);
    assert_eq!(t.get_task(&umbrella).await["status"], "doing");
    let events = events_of(&t, &unseen).await;
    assert!(events.iter().any(|(k, _)| k == "task_taken"), "{events:?}");
    // A task still in doing closes the same way, once its work has landed.
    let (s, v) = take(&t, B, &working).await;
    refused(s, &v);
    let (s, v) = done_take(&t, B, &working, pr.clone()).await;
    closed_by_b(s, &v);

    // An idle owner waits, on CI or on the user: it has not left.
    t.register(A, Some(REPO), Some("main")).await;
    let waiting = task_in(&t, TaskStatus::Review).await;
    let state = format!("/v1/agents/{A}/state");
    t.ok("POST", &state, Some(A), Some(json!({ "state": "idle" })))
        .await;
    t.advance(TimeDelta::minutes(61));
    t.register(B, Some(REPO), Some("main")).await;
    let (s, v) = done_take(&t, B, &waiting, pr.clone()).await;
    refused(s, &v);

    // An owner held by a usage limit comes back at the reset, so it has not left.
    let held = t.task(B, "factory", "Held by the limit").await;
    t.register(A, Some(REPO), Some("main")).await;
    assert!(t.claim(A, &held).await.0.is_success());
    let (s, v) = t.patch(A, &held, json!({ "status": "review" })).await;
    assert!(s.is_success(), "{v}");
    let reset = (t.state.now() + TimeDelta::hours(4)).to_rfc3339();
    t.ok(
        "POST",
        &format!("/v1/agents/{A}/state"),
        Some(A),
        Some(json!({ "state": "paused", "paused_until": reset })),
    )
    .await;
    t.advance(TimeDelta::minutes(61));
    t.register(B, Some(REPO), Some("main")).await;
    let (s, v) = done_take(&t, B, &held, pr.clone()).await;
    refused(s, &v);

    // An ended session has left at once. Only an agent on the board's repositories closes
    // its task.
    t.register(A, Some(REPO), Some("main")).await;
    end_session(&t, A).await;
    let far = "claude:far";
    t.register(far, Some("github.com/example/other"), Some("main"))
        .await;
    let (s, v) = done_take(&t, far, &ended, pr.clone()).await;
    refused(s, &v);
    let (s, v) = done_take(&t, B, &ended, pr.clone()).await;
    closed_by_b(s, &v);

    // On a board that a coordinator runs, the coordinator closes it: others wait.
    let run = t.task(A, "factory", "On a board with a coordinator").await;
    t.register(A, Some(REPO), Some("main")).await;
    assert!(t.claim(A, &run).await.0.is_success());
    let (s, v) = t.patch(A, &run, json!({ "status": "review" })).await;
    assert!(s.is_success(), "{v}");
    end_session(&t, A).await;
    t.register("claude:other", Some(REPO), Some("main")).await;
    let lease_until = (t.state.now() + TimeDelta::days(1)).timestamp_millis();
    t.exec(&format!(
        "UPDATE boards SET coordinator = '{B}', coordinator_device = 'mac', \
         coordinator_expires_at = {lease_until} WHERE slug = 'factory'"
    ));
    let (s, v) = done_take(&t, "claude:other", &run, pr.clone()).await;
    refused(s, &v);
    assert!(!v["next"].as_str().unwrap().contains("nobody runs"), "{v}");

    // So does the daemon, on a board that it dispatches.
    t.exec(
        "UPDATE boards SET coordinator = NULL, coordinator_device = NULL, \
         coordinator_expires_at = NULL WHERE slug = 'factory'",
    );
    let dispatch = json!({ "autopilot": true, "dispatch": "daemon" });
    t.ok("PATCH", "/v1/boards/factory", None, Some(dispatch))
        .await;
    let (s, v) = done_take(&t, "claude:other", &run, pr.clone()).await;
    refused(s, &v);
    t.ok(
        "PATCH",
        "/v1/boards/factory",
        None,
        Some(json!({ "autopilot": false })),
    )
    .await;
    let (s, v) = done_take(&t, "claude:other", &run, pr).await;
    assert_eq!(s, 200, "{v}");
}

#[tokio::test]
async fn a_left_owners_task_is_dropped_with_a_reason_on_a_board_nobody_runs() {
    let t = setup().await;
    let drop = |reason: Option<&str>| match reason {
        Some(reason) => json!({ "status": "dropped", "reason": reason }),
        None => json!({ "status": "dropped" }),
    };
    let forbidden = |s: u16, v: &Value| {
        assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
        assert!(
            !v["message"]
                .as_str()
                .unwrap()
                .contains("while its session runs"),
            "{v}"
        );
    };
    let parked = task_in(&t, TaskStatus::Blocked).await;
    let why = "superseded by the new design";

    // The owner's session runs: the task is its own.
    let (s, v) = patch_as(&t, B, false, &parked, drop(Some(why))).await;
    forbidden(s, &v);
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("its session is running"),
        "{v}"
    );

    // An idle owner waits, on CI or on the user: it has not left.
    let state = format!("/v1/agents/{A}/state");
    t.ok("POST", &state, Some(A), Some(json!({ "state": "idle" })))
        .await;
    t.advance(TimeDelta::hours(2));
    t.register(B, Some(REPO), Some("main")).await;
    let (s, v) = patch_as(&t, B, false, &parked, drop(Some(why))).await;
    forbidden(s, &v);

    // Its session ended: it has left, long before it is stale.
    t.register(A, Some(REPO), Some("main")).await;
    end_session(&t, A).await;
    t.advance(TimeDelta::minutes(5));
    t.register(B, Some(REPO), Some("main")).await;
    assert_eq!(t.get_task(&parked).await["owner_stale"], false);

    // An agent off the board's repositories may not drop it.
    let far = "claude:far";
    t.register(far, Some("github.com/example/other"), Some("main"))
        .await;
    let (s, v) = patch_as(&t, far, false, &parked, drop(Some(why))).await;
    forbidden(s, &v);
    assert!(v["message"].as_str().unwrap().contains("has ended"), "{v}");

    // On a board that a coordinator runs, the coordinator drops it: others wait.
    let lease_until = (t.state.now() + TimeDelta::days(1)).timestamp_millis();
    t.register("t3:coord", Some(REPO), Some("main")).await;
    t.exec(&format!(
        "UPDATE boards SET coordinator = 't3:coord', coordinator_device = 'mac', \
         coordinator_expires_at = {lease_until} WHERE slug = 'factory'"
    ));
    let (s, v) = patch_as(&t, B, false, &parked, drop(Some(why))).await;
    forbidden(s, &v);
    assert!(!v["next"].as_str().unwrap().contains("nobody runs"), "{v}");
    t.exec(
        "UPDATE boards SET coordinator = NULL, coordinator_device = NULL, \
         coordinator_expires_at = NULL WHERE slug = 'factory'",
    );

    // A take or a move that starts work still waits until the owner is stale, and the
    // refusals name the drop.
    let (s, v) = take(&t, B, &parked).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    assert!(v["next"].as_str().unwrap().contains("reason=<why>"), "{v}");
    let (s, v) = patch_as(&t, B, false, &parked, json!({ "status": "doing" })).await;
    forbidden(s, &v);
    assert!(v["next"].as_str().unwrap().contains("reason=<why>"), "{v}");

    // The drop needs a reason, and a blank one counts as none.
    for reason in [None, Some("  ")] {
        let (s, v) = patch_as(&t, B, false, &parked, drop(reason)).await;
        assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
        let hint = format!("task_update {parked} status=dropped reason=<why>");
        assert!(v["next"].as_str().unwrap().contains(&hint), "{v}");
    }
    // A reason is only for a drop.
    let (s, v) = patch_as(
        &t,
        B,
        false,
        &parked,
        json!({ "status": "review", "reason": why }),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    assert_eq!(t.get_task(&parked).await["status"], "blocked");

    // With the reason, the drop goes through and records why.
    let (s, v) = patch_as(&t, B, false, &parked, drop(Some(why))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["status"], "dropped", "{v}");
    let detail = t
        .ok("GET", &format!("/v1/tasks/{parked}"), None, None)
        .await;
    let notes = detail["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n["kind"] == "decision" && n["text"] == why && n["agent"] == B),
        "{notes:?}"
    );
    let events = events_of(&t, &parked).await;
    let dropped = events
        .iter()
        .rev()
        .find(|(k, _)| k == "task_updated")
        .unwrap();
    assert_eq!(dropped.1["left_owner"], A, "{events:?}");
    assert_eq!(dropped.1["to"], "dropped", "{events:?}");
}

#[tokio::test]
async fn the_owner_drops_its_task_with_a_reason_kept_as_a_note() {
    let t = setup().await;
    let id = task_in(&t, TaskStatus::Doing).await;
    let (s, v) = patch_as(
        &t,
        A,
        false,
        &id,
        json!({ "status": "dropped", "reason": "done elsewhere" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let detail = t.ok("GET", &format!("/v1/tasks/{id}"), None, None).await;
    let notes = detail["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n["kind"] == "decision" && n["text"] == "done elsewhere"),
        "{notes:?}"
    );
    let events = events_of(&t, &id).await;
    assert!(
        events.iter().all(|(_, p)| p.get("left_owner").is_none()),
        "{events:?}"
    );

    // A reason sent again for the task already dropped changes nothing else, but is kept.
    let (s, v) = patch_as(
        &t,
        A,
        false,
        &id,
        json!({ "status": "dropped", "reason": "superseded by the new plan" }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let detail = t.ok("GET", &format!("/v1/tasks/{id}"), None, None).await;
    let decisions: Vec<_> = detail["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["kind"] == "decision")
        .map(|n| n["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        decisions,
        ["done elsewhere", "superseded by the new plan"],
        "{detail}"
    );

    // A reason meant for a block is pointed at blocked_reason.
    let other = task_in(&t, TaskStatus::Doing).await;
    let (s, v) = patch_as(
        &t,
        A,
        false,
        &other,
        json!({ "status": "blocked", "reason": "waits on CI" }),
    )
    .await;
    assert_eq!(s, 400, "{v}");
    assert_eq!(v["code"], "invalid", "{v}");
    assert!(
        v["next"].as_str().unwrap().contains("blocked_reason"),
        "{v}"
    );
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
    // Notes run past 500 characters with evidence in them (ask #98); 800 is the default
    // limit, and a title takes 80.
    assert_eq!((defaults().note_chars, defaults().title_chars), (800, 80));
    t.task(A, "factory", &"x".repeat(80)).await;
    let (s, v) = t
        .call(
            "PATCH",
            &format!("/v1/tasks/{id}"),
            Some(A),
            Some(json!({
                "agent": A, "version": t.get_task(&id).await["version"], "title": "y".repeat(80),
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    t.ok(
        "POST",
        &format!("/v1/tasks/{id}/notes"),
        Some(A),
        Some(json!({ "agent": A, "kind": "finding", "text": "x".repeat(800) })),
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
async fn a_coordinator_takes_an_ended_childs_review_task_by_handle_or_full_id() {
    let t = setup().await;
    let coordinator = "t3:mcp:launch:0123abcd";
    let handle = t.register(coordinator, Some(REPO), None).await["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(handle, coordinator);
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
        Some(coordinator),
        Some(json!({ "device": "mac", "agent": coordinator })),
    )
    .await;
    let by_handle = task_in(&t, TaskStatus::Review).await;
    let by_id = task_in(&t, TaskStatus::Review).await;
    t.ok(
        "POST",
        &format!("/v1/agents/{A}/state"),
        Some(A),
        Some(json!({ "state": "ended" })),
    )
    .await;

    // An unknown handle is named as such, and the task stays with its owner.
    let (s, v) = patch_as(
        &t,
        coordinator,
        false,
        &by_handle,
        json!({ "owner": "t3:ffff" }),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (404, Some("not_found")), "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("t3:ffff is not registered"),
        "{v}"
    );
    assert_eq!(t.get_task(&by_handle).await["owner"], A);

    for (id, owner) in [(&by_handle, handle.as_str()), (&by_id, coordinator)] {
        // The refusal names the coordinator's step: the handover to itself.
        let (s, v) = t.done(coordinator, id, check(0)).await;
        assert_eq!(s.as_u16(), 403, "{v}");
        assert!(
            v["next"]
                .as_str()
                .unwrap()
                .contains(&format!("task_update {id} owner={coordinator}")),
            "{v}"
        );
        let (s, v) = patch_as(&t, coordinator, false, id, json!({ "owner": owner })).await;
        assert_eq!(
            (s, v["owner"].as_str(), v["status"].as_str()),
            (200, Some(coordinator), Some("review")),
            "{v}"
        );
        // Clients tie a handle they sent to the full id through the detail's handles.
        let detail = t.ok("GET", &format!("/v1/tasks/{id}"), None, None).await;
        assert_eq!(detail["handles"][coordinator], handle.as_str(), "{detail}");
        let (s, v) = t.done(coordinator, id, check(0)).await;
        assert_eq!(
            (s.as_u16(), v["status"].as_str()),
            (200, Some("done")),
            "{v}"
        );
    }
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
        v["next"]
            .as_str()
            .unwrap()
            .contains("doing, blocked, dropped"),
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

#[tokio::test]
async fn the_task_list_narrows_to_a_parents_children_with_their_statuses() {
    let t = setup().await;
    t.board("other", &[]).await;
    let parent = t.task(A, "factory", "Umbrella").await;
    let child = |title: &'static str, priority: &'static str| json!({ "board": "factory", "title": title, "parent": parent, "priority": priority });
    let first = t.task_with(A, child("First child", "p2")).await;
    let second = t.task_with(A, child("Second child", "p1")).await;
    let third = t.task_with(A, child("Third child", "p2")).await;
    // A grandchild is not a direct child; a task elsewhere is no child at all.
    let grandchild = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Grandchild", "parent": first }),
        )
        .await;
    t.task(A, "factory", "Unrelated").await;
    assert!(t.claim(A, &second).await.0.is_success());
    assert!(t.claim(B, &third).await.0.is_success());
    assert!(t.done(B, &third, check(0)).await.0.is_success());

    let ids = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let path = format!("/v1/tasks?parent={parent}");
    let all = t.ok("GET", &path, None, None).await;
    // List order: priority, then number.
    assert_eq!(ids(&all), [second.as_str(), first.as_str(), third.as_str()]);
    // The filter combines with the others.
    let open = t
        .ok("GET", &format!("{path}&status=open"), None, None)
        .await;
    assert_eq!(ids(&open), [first.as_str()]);
    let ready = t.ok("GET", &format!("{path}&ready=true"), None, None).await;
    assert_eq!(ids(&ready), [first.as_str()]);
    let mine = t
        .ok("GET", &format!("{path}&mine=true"), Some(A), None)
        .await;
    assert_eq!(ids(&mine), [second.as_str()]);
    let owned = t.ok("GET", &format!("{path}&owner={B}"), None, None).await;
    assert_eq!(ids(&owned), [third.as_str()]);
    let here = t
        .ok("GET", &format!("{path}&board=factory"), None, None)
        .await;
    assert_eq!(ids(&here).len(), 3);
    let elsewhere = t
        .ok("GET", &format!("{path}&board=other"), None, None)
        .await;
    assert_eq!(ids(&elsewhere), Vec::<String>::new());
    let grand = t
        .ok("GET", &format!("/v1/tasks?parent={first}"), None, None)
        .await;
    assert_eq!(ids(&grand), [grandchild.as_str()]);
    // A task without children, or one that does not exist, has an empty list.
    let none = t
        .ok("GET", &format!("/v1/tasks?parent={grandchild}"), None, None)
        .await;
    assert_eq!(ids(&none), Vec::<String>::new());
    let none = t
        .ok("GET", "/v1/tasks?parent=factory-999", None, None)
        .await;
    assert_eq!(ids(&none), Vec::<String>::new());
    let (s, v) = t
        .call("GET", "/v1/tasks?parent=not%20an%20id", None, None)
        .await;
    assert_eq!(s.as_u16(), 400, "{v}");

    // Every task read counts children, whatever their status; none is left out.
    let listed = t.ok("GET", "/v1/tasks?board=factory", None, None).await;
    let count = |id: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .unwrap()
            .get("child_count")
            .cloned()
    };
    assert_eq!(count(&parent), Some(json!(3)));
    assert_eq!(count(&first), Some(json!(1)));
    assert_eq!(count(&second), None);

    // The detail lists the children by number, with their titles and statuses.
    let detail = t
        .ok("GET", &format!("/v1/tasks/{parent}"), None, None)
        .await;
    assert_eq!(detail["task"]["child_count"], 3);
    assert_eq!(detail["children"], json!([first, second, third]));
    assert_eq!(
        detail["child_tasks"],
        json!([
            { "id": first, "title": "First child", "status": "open", "ready": true,
              "owner": null, "owner_stale": false },
            { "id": second, "title": "Second child", "status": "doing", "ready": false,
              "owner": A, "owner_stale": false },
            { "id": third, "title": "Third child", "status": "done", "ready": false,
              "owner": B, "owner_stale": false },
        ])
    );
    let leaf = t
        .ok("GET", &format!("/v1/tasks/{second}"), None, None)
        .await;
    assert_eq!(leaf["children"], json!([]));
    assert!(leaf.get("child_tasks").is_none(), "{leaf}");
    assert!(leaf["task"].get("child_count").is_none(), "{leaf}");
}

#[tokio::test]
async fn a_parent_on_another_board_is_refused_with_the_alternatives() {
    let t = setup().await;
    t.board("other", &[]).await;
    let elsewhere = t.task(A, "other", "Elsewhere").await;
    let (s, v) = t
        .call(
            "POST",
            "/v1/tasks",
            Some(A),
            Some(json!({ "agent": A, "board": "factory", "title": "Child",
                         "parent": elsewhere })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid")),
        "{v}"
    );
    let next = v["next"].as_str().unwrap();
    assert!(next.contains("name it in the body, or make that task wait for this one by adding this one to its blockers"), "{v}");
}

#[tokio::test]
async fn the_owner_closes_its_blocked_task_while_another_is_in_doing() {
    // The default limit: one task in doing.
    let t = setup().await;
    let blocked = task_in(&t, TaskStatus::Blocked).await;
    let busy = task_in(&t, TaskStatus::Doing).await;
    assert_eq!(t.get_task(&blocked).await["blocked_reason"], "waiting");

    // Neither another agent nor the board's coordinator closes A's blocked task.
    t.register(C, Some(REPO), None).await;
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
    for other in [B, C] {
        let (s, v) = t.done(other, &blocked, check(0)).await;
        assert_eq!(
            (s.as_u16(), v["code"].as_str()),
            (403, Some("forbidden")),
            "{other}: {v}"
        );
    }
    assert_eq!(t.get_task(&blocked).await["status"], "blocked");

    let (s, v) = t.done(A, &blocked, json!([])).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("evidence_required")),
        "{v}"
    );
    let (s, v) = t.done(A, &blocked, check(0)).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("done"), Some(A)),
        "{v}"
    );
    assert_eq!(v["blocked_reason"], Value::Null, "{v}");
    assert_eq!(t.get_task(&busy).await["status"], "doing");
    // Done again says it is closed and how to reopen it, not to claim it.
    let (s, v) = t.done(A, &blocked, check(0)).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(
        v["next"]
            .as_str()
            .unwrap()
            .contains(&format!("task_update {blocked} status=open")),
        "{v}"
    );

    // A blocked task closes only once what it still names as its wait is settled.
    assert!(t.done(A, &busy, check(0)).await.0.is_success());
    let asked = task_in(&t, TaskStatus::Blocked).await;
    let ask = t
        .ok(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "task": asked, "kind": "decision",
                "question": "Ship it?", "options": ["yes", "no"],
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let (s, v) = t.done(A, &asked, check(0)).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    let next = v["next"].as_str().unwrap();
    assert!(
        next.contains(&format!("ask_withdraw {ask}"))
            && next.contains(&format!("board withdraw {ask}")),
        "{v}"
    );
    t.ok(
        "POST",
        &format!("/v1/asks/{ask}/withdraw"),
        Some(A),
        Some(json!({ "agent": A })),
    )
    .await;
    let (s, v) = t.done(A, &asked, check(0)).await;
    assert!(s.is_success(), "{v}");

    let waits = task_in(&t, TaskStatus::Blocked).await;
    let blocker = t.task(A, "factory", "Still open").await;
    let (s, v) = t.patch(A, &waits, json!({ "blockers": [blocker] })).await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.done(A, &waits, check(0)).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains(&blocker), "{v}");
    let (s, v) = t.patch(A, &waits, json!({ "blockers": [] })).await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.done(A, &waits, check(0)).await;
    assert!(s.is_success(), "{v}");

    // Nobody's blocked task is not closed by whoever passes: it is claimed first.
    let first = t.task(A, "factory", "First").await;
    let unowned = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Unowned", "blockers": [first] }),
        )
        .await;
    let (s, v) = t.patch(A, &unowned, json!({ "status": "blocked" })).await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.done(A, &unowned, check(0)).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (403, Some("forbidden")),
        "{v}"
    );
    assert!(v["next"].as_str().unwrap().contains("task_claim"), "{v}");
}

#[tokio::test]
async fn a_parked_task_moves_between_review_and_blocked() {
    let t = setup().await;
    let id = task_in(&t, TaskStatus::Review).await;
    // A reason or an unfinished blocker is still required.
    let (s, v) = t.patch(A, &id, json!({ "status": "blocked" })).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid")),
        "{v}"
    );
    let (s, v) = t
        .patch(
            A,
            &id,
            json!({ "status": "blocked", "blocked_reason": "waits on the user's merge" }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (
            v["status"].as_str(),
            v["owner"].as_str(),
            v["blocked_reason"].as_str()
        ),
        (Some("blocked"), Some(A), Some("waits on the user's merge")),
        "{v}"
    );
    // It parks in review again without going through doing; the reason goes with blocked.
    let (s, v) = t
        .patch(
            A,
            &id,
            json!({ "status": "review", "blocked_reason": "still waiting" }),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid")),
        "{v}"
    );
    let (s, v) = t.patch(A, &id, json!({ "status": "review" })).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        (v["status"].as_str(), v["owner"].as_str()),
        (Some("review"), Some(A)),
        "{v}"
    );
    assert_eq!(v["blocked_reason"], Value::Null, "{v}");

    // A blocker alone also parks it in blocked.
    let blocker = t.task(A, "factory", "Deploy it").await;
    let (s, v) = t
        .patch(
            A,
            &id,
            json!({ "status": "blocked", "blockers": [blocker] }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["status"], "blocked", "{v}");

    // Another agent may not move A's parked task.
    let (s, v) = t.patch(B, &id, json!({ "status": "review" })).await;
    assert_eq!(s.as_u16(), 403, "{v}");

    // A move between review and blocked is no reopen in the stats.
    let stats = t.ok("GET", "/v1/stats?days=7", None, None).await;
    assert_eq!(stats["boards"][0]["reopened_after_review"], 0, "{stats}");

    // Nobody's blocked task, such as an unaccepted proposal, does not wait in review: it
    // would have no owner to finish it, and would skip the proposal gate on doing.
    for body in ["Unowned", "Proposal: cache the builds. Size S."] {
        let unowned = t
            .task_with(
                A,
                json!({ "board": "factory", "title": "Unowned", "body": body, "auto": false }),
            )
            .await;
        let (s, v) = t
            .patch(
                B,
                &unowned,
                json!({ "status": "blocked", "blocked_reason": "waiting" }),
            )
            .await;
        assert!(s.is_success(), "{v}");
        assert_eq!(v["owner"], Value::Null, "{v}");
        let (s, v) = t.patch(B, &unowned, json!({ "status": "review" })).await;
        assert_eq!(
            (s.as_u16(), v["code"].as_str()),
            (400, Some("invalid_transition")),
            "{v}"
        );
        assert!(
            v["next"]
                .as_str()
                .unwrap()
                .contains(&format!("task_update {unowned} status=doing")),
            "{v}"
        );
        assert_eq!(t.get_task(&unowned).await["status"], "blocked");
    }
}

#[tokio::test]
async fn a_claim_held_by_an_open_ask_names_how_to_end_it() {
    let t = setup().await;
    let id = task_in(&t, TaskStatus::Doing).await;
    let ask = t
        .ok(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "task": id, "kind": "decision", "question": "Which one?",
                "options": ["this", "that"],
            })),
        )
        .await;
    let ask = ask["id"].as_i64().unwrap();
    let (s, v) = t.patch(A, &id, json!({ "status": "review" })).await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t.claim(A, &id).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    let next = v["next"].as_str().unwrap();
    assert!(
        next.contains(&format!("ask #{ask}"))
            && next.contains(&format!("ask_withdraw {ask}; CLI: board withdraw {ask}")),
        "{v}"
    );
}

// A delegation chain: GRANDPARENT delegated PARENT, which delegated CHILD.
const GRANDPARENT: &str = "t3:grand";
const PARENT: &str = "t3:thread:delegated-task:g1-p1";
const CHILD: &str = "t3:thread:delegated-task:p1-c1";
const STAND_IN: &str = "the task's creator and the agents that delegated its owner may take, \
     move or close it once the owner's session has ended";

/// Registers `agent` as a T3 thread that `parent` delegated, as the hooks do.
async fn register_delegated(t: &T, agent: &str, parent: &str) {
    t.ok(
        "POST",
        "/v1/agents",
        Some(agent),
        Some(json!({
            "id": agent, "runtime": "claude", "device": "mac", "repo": REPO, "branch": "main",
            "native_ids": {"t3_thread": &agent[3..], "t3_parent": &parent[3..]},
        })),
    )
    .await;
}

async fn end_session(t: &T, agent: &str) {
    t.ok(
        "POST",
        &format!("/v1/agents/{agent}/state"),
        Some(agent),
        Some(json!({ "state": "ended" })),
    )
    .await;
}

async fn take(t: &T, agent: &str, task: &str) -> (u16, Value) {
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/tasks/{task}/take"),
            Some(agent),
            Some(json!({ "agent": agent })),
        )
        .await;
    (s.as_u16(), v)
}

/// A task A created, that CHILD claimed and parked in review.
async fn childs_task(t: &T) -> String {
    let id = t.task(A, "factory", "Delegated work").await;
    assert!(t.claim(CHILD, &id).await.0.is_success());
    let (s, v) = t.patch(CHILD, &id, json!({ "status": "review" })).await;
    assert!(s.is_success(), "{v}");
    id
}

async fn delegation(t: &T) {
    t.register(GRANDPARENT, Some(REPO), Some("main")).await;
    register_delegated(t, PARENT, GRANDPARENT).await;
    register_delegated(t, CHILD, PARENT).await;
}

#[tokio::test]
async fn the_creator_or_a_delegating_parent_takes_an_ended_childs_task_at_once() {
    let t = setup().await;
    delegation(&t).await;
    let ids = [
        childs_task(&t).await,
        childs_task(&t).await,
        childs_task(&t).await,
    ];

    // While the child runs, nobody takes its task, its parents and the creator included.
    for agent in [PARENT, GRANDPARENT, A, B] {
        let (s, v) = take(&t, agent, &ids[0]).await;
        assert_eq!(
            (s, v["code"].as_str()),
            (409, Some("claim_refused")),
            "{agent}: {v}"
        );
        assert!(v["next"].as_str().unwrap().contains(STAND_IN), "{v}");
    }

    end_session(&t, CHILD).await;
    assert_eq!(
        t.get_task(&ids[0]).await["owner_stale"],
        false,
        "an owner whose session just ended is not stale"
    );
    // An unrelated agent still waits until the owner is stale.
    let (s, v) = take(&t, B, &ids[0]).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    // A plain claim by the parent names the take, and is not counted as refused.
    let (s, v) = t.claim(PARENT, &ids[0]).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition")),
        "{v}"
    );
    assert!(v["next"].as_str().unwrap().contains("take=true"), "{v}");

    // The parent, the agent that delegated the parent, and the creator take at once.
    for (agent, id) in [(PARENT, &ids[0]), (GRANDPARENT, &ids[1]), (A, &ids[2])] {
        let (s, v) = take(&t, agent, id).await;
        assert_eq!(
            (s, v["owner"].as_str(), v["status"].as_str()),
            (200, Some(agent), Some("doing")),
            "{agent}: {v}"
        );
    }
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM events WHERE type = 'task_taken' \
             AND json_extract(payload, '$.from') = '{CHILD}' \
             AND json_extract(payload, '$.stand_in') = 1"
        )),
        3
    );
    // None of them was stale, nor a refused claim; B's refused take is not a claim either.
    let stats = t.ok("GET", "/v1/stats", None, None).await;
    assert_eq!(
        (
            stats["tasks_taken_stale"].as_u64(),
            stats["claims_refused"].as_u64()
        ),
        (Some(0), Some(0)),
        "{stats}"
    );
}

#[tokio::test]
async fn a_delegating_parent_closes_moves_or_hands_back_an_ended_childs_task() {
    let t = setup().await;
    delegation(&t).await;
    // The parent works on its own task meanwhile, so taking a task into doing hits its
    // doing limit; closing, moving and handing over do not need the take.
    let own = t.task(PARENT, "factory", "Parent's own work").await;
    assert!(t.claim(PARENT, &own).await.0.is_success());
    let [closed, moved, handed, revived] = [
        childs_task(&t).await,
        childs_task(&t).await,
        childs_task(&t).await,
        childs_task(&t).await,
    ];
    end_session(&t, CHILD).await;

    // An unrelated agent is refused on every route, with a hint naming who may act. Nobody
    // runs this board, so for the task in review that is the one-step close.
    let (s, v) = t.done(B, &closed, check(0)).await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (403, Some("forbidden")));
    let one_step = format!("task_done {closed} take=true");
    assert!(v["next"].as_str().unwrap().contains(&one_step), "{v}");
    // An agent on the board's repositories drops it only with a reason, and one elsewhere
    // not at all.
    let (s, v) = patch_as(&t, B, false, &moved, json!({ "status": "dropped" })).await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    assert!(v["next"].as_str().unwrap().contains("reason=<why>"), "{v}");
    let far = "claude:far";
    t.register(far, Some("github.com/example/other"), Some("main"))
        .await;
    let (s, v) = patch_as(&t, far, false, &moved, json!({ "status": "dropped" })).await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    assert!(v["next"].as_str().unwrap().contains(STAND_IN), "{v}");
    let (s, v) = patch_as(&t, B, false, &handed, json!({ "owner": B })).await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    assert!(v["next"].as_str().unwrap().contains(STAND_IN), "{v}");

    let (s, v) = take(&t, PARENT, &closed).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    let (s, v) = t.done(PARENT, &closed, check(0)).await;
    assert_eq!(
        (s.as_u16(), v["status"].as_str()),
        (200, Some("done")),
        "{v}"
    );
    let (s, v) = patch_as(
        &t,
        PARENT,
        false,
        &moved,
        json!({ "status": "blocked", "blocked_reason": "waits on the deploy" }),
    )
    .await;
    assert_eq!(
        (s, v["status"].as_str(), v["owner"].as_str()),
        (200, Some("blocked"), Some(CHILD)),
        "{v}"
    );
    let (s, v) = patch_as(&t, PARENT, false, &handed, json!({ "owner": PARENT })).await;
    assert_eq!(
        (s, v["status"].as_str(), v["owner"].as_str()),
        (200, Some("review"), Some(PARENT)),
        "{v}"
    );

    // A child that acts again runs again, and its task is its own once more.
    register_delegated(&t, CHILD, PARENT).await;
    let (s, v) = t.done(PARENT, &revived, check(0)).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (403, Some("forbidden")),
        "{v}"
    );
    let (s, v) = take(&t, PARENT, &revived).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
}

async fn release(t: &T, agent: &str, task: &str) -> (u16, Value) {
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/tasks/{task}/release"),
            Some(agent),
            Some(json!({ "agent": agent })),
        )
        .await;
    (s.as_u16(), v)
}

#[tokio::test]
async fn the_creator_releases_hands_over_or_closes_an_ended_owners_task() {
    let t = setup().await;
    delegation(&t).await;
    let [released, handed, closed] = [
        childs_task(&t).await,
        childs_task(&t).await,
        childs_task(&t).await,
    ];
    // A feedback item's creator is only its reporter: it does not stand in.
    let item = t
        .ok(
            "POST",
            "/v1/feedback",
            Some(A),
            Some(json!({ "about": "board", "text": "the snapshot hangs" })),
        )
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(t.claim(CHILD, &item).await.0.is_success());
    assert_eq!(t.get_task(&item).await["created_by"], A);

    // While the child runs, the creator is refused like anyone else.
    let (s, v) = release(&t, A, &released).await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("its session is running"),
        "{v}"
    );
    assert!(v["next"].as_str().unwrap().contains(STAND_IN), "{v}");

    end_session(&t, CHILD).await;
    let (s, v) = release(&t, A, &released).await;
    assert_eq!(
        (s, v["status"].as_str(), v["owner"].as_str()),
        (200, Some("open"), None),
        "{v}"
    );
    let (s, v) = patch_as(&t, A, false, &handed, json!({ "owner": B })).await;
    assert_eq!(
        (s, v["status"].as_str(), v["owner"].as_str()),
        (200, Some("review"), Some(B)),
        "{v}"
    );
    let (s, v) = t.done(A, &closed, check(0)).await;
    assert_eq!(
        (s.as_u16(), v["status"].as_str()),
        (200, Some("done")),
        "{v}"
    );

    let (s, v) = take(&t, A, &item).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    let (s, v) = t.done(A, &item, check(0)).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (403, Some("forbidden")),
        "{v}"
    );
    // The refusal says the session ended, and why that is not enough.
    let message = v["message"].as_str().unwrap();
    assert!(message.contains("its session has ended"), "{v}");
    assert!(!message.contains("while its session runs"), "{v}");
    // The parent that delegated its owner still stands in.
    let (s, v) = take(&t, PARENT, &item).await;
    assert_eq!((s, v["owner"].as_str()), (200, Some(PARENT)), "{v}");
}

#[tokio::test]
async fn nobody_stands_in_while_the_owner_may_come_back() {
    let t = setup().await;
    delegation(&t).await;
    let grandchild = "t3:thread:delegated-task:c1-g1";
    let [delegating, limited] = [childs_task(&t).await, childs_task(&t).await];

    // The ended child's own delegate still works: its task waits for it.
    register_delegated(&t, grandchild, CHILD).await;
    end_session(&t, CHILD).await;
    let (s, v) = take(&t, PARENT, &delegating).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    end_session(&t, grandchild).await;
    let (s, v) = take(&t, PARENT, &delegating).await;
    assert_eq!((s, v["owner"].as_str()), (200, Some(PARENT)), "{v}");

    // A session that ended waiting for a usage-limit reset comes back after it.
    register_delegated(&t, CHILD, PARENT).await;
    let reset = t.state.now() + TimeDelta::hours(6);
    t.ok(
        "POST",
        &format!("/v1/agents/{CHILD}/state"),
        Some(CHILD),
        Some(json!({ "state": "paused", "paused_until": reset.to_rfc3339() })),
    )
    .await;
    end_session(&t, CHILD).await;
    let (s, v) = take(&t, A, &limited).await;
    assert_eq!((s, v["code"].as_str()), (409, Some("claim_refused")), "{v}");
    let (s, v) = t.done(A, &limited, check(0)).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (403, Some("forbidden")),
        "{v}"
    );
    t.advance(TimeDelta::hours(7));
    assert_eq!(t.get_task(&limited).await["owner_stale"], false);
    let (s, v) = take(&t, A, &limited).await;
    assert_eq!((s, v["owner"].as_str()), (200, Some(A)), "{v}");
}

#[tokio::test]
async fn q_lists_the_tasks_that_hold_every_word() {
    let t = setup().await;
    let title = t.task(A, "factory", "Rotate the Signing key").await;
    let body = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Second", "body": "the signing step fails",
                    "acceptance": "the KEY is rotated" }),
        )
        .await;
    let noted = t.task(A, "factory", "Third").await;
    t.ok(
        "POST",
        &format!("/v1/tasks/{noted}/notes"),
        Some(A),
        Some(json!({ "agent": A, "kind": "finding", "text": "Signing needs a new key" })),
    )
    .await;
    let other = t.task(A, "factory", "Signing without the other word").await;
    let ids = |v: Value| -> Vec<String> {
        let mut ids = listed(&v);
        ids.sort();
        ids
    };

    // Every word must be there, in the title, body, acceptance or a note, in any case.
    let found = t.ok("GET", "/v1/tasks?q=KEY%20signing", None, None).await;
    let mut want = vec![title.clone(), body.clone(), noted.clone()];
    want.sort();
    assert!(
        found
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task.get("any_word").is_none())
    );
    assert_eq!(ids(found), want);
    // A task id finds its task first, and q combines with the other filters.
    let found = t
        .ok("GET", &format!("/v1/tasks?q={other}"), None, None)
        .await;
    assert_eq!(listed(&found)[0], other);
    assert!(t.claim(A, &title).await.0.is_success());
    let found = t
        .ok("GET", "/v1/tasks?q=rotate+signing&status=doing", None, None)
        .await;
    assert_eq!(ids(found), vec![title.clone()]);
    // A word goes on (a prefix), and words are stemmed in both directions.
    for q in ["sign", "signed", "rotat", "rotation"] {
        let found = t.ok("GET", &format!("/v1/tasks?q={q}"), None, None).await;
        assert!(listed(&found).contains(&title), "{q}: {found}");
    }
    // The middle of a word is no match, and a word of signs alone finds nothing. Query
    // syntax is plain text.
    for q in ["otate", "%25", "-%3E", "%22", "title%3Asigning"] {
        let found = t.ok("GET", &format!("/v1/tasks?q={q}"), None, None).await;
        assert!(!listed(&found).contains(&other), "{q}: {found}");
    }
    let found = t.ok("GET", "/v1/tasks?q=%25", None, None).await;
    assert_eq!(found, json!([]));
    // Signs around a word are left out; the parts of a word stand in a row.
    let named = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Fourth", "body": "see `crates/board/src/mcp.rs`" }),
        )
        .await;
    for q in ["mcp.rs", "(mcp.rs)", "%22mcp.rs%22", "-%3E%20mcp.rs"] {
        let found = t.ok("GET", &format!("/v1/tasks?q={q}"), None, None).await;
        assert_eq!(listed(&found), vec![named.clone()], "{q}");
    }
    let found = t.ok("GET", "/v1/tasks?q=rs.mcp", None, None).await;
    assert_eq!(found, json!([]));

    // Letters fold in every script.
    let accent = t.task(A, "factory", "Visit the \u{c9}cole").await;
    let found = t.ok("GET", "/v1/tasks?q=%C3%A9cole", None, None).await;
    assert_eq!(ids(found), vec![accent]);
    // A blank q is no search.
    let found = t.ok("GET", "/v1/tasks?q=%20", None, None).await;
    assert_eq!(found.as_array().unwrap().len(), 6);

    for q in [&"x".repeat(201), "a%20b%20c%20d%20e%20f%20g%20h%20i"] {
        let (s, code, v) = t.fail("GET", &format!("/v1/tasks?q={q}"), None, None).await;
        assert_eq!((s, code.as_str()), (400, "invalid"), "{v}");
    }
}

#[tokio::test]
async fn q_finds_an_id_whole_and_ranks_a_title_above_text_however_long_the_task() {
    let t = setup().await;
    let mut ids = Vec::new();
    for n in 1..=12 {
        ids.push(t.task(A, "factory", &format!("Item {n}")).await);
    }
    let (one, twelve) = (ids[0].clone(), ids[11].clone());
    t.ok(
        "POST",
        &format!("/v1/tasks/{twelve}/notes"),
        Some(A),
        Some(json!({ "agent": A, "kind": "note", "text": format!("follows {one}") })),
    )
    .await;
    // An id is no prefix of another id: the task itself first, then the one that names it.
    let found = t.ok("GET", &format!("/v1/tasks?q={one}"), None, None).await;
    assert_eq!(listed(&found), vec![one.clone(), twelve.clone()], "{found}");
    // A part of an id, such as the board's slug, does not find each task of the board.
    // It finds only the task whose note names an id of that board.
    let found = t.ok("GET", "/v1/tasks?q=factory", None, None).await;
    assert_eq!(listed(&found), vec![twelve.clone()]);

    // A long task with the word in its title stays above a short one with it in its body.
    let long = t.task(A, "factory", "Lantern upkeep").await;
    for n in 0..30 {
        let text = format!(
            "report {n}: {}",
            "nothing of note happened on this run ".repeat(15)
        );
        t.ok(
            "POST",
            &format!("/v1/tasks/{long}/notes"),
            Some(A),
            Some(json!({ "agent": A, "kind": "note", "text": text })),
        )
        .await;
    }
    let short = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Short", "body": "lantern" }),
        )
        .await;
    let found = t.ok("GET", "/v1/tasks?q=lantern", None, None).await;
    assert_eq!(listed(&found), vec![long, short]);
}

#[tokio::test]
async fn q_lists_the_best_match_first_with_the_text_found() {
    let t = setup().await;
    // Made in the reverse of their relevance, at one priority.
    let in_note = t.task(A, "factory", "Third").await;
    t.ok(
        "POST",
        &format!("/v1/tasks/{in_note}/notes"),
        Some(A),
        Some(json!({ "agent": A, "kind": "note", "text": "the daemon\nwas restarted here" })),
    )
    .await;
    let in_body = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Second",
                    "body": "Long text before the word. Then restart the daemon on each device, and more text after the word that the snippet leaves out because it is far away from it." }),
        )
        .await;
    let in_title = t.task(A, "factory", "Restart the daemon").await;
    t.task(A, "factory", "No word of the search").await;

    let found = t.ok("GET", "/v1/tasks?q=restart+daemon", None, None).await;
    assert_eq!(
        listed(&found),
        vec![in_title.clone(), in_body.clone(), in_note.clone()],
        "title, then body, then notes"
    );
    // The title needs no snippet; the others carry a short one line with the words marked.
    assert_eq!(found[0].get("snippet"), None, "{found}");
    let snippet = found[1]["snippet"].as_str().unwrap();
    assert!(snippet.contains("[restart] the [daemon]"), "{snippet}");
    assert!(snippet.chars().count() < 120, "{snippet}");
    assert!(!snippet.contains("far away"), "{snippet}");
    assert_eq!(found[2]["snippet"], "the [daemon] was [restarted] here");
    // Priority orders the list only among equal matches.
    let urgent = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Urgent", "priority": "p1",
                    "body": "restart the daemon" }),
        )
        .await;
    let found = t.ok("GET", "/v1/tasks?q=restart+daemon", None, None).await;
    assert_eq!(listed(&found)[0], in_title);
    assert!(listed(&found).contains(&urgent));
    // Without a search there is no snippet.
    let all = t.ok("GET", "/v1/tasks", None, None).await;
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .all(|task| task.get("snippet").is_none())
    );

    // A page continues after a task by its rank.
    let first = t
        .ok("GET", "/v1/tasks?q=restart+daemon&limit=2", None, None)
        .await;
    let rest = t
        .ok(
            "GET",
            &format!("/v1/tasks?q=restart+daemon&after={}", listed(&first)[1]),
            None,
            None,
        )
        .await;
    assert_eq!(
        [listed(&first), listed(&rest)].concat(),
        listed(&found),
        "the pages are the list"
    );
    let last = listed(&found).pop().unwrap();
    let end = t
        .ok(
            "GET",
            &format!("/v1/tasks?q=restart+daemon&after={last}"),
            None,
            None,
        )
        .await;
    assert_eq!(
        end,
        json!([]),
        "the end of the list is no reason for any word"
    );
    // A cursor the search does not list is refused.
    let unlisted = t.task(A, "factory", "Elsewhere").await;
    let (s, code, v) = t
        .fail(
            "GET",
            &format!("/v1/tasks?q=restart+daemon&after={unlisted}"),
            None,
            None,
        )
        .await;
    assert_eq!((s, code.as_str()), (400, "invalid"), "{v}");
}

#[tokio::test]
async fn q_without_a_task_of_every_word_lists_the_tasks_of_any_word() {
    let t = setup().await;
    let both = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "One", "body": "the backup of the keychain" }),
        )
        .await;
    let one = t.task(A, "factory", "Backup the database").await;
    t.task(A, "factory", "Neither").await;

    // No task holds all three words: each task with one of them is listed and marked.
    let found = t
        .ok("GET", "/v1/tasks?q=backup+keychain+zebra", None, None)
        .await;
    assert_eq!(
        listed(&found),
        vec![both.clone(), one.clone()],
        "more words first"
    );
    assert!(
        found
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["any_word"] == true)
    );
    // It pages as the full search does.
    let rest = t
        .ok(
            "GET",
            &format!("/v1/tasks?q=backup+keychain+zebra&after={both}"),
            None,
            None,
        )
        .await;
    assert_eq!(listed(&rest), vec![one.clone()]);
    assert_eq!(rest[0]["any_word"], true);
    // A task of every word ends the fallback, and no word at all lists nothing.
    let found = t.ok("GET", "/v1/tasks?q=backup+keychain", None, None).await;
    assert_eq!(listed(&found), vec![both]);
    assert_eq!(found[0].get("any_word"), None);
    let found = t.ok("GET", "/v1/tasks?q=zebra+quagga", None, None).await;
    assert_eq!(found, json!([]));
    // The other filters hold in the fallback too.
    let found = t
        .ok("GET", "/v1/tasks?q=backup+zebra&status=doing", None, None)
        .await;
    assert_eq!(found, json!([]));
}

#[tokio::test]
async fn the_search_follows_every_write() {
    let t = setup().await;
    let task = t.task(A, "factory", "Plain").await;
    let find = async |q: &str| listed(&t.ok("GET", &format!("/v1/tasks?q={q}"), None, None).await);
    assert_eq!(find("walrus").await, Vec::<String>::new());

    // An edit of the text, and the text it replaced.
    let version = t.get_task(&task).await["version"].clone();
    t.ok(
        "PATCH",
        &format!("/v1/tasks/{task}"),
        Some(A),
        Some(
            json!({ "agent": A, "version": version, "title": "Feed the walrus",
                     "body": "with herring", "acceptance": "the bucket is empty" }),
        ),
    )
    .await;
    for q in ["walrus", "herring", "bucket"] {
        assert_eq!(find(q).await, vec![task.clone()], "{q}");
    }
    assert_eq!(find("plain").await, Vec::<String>::new());
    // Each note, and the summary of a done task.
    for text in ["first the otter", "then the narwhal"] {
        t.ok(
            "POST",
            &format!("/v1/tasks/{task}/notes"),
            Some(A),
            Some(json!({ "agent": A, "kind": "note", "text": text })),
        )
        .await;
    }
    assert_eq!(find("otter+narwhal").await, vec![task.clone()]);
    assert!(t.claim(A, &task).await.0.is_success());
    t.ok(
        "POST",
        &format!("/v1/tasks/{task}/done"),
        Some(A),
        Some(
            json!({ "agent": A, "version": t.get_task(&task).await["version"],
                     "summary": "the pelican is fed", "evidence": check(0) }),
        ),
    )
    .await;
    assert_eq!(find("pelican").await, vec![task.clone()]);
    // A board deleted with its tasks and notes takes their documents with it.
    t.ok(
        "PATCH",
        "/v1/boards/factory",
        None,
        Some(json!({ "status": "closed" })),
    )
    .await;
    let (s, v) = t.as_user("DELETE", "/v1/boards/factory", None).await;
    assert_eq!(s.as_u16(), 204, "{v}");
    assert_eq!(t.count("SELECT COUNT(*) FROM tasks"), 0);
    // One document for each task, equal to the tables.
    assert_eq!(
        t.count("SELECT COUNT(*) FROM task_search"),
        t.count("SELECT COUNT(*) FROM tasks")
    );
    assert_eq!(
        t.count("SELECT COUNT(*) FROM task_search_docs"),
        t.count("SELECT COUNT(*) FROM tasks")
    );
}

#[tokio::test]
async fn the_closed_filter_splits_done_and_dropped_from_the_rest() {
    let t = setup().await;
    let open = task_in(&t, TaskStatus::Open).await;
    let done = task_in(&t, TaskStatus::Done).await;
    let dropped = task_in(&t, TaskStatus::Dropped).await;
    let review = task_in(&t, TaskStatus::Review).await;
    let ids = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let all = t.ok("GET", "/v1/tasks?board=factory", None, None).await;
    assert_eq!(ids(&all).len(), 4);
    let live = t
        .ok("GET", "/v1/tasks?board=factory&closed=false", None, None)
        .await;
    assert_eq!(ids(&live), [open.clone(), review.clone()]);
    let closed = t
        .ok("GET", "/v1/tasks?board=factory&closed=true", None, None)
        .await;
    assert_eq!(ids(&closed), [done.clone(), dropped]);
    // It combines with the other filters.
    let none = t
        .ok(
            "GET",
            "/v1/tasks?board=factory&status=done&closed=false",
            None,
            None,
        )
        .await;
    assert!(ids(&none).is_empty());
    let (s, v) = t.call("GET", "/v1/tasks?closed=maybe", None, None).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid")),
        "{v}"
    );
}

/// The ids of a task list, in the order the server gave them.
fn listed(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn since_lists_the_tasks_changed_after_a_time_or_an_event() {
    let t = setup().await;
    let old = t.task(A, "factory", "Old").await;
    let noted = t.task(A, "factory", "Noted").await;
    let claimed = t.task(A, "factory", "Claimed").await;
    let seq = t.ok("GET", "/v1/events?since=0", None, None).await["next_since"]
        .as_i64()
        .unwrap();
    let before = super::iso(t.state.now());
    t.advance(TimeDelta::minutes(5));

    // Nothing changed after either point yet.
    for since in [seq.to_string(), before.clone()] {
        let found = t
            .ok("GET", &format!("/v1/tasks?since={since}"), None, None)
            .await;
        assert_eq!(listed(&found), Vec::<String>::new(), "since={since}");
    }

    // A note, a claim and a new task are changes; the untouched task stays out.
    t.ok(
        "POST",
        &format!("/v1/tasks/{noted}/notes"),
        Some(A),
        Some(json!({ "agent": A, "kind": "note", "text": "seen" })),
    )
    .await;
    assert!(t.claim(A, &claimed).await.0.is_success());
    let new = t.task(B, "factory", "New").await;
    for since in [seq.to_string(), before.clone()] {
        let found = t
            .ok("GET", &format!("/v1/tasks?since={since}"), None, None)
            .await;
        assert_eq!(
            listed(&found),
            vec![noted.clone(), claimed.clone(), new.clone()],
            "since={since}"
        );
        assert!(!listed(&found).contains(&old));
    }
    // A lease held for a task and the server's own notice about one are not changes.
    t.task_with(
        A,
        json!({
            "board": "factory", "title": "Later",
            "not_before": super::iso(t.state.now() + TimeDelta::minutes(1)),
        }),
    )
    .await;
    let quiet = t.count("SELECT MAX(seq) FROM events");
    t.advance(TimeDelta::minutes(2));
    t.ok(
        "POST",
        "/v1/leases",
        Some(A),
        Some(json!({ "agent": A, "repo": REPO, "globs": ["src/**"], "task": old })),
    )
    .await;
    assert_eq!(announce_not_before(&t), 1);
    assert!(t.count("SELECT MAX(seq) FROM events") >= quiet + 2);
    let found = t
        .ok("GET", &format!("/v1/tasks?since={quiet}"), None, None)
        .await;
    assert_eq!(listed(&found), Vec::<String>::new());
    // It combines with the other filters, and a time may carry an offset.
    let found = t
        .ok(
            "GET",
            &format!("/v1/tasks?since={seq}&status=doing"),
            None,
            None,
        )
        .await;
    assert_eq!(listed(&found), vec![claimed]);
    let found = t
        .ok(
            "GET",
            "/v1/tasks?since=2026-11-06T21:00:00%2B09:00",
            None,
            None,
        )
        .await;
    assert_eq!(listed(&found).len(), 4, "the four changed after 12:00Z");
    // A blank since is no filter; anything else must be a sequence number or a time.
    let found = t.ok("GET", "/v1/tasks?since=", None, None).await;
    assert_eq!(found.as_array().unwrap().len(), 5);
    for since in ["yesterday", "-1", "99999999999999999999", "2026-11-06"] {
        let (s, code, v) = t
            .fail("GET", &format!("/v1/tasks?since={since}"), None, None)
            .await;
        assert_eq!((s, code.as_str()), (400, "invalid"), "{since}: {v}");
    }
}

#[tokio::test]
async fn after_continues_a_cut_list_in_list_order() {
    let t = setup().await;
    t.board("other", &[]).await;
    let mut all = Vec::new();
    for (board, title, priority) in [
        ("factory", "Low one", "p3"),
        ("factory", "High one", "p1"),
        ("other", "Other board, same number", "p3"),
        ("factory", "Middle", "p2"),
        ("factory", "High two", "p1"),
        ("factory", "Low two", "p3"),
    ] {
        all.push(
            t.task_with(
                A,
                json!({ "board": board, "title": title, "priority": priority }),
            )
            .await,
        );
    }
    let whole = listed(&t.ok("GET", "/v1/tasks", None, None).await);
    assert_eq!(whole.len(), all.len());

    // Pages of two, each continued from the last task of the one before, give the whole
    // list once: priority, then id number, then board.
    let mut paged: Vec<String> = Vec::new();
    loop {
        let after = paged
            .last()
            .map(|id| format!("&after={id}"))
            .unwrap_or_default();
        let page = listed(
            &t.ok("GET", &format!("/v1/tasks?limit=2{after}"), None, None)
                .await,
        );
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 2);
        paged.extend(page);
    }
    assert_eq!(paged, whole);

    // The cursor need not pass the filters of the list it continues.
    let p3 = listed(
        &t.ok(
            "GET",
            &format!("/v1/tasks?board=factory&after={}", whole[1]),
            None,
            None,
        )
        .await,
    );
    assert_eq!(p3, vec![all[3].clone(), all[0].clone(), all[5].clone()]);
    // After the last task nothing follows.
    let last = whole.last().unwrap();
    let found = t
        .ok("GET", &format!("/v1/tasks?after={last}"), None, None)
        .await;
    assert_eq!(listed(&found), Vec::<String>::new());

    let (s, code, v) = t
        .fail("GET", "/v1/tasks?after=factory-999", None, None)
        .await;
    assert_eq!((s, code.as_str()), (404, "not_found"), "{v}");
    let (s, _, v) = t.fail("GET", "/v1/tasks?after=nope", None, None).await;
    assert_eq!(s, 400, "{v}");
}
