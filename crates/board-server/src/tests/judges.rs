//! Daemon dispatch: the board's `dispatch` setting, the `dispatch:<device>` agent and the
//! judges its lease holder registers.

use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{T, check, coordinator_ttl};

const DISPATCH: &str = "dispatch:mac";
const WORKER: &str = "t3:worker";
const JUDGE: &str = "t3:judge";

/// `factory` on autopilot with `DISPATCH` holding its coordinator lease, and a second
/// board `other` coordinated by nobody. `WORKER` and `JUDGE` are registered.
async fn dispatched() -> T {
    let t = T::new();
    t.board("factory", &[]).await;
    t.board("other", &[]).await;
    t.ok(
        "PATCH",
        "/v1/boards/factory",
        None,
        Some(json!({ "autopilot": true, "dispatch": "daemon" })),
    )
    .await;
    t.register(DISPATCH, None, None).await;
    t.register(WORKER, None, None).await;
    t.register(JUDGE, None, None).await;
    claim_lease(&t).await;
    t
}

/// Takes or renews `factory`'s coordinator lease for `DISPATCH`.
async fn claim_lease(t: &T) {
    let board = t
        .ok(
            "POST",
            "/v1/boards/factory/coordinator",
            Some(DISPATCH),
            Some(json!({ "device": "mac", "agent": DISPATCH })),
        )
        .await;
    assert_eq!(board["coordinator"], DISPATCH);
}

/// A task on `board` that `WORKER` claimed and parked in review.
async fn parked(t: &T, board: &str) -> String {
    let id = t.task(WORKER, board, "Build it").await;
    let (s, v) = t.claim(WORKER, &id).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = t.patch(WORKER, &id, json!({ "status": "review" })).await;
    assert_eq!(s, 200, "{v}");
    id
}

async fn register_judge(t: &T, by: &str, body: Value) -> (u16, Value) {
    let (s, v) = t
        .call("POST", "/v1/boards/factory/judges", Some(by), Some(body))
        .await;
    (s.as_u16(), v)
}

async fn judges(t: &T) -> Vec<String> {
    t.ok("GET", "/v1/boards/factory/judges", None, None)
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["agent"].as_str().unwrap().to_owned())
        .collect()
}

/// Whether `agent` may hand `task` to `WORKER`, which it then hands back to keep the task
/// as it was.
async fn may_hand_over(t: &T, agent: &str, task: &str) -> bool {
    // Both owners must be live.
    t.register(DISPATCH, None, None).await;
    t.register(WORKER, None, None).await;
    let before = t.get_task(task).await["owner"].clone();
    let (s, v) = t.patch(agent, task, json!({ "owner": DISPATCH })).await;
    if s.as_u16() == 403 {
        assert_eq!(v["code"], "forbidden", "{v}");
        return false;
    }
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["owner"], DISPATCH);
    let (s, v) = t.patch(DISPATCH, task, json!({ "owner": before })).await;
    assert_eq!(s, 200, "{v}");
    true
}

#[tokio::test]
async fn the_dispatch_setting_is_thread_by_default_and_changes_with_an_event() {
    let t = T::new();
    let board = t.board("factory", &[]).await;
    assert_eq!(board["dispatch"], "thread");
    let since = t.count("SELECT MAX(seq) FROM events");
    let board = t
        .ok(
            "PATCH",
            "/v1/boards/factory",
            None,
            Some(json!({ "dispatch": "daemon" })),
        )
        .await;
    assert_eq!(board["dispatch"], "daemon");
    assert_eq!(
        t.ok("GET", "/v1/boards/factory", None, None).await["dispatch"],
        "daemon"
    );
    let events = t
        .ok("GET", &format!("/v1/events?since={since}"), None, None)
        .await;
    assert_eq!(
        events["events"][0]["payload"]["fields"],
        json!(["dispatch"])
    );
    let (code, _, v) = t
        .fail(
            "PATCH",
            "/v1/boards/factory",
            None,
            Some(json!({ "dispatch": "cron" })),
        )
        .await;
    assert_eq!(code, 400, "{v}");
}

async fn full_snapshot(t: &T, agent: &str) -> String {
    t.ok(
        "GET",
        &format!("/v1/snapshot?agent={agent}&kind=full"),
        Some(agent),
        None,
    )
    .await["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn a_full_snapshot_names_daemon_dispatch_only() {
    let t = T::new();
    t.board("factory", &[super::REPO]).await;
    t.register(WORKER, Some(super::REPO), None).await;
    assert!(!full_snapshot(&t, WORKER).await.contains("dispatch"));
    t.ok(
        "PATCH",
        "/v1/boards/factory",
        None,
        Some(json!({ "dispatch": "daemon" })),
    )
    .await;
    assert!(
        full_snapshot(&t, WORKER)
            .await
            .contains("\ndispatch: daemon\n")
    );
}

#[tokio::test]
async fn a_dispatch_agent_belongs_to_its_own_device() {
    let t = T::new();
    t.board("factory", &[]).await;
    let agent = t.register(DISPATCH, None, None).await;
    assert_eq!(agent["handle"], DISPATCH);

    // Another device's daemon: the mac token cannot speak for it.
    let (code, _, v) = t
        .fail(
            "POST",
            "/v1/agents",
            Some("dispatch:air"),
            Some(json!({ "id": "dispatch:air", "runtime": "claude", "device": "mac" })),
        )
        .await;
    assert_eq!(code, 403, "{v}");
    // Not even the user registers it for another device.
    let (s, v) = t
        .as_user(
            "POST",
            "/v1/agents",
            Some(json!({ "id": "dispatch:air", "runtime": "claude", "device": "mac" })),
        )
        .await;
    assert_eq!(s, 400, "{v}");
    // A device leases the coordinator only for its own dispatch agent.
    t.ok(
        "PATCH",
        "/v1/boards/factory",
        None,
        Some(json!({ "autopilot": true })),
    )
    .await;
    let bearer = format!("Bearer {}", t.user_token);
    let (s, v) = t
        .send(
            "POST",
            "/v1/boards/factory/coordinator",
            &[
                ("authorization", bearer.as_str()),
                ("x-board-agent", DISPATCH),
            ],
            Some(json!({ "device": "air", "agent": DISPATCH })),
        )
        .await;
    assert_eq!(s, 403, "{v}");
}

#[tokio::test]
async fn a_dispatch_agent_holding_the_lease_hands_over_and_finishes_a_task() {
    let t = dispatched().await;
    let id = parked(&t, "factory").await;
    let (s, v) = t.patch(DISPATCH, &id, json!({ "owner": DISPATCH })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["owner"].as_str(), v["status"].as_str()),
        (Some(DISPATCH), Some("review"))
    );
    let (s, v) = t.done(DISPATCH, &id, check(0)).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["status"], "done");

    // Without the lease it is an agent like any other.
    let other = parked(&t, "other").await;
    assert!(!may_hand_over(&t, DISPATCH, &other).await);
}

#[tokio::test]
async fn a_judge_has_the_coordinators_rights_on_its_board_only() {
    let t = dispatched().await;
    let id = parked(&t, "factory").await;
    let elsewhere = parked(&t, "other").await;
    assert!(!may_hand_over(&t, JUDGE, &id).await);

    let (s, v) = register_judge(
        &t,
        DISPATCH,
        json!({ "agent": JUDGE, "kind": "review", "tasks": [id, id] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["kind"], "review");
    assert_eq!(v["tasks"], json!([id]));
    assert_eq!(v["registered_by"], DISPATCH);
    assert_eq!(judges(&t).await, [JUDGE]);

    assert!(may_hand_over(&t, JUDGE, &id).await);
    assert!(!may_hand_over(&t, JUDGE, &elsewhere).await);
    // It may move the task too, as the coordinator may.
    let (s, v) = t
        .patch(
            JUDGE,
            &id,
            json!({ "status": "blocked", "blocked_reason": "waits on the user" }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
}

#[tokio::test]
async fn only_the_lease_holder_registers_judges_of_its_board_s_tasks() {
    let t = dispatched().await;
    let elsewhere = parked(&t, "other").await;
    let (s, v) = register_judge(&t, WORKER, json!({ "agent": JUDGE, "kind": "goal" })).await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    let (s, v) = register_judge(
        &t,
        DISPATCH,
        json!({ "agent": JUDGE, "kind": "review", "tasks": [elsewhere] }),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let (s, v) = register_judge(&t, DISPATCH, json!({ "agent": DISPATCH, "kind": "goal" })).await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let many: Vec<String> = (1..=21).map(|n| format!("factory-{n}")).collect();
    let (s, v) = register_judge(
        &t,
        DISPATCH,
        json!({ "agent": JUDGE, "kind": "verdict", "tasks": many }),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    // A judge registers no other judge.
    let (s, _) = register_judge(&t, DISPATCH, json!({ "agent": JUDGE, "kind": "goal" })).await;
    assert_eq!(s, 200);
    let (s, v) = register_judge(&t, JUDGE, json!({ "agent": WORKER, "kind": "goal" })).await;
    assert_eq!((s, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    assert!(judges(&t).await.iter().all(|j| j == JUDGE));
}

#[tokio::test]
async fn a_judge_loses_the_rights_when_its_agent_ends_even_if_it_resumes() {
    let t = dispatched().await;
    let id = parked(&t, "factory").await;
    register_judge(
        &t,
        DISPATCH,
        json!({ "agent": JUDGE, "kind": "review", "tasks": [id] }),
    )
    .await;
    assert!(may_hand_over(&t, JUDGE, &id).await);
    t.ok(
        "POST",
        &format!("/v1/agents/{JUDGE}/state"),
        Some(JUDGE),
        Some(json!({ "state": "ended", "paused_until": null })),
    )
    .await;
    assert!(judges(&t).await.is_empty());
    // Its calls mark it running again, but the registration stays over.
    assert!(!may_hand_over(&t, JUDGE, &id).await);
    // An ended agent cannot be registered; once running again, it may be.
    t.ok(
        "POST",
        &format!("/v1/agents/{JUDGE}/state"),
        Some(JUDGE),
        Some(json!({ "state": "ended", "paused_until": null })),
    )
    .await;
    let (s, v) = register_judge(
        &t,
        DISPATCH,
        json!({ "agent": JUDGE, "kind": "review", "tasks": [id] }),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
    t.register(JUDGE, None, None).await;
    let (s, v) = register_judge(
        &t,
        DISPATCH,
        json!({ "agent": JUDGE, "kind": "review", "tasks": [id] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert!(may_hand_over(&t, JUDGE, &id).await);
}

#[tokio::test]
async fn a_judge_loses_the_rights_after_two_hours_and_registering_again_does_not_extend_them() {
    let t = dispatched().await;
    let id = parked(&t, "factory").await;
    let (_, first) = register_judge(
        &t,
        DISPATCH,
        json!({ "agent": JUDGE, "kind": "review", "tasks": [id] }),
    )
    .await;
    let step = coordinator_ttl() / 2;
    let mut passed = TimeDelta::zero();
    while passed + step < TimeDelta::hours(2) {
        t.advance(step);
        passed += step;
        claim_lease(&t).await;
    }
    let (s, again) = register_judge(
        &t,
        DISPATCH,
        json!({ "agent": JUDGE, "kind": "verdict", "tasks": [id] }),
    )
    .await;
    assert_eq!(s, 200, "{again}");
    assert_eq!(again["kind"], "verdict");
    assert_eq!(again["expires_at"], first["expires_at"]);
    t.advance(TimeDelta::hours(2) - passed - TimeDelta::seconds(1));
    assert!(may_hand_over(&t, JUDGE, &id).await);
    t.advance(TimeDelta::seconds(1));
    assert!(judges(&t).await.is_empty());
    assert!(!may_hand_over(&t, JUDGE, &id).await);
}

#[tokio::test]
async fn a_judge_loses_the_rights_when_its_registrant_loses_the_lease() {
    let t = dispatched().await;
    let id = parked(&t, "factory").await;
    register_judge(
        &t,
        DISPATCH,
        json!({ "agent": JUDGE, "kind": "review", "tasks": [id] }),
    )
    .await;
    let (s, _) = t
        .as_user("DELETE", "/v1/boards/factory/coordinator", None)
        .await;
    assert_eq!(s, 200);
    assert!(judges(&t).await.is_empty());
    assert!(!may_hand_over(&t, JUDGE, &id).await);
    // The same holder taking the lease again gives the live registration back.
    claim_lease(&t).await;
    assert!(may_hand_over(&t, JUDGE, &id).await);
}

#[tokio::test]
async fn a_judges_turn_endings_are_kept_as_a_coordinators() {
    let t = dispatched().await;
    let stop = |said: &'static str| {
        let t = &t;
        async move {
            t.ok(
                "POST",
                &format!("/v1/agents/{JUDGE}/state"),
                Some(JUDGE),
                Some(json!({ "state": "idle", "paused_until": null, "said": said })),
            )
            .await;
        }
    };
    stop("Before registration.").await;
    register_judge(&t, DISPATCH, json!({ "agent": JUDGE, "kind": "goal" })).await;
    stop("review: pass").await;
    let (s, said) = t
        .as_user("GET", &format!("/v1/agents/{JUDGE}/said"), None)
        .await;
    assert_eq!(s, 200);
    let said = said.as_array().unwrap();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(
        (said[0]["board"].as_str(), said[0]["text"].as_str()),
        (Some("factory"), Some("review: pass"))
    );
}

#[tokio::test]
async fn a_judge_named_by_its_handle_is_registered_by_its_id() {
    let t = dispatched().await;
    let id = parked(&t, "factory").await;
    let handle = t.register(JUDGE, None, None).await["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    let (s, v) = register_judge(
        &t,
        DISPATCH,
        json!({ "agent": handle, "kind": "review", "tasks": [id] }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["agent"], JUDGE);
    assert!(may_hand_over(&t, JUDGE, &id).await);
    // The lease holder's own handle is still itself.
    let (s, v) = register_judge(&t, DISPATCH, json!({ "agent": DISPATCH, "kind": "goal" })).await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
}

#[tokio::test]
async fn repeats_are_dropped_before_the_task_limit_and_a_review_names_its_task() {
    let t = dispatched().await;
    let mut tasks = Vec::new();
    for n in 0..20 {
        tasks.push(t.task(WORKER, "factory", &format!("Task {n}")).await);
    }
    let mut repeated = tasks.clone();
    repeated.push(tasks[0].clone());
    let (s, v) = register_judge(
        &t,
        DISPATCH,
        json!({ "agent": JUDGE, "kind": "verdict", "tasks": repeated }),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["tasks"].as_array().unwrap().len(), 20);
    let (s, v) = register_judge(&t, DISPATCH, json!({ "agent": JUDGE, "kind": "review" })).await;
    assert_eq!((s, v["code"].as_str()), (400, Some("invalid")), "{v}");
}

#[tokio::test]
async fn the_list_says_which_judges_still_run_and_drops_ended_ones() {
    let t = dispatched().await;
    let id = parked(&t, "factory").await;
    register_judge(
        &t,
        DISPATCH,
        json!({ "agent": JUDGE, "kind": "review", "tasks": [id] }),
    )
    .await;
    // Not registered yet: its session is starting.
    register_judge(
        &t,
        DISPATCH,
        json!({ "agent": "t3:starting", "kind": "goal" }),
    )
    .await;
    let list = t.ok("GET", "/v1/boards/factory/judges", None, None).await;
    assert_eq!(list[0]["running"], true, "{list}");
    assert_eq!(list[1]["running"], true, "{list}");
    // A turn that ended leaves the rights but not the count.
    t.ok(
        "POST",
        &format!("/v1/agents/{JUDGE}/state"),
        Some(JUDGE),
        Some(json!({ "state": "idle", "paused_until": null })),
    )
    .await;
    let list = t.ok("GET", "/v1/boards/factory/judges", None, None).await;
    assert_eq!(list[0]["running"], false, "{list}");
    assert!(may_hand_over(&t, JUDGE, &id).await);
    // An ended registration is dropped at the next registration.
    t.ok(
        "POST",
        &format!("/v1/agents/{JUDGE}/state"),
        Some(JUDGE),
        Some(json!({ "state": "ended", "paused_until": null })),
    )
    .await;
    register_judge(&t, DISPATCH, json!({ "agent": "t3:next", "kind": "goal" })).await;
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM judges WHERE agent = '{JUDGE}'"
        )),
        0
    );
}
