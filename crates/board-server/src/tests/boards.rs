use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{T, check, coordinator_ttl};

#[tokio::test]
async fn boards_are_created_updated_and_counted() {
    let t = T::new();
    let board = t
        .ok("POST", "/v1/boards", None, Some(json!({
            "slug": "factory", "title": "Factory", "repos": ["https://github.com/Example/App.git"],
        })))
        .await;
    assert_eq!(board["repos"], json!(["github.com/example/app"]));
    assert_eq!(board["status"], "active");
    let (s, _) = t
        .call(
            "POST",
            "/v1/boards",
            None,
            Some(json!({ "slug": "factory", "title": "Again" })),
        )
        .await;
    assert_eq!(s.as_u16(), 400);
    let updated = t
        .ok(
            "PATCH",
            "/v1/boards/factory",
            None,
            Some(json!({ "autopilot": true, "goal": "ship" })),
        )
        .await;
    assert_eq!(
        (updated["autopilot"].as_bool(), updated["goal"].as_str()),
        (Some(true), Some("ship"))
    );
    let (s, _) = t.call("GET", "/v1/boards/missing", None, None).await;
    assert_eq!(s.as_u16(), 404);

    t.register("t3:alpha", None, None).await;
    let joined = t
        .ok(
            "POST",
            "/v1/boards/factory/join",
            Some("t3:alpha"),
            Some(json!({ "agent": "t3:alpha" })),
        )
        .await;
    assert_eq!(joined["slug"], "factory");
    let task = t.task("t3:alpha", "factory", "Auto by default").await;
    assert_eq!(t.get_task(&task).await["auto"], true);
    t.ok(
        "POST",
        "/v1/asks",
        Some("t3:alpha"),
        Some(json!({
            "agent": "t3:alpha", "board": "factory", "kind": "decision", "question": "Ship?",
        })),
    )
    .await;
    let list = t.ok("GET", "/v1/boards", None, None).await;
    assert_eq!(list[0]["counts"]["open"], 1);
    assert_eq!(list[0]["open_asks"], 1);
}

#[tokio::test]
async fn one_device_holds_the_coordinator_lease() {
    let t = T::new();
    let laptop = t
        .as_user(
            "POST",
            "/v1/admin/devices",
            Some(json!({ "name": "laptop" })),
        )
        .await
        .1["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let as_laptop = |method: &'static str, body: Option<serde_json::Value>| {
        let bearer = format!("Bearer {laptop}");
        let t = &t;
        async move {
            t.send(
                method,
                "/v1/boards/auto/coordinator",
                &[("authorization", bearer.as_str())],
                body,
            )
            .await
        }
    };
    t.board("auto", &[]).await;
    let (s, v) = t
        .call(
            "POST",
            "/v1/boards/auto/coordinator",
            None,
            Some(json!({ "device": "mac" })),
        )
        .await;
    assert_eq!(s.as_u16(), 400, "not on autopilot: {v}");
    t.ok(
        "PATCH",
        "/v1/boards/auto",
        None,
        Some(json!({ "autopilot": true })),
    )
    .await;

    let (s, v) = t
        .call(
            "POST",
            "/v1/boards/auto/coordinator",
            None,
            Some(json!({ "device": "laptop" })),
        )
        .await;
    assert_eq!(s.as_u16(), 403, "a device claims as itself: {v}");
    let board = t
        .ok(
            "POST",
            "/v1/boards/auto/coordinator",
            None,
            Some(json!({ "device": "mac" })),
        )
        .await;
    assert_eq!(board["coordinator"], json!(null));
    let (s, v) = as_laptop("POST", Some(json!({ "device": "laptop" }))).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (409, Some("claim_refused"))
    );
    let (s, _) = as_laptop("DELETE", None).await;
    assert_eq!(s.as_u16(), 403);

    t.register("t3:coord", None, None).await;
    let board = t
        .ok(
            "POST",
            "/v1/boards/auto/coordinator",
            Some("t3:coord"),
            Some(json!({
                "device": "mac", "agent": "t3:coord",
            })),
        )
        .await;
    assert_eq!(board["coordinator"], "t3:coord");
    // Each step is inside the lease, two are past an unrenewed one.
    let step = TimeDelta::seconds(coordinator_ttl().num_seconds() * 2 / 3);
    // Renewal without an agent keeps the coordinator.
    t.advance(step);
    let board = t
        .ok(
            "POST",
            "/v1/boards/auto/coordinator",
            None,
            Some(json!({ "device": "mac" })),
        )
        .await;
    assert_eq!(board["coordinator"], "t3:coord");
    // The coordinator's heartbeat renews the lease too.
    t.advance(step);
    t.ok(
        "POST",
        "/v1/agents/t3:coord/state",
        Some("t3:coord"),
        Some(json!({ "state": "running" })),
    )
    .await;
    t.advance(coordinator_ttl() - TimeDelta::minutes(1));
    let (s, _) = as_laptop("POST", Some(json!({ "device": "laptop" }))).await;
    assert_eq!(s.as_u16(), 409, "renewed by the heartbeat");
    t.advance(TimeDelta::minutes(2));
    assert_eq!(
        t.ok("GET", "/v1/boards/auto", None, None).await["coordinator"],
        json!(null)
    );
    let (s, v) = as_laptop("POST", Some(json!({ "device": "laptop" }))).await;
    assert_eq!(s.as_u16(), 200, "expired lease: {v}");
    let (s, v) = as_laptop("DELETE", None).await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let page = t
        .ok("GET", "/v1/events?since=0&limit=1000", None, None)
        .await;
    let released = page["events"].as_array().unwrap().last().unwrap();
    assert_eq!(released["type"], "board_updated");
    assert_eq!(released["payload"]["coordinator"], json!(null));
    assert_eq!(
        released["payload"]["device"], "laptop",
        "names the releasing device"
    );
}

#[tokio::test]
async fn stats_count_the_feedback_signals() {
    let t = T::new();
    let a = "t3:alpha";
    t.register(a, None, None).await;
    t.board("factory", &[]).await;
    let id = t.task(a, "factory", "Measured").await;
    assert!(t.claim(a, &id).await.0.is_success());
    t.advance(TimeDelta::minutes(30));
    assert!(
        t.done(a, &id, json!([{ "kind": "commit", "value": "abc1234" }]))
            .await
            .0
            .is_success()
    );
    let other = t.task(a, "factory", "Checked").await;
    assert!(t.claim(a, &other).await.0.is_success());
    t.advance(TimeDelta::minutes(10));
    assert!(t.done(a, &other, check(0)).await.0.is_success());
    let ask = t
        .ok("POST", "/v1/asks", Some(a), Some(json!({
            "agent": a, "board": "factory", "kind": "action", "question": "Log in to the registry",
        })))
        .await;
    t.advance(TimeDelta::minutes(4));
    t.as_user(
        "POST",
        &format!("/v1/asks/{}/answer", ask["id"]),
        Some(json!({ "answer": "done" })),
    )
    .await;
    let stats = t.ok("GET", "/v1/stats?days=7", None, None).await;
    assert_eq!(stats["tasks_created"], 2);
    assert_eq!(stats["tasks_done"], 2);
    assert_eq!(stats["done_without_check_evidence"], 1);
    assert_eq!(stats["doing_minutes_median"], 20.0);
    assert_eq!(stats["asks_created"], 1);
    assert_eq!(stats["asks_answered"], 1);
    assert_eq!(stats["ask_answer_minutes_median"], 4.0);
    assert_eq!(stats["done_under_a_minute"], 0);
    // Work done off the board and claimed afterwards shows as a quick done.
    let quick = t.task(a, "factory", "Retro-claimed").await;
    assert!(t.claim(a, &quick).await.0.is_success());
    t.advance(TimeDelta::seconds(20));
    assert!(t.done(a, &quick, check(0)).await.0.is_success());
    let stats = t.ok("GET", "/v1/stats?days=7", None, None).await;
    assert_eq!(stats["done_under_a_minute"], 1);
    // Finishing a stale owner's work right after taking it over is not a quick done.
    let stale = t.task(a, "factory", "Left behind").await;
    assert!(t.claim(a, &stale).await.0.is_success());
    t.advance(super::stale_after() + TimeDelta::hours(1));
    let b = "t3:beta";
    t.register(b, None, None).await;
    t.ok(
        "POST",
        &format!("/v1/tasks/{stale}/take"),
        Some(b),
        Some(json!({ "agent": b })),
    )
    .await;
    t.advance(TimeDelta::seconds(20));
    assert!(t.done(b, &stale, check(0)).await.0.is_success());
    // Nor is a feedback item resolved right after its claim: it is routed, not worked.
    t.board("board-feedback", &[]).await;
    let item = t.task(b, "board-feedback", "Hook blocks git fetch").await;
    assert!(t.claim(b, &item).await.0.is_success());
    t.advance(TimeDelta::seconds(20));
    t.ok(
        "POST",
        &format!("/v1/feedback/{item}/resolve"),
        Some(b),
        Some(json!({ "kind": "declined", "reason": "works as intended" })),
    )
    .await;
    let stats = t.ok("GET", "/v1/stats?days=30", None, None).await;
    assert_eq!(stats["done_under_a_minute"], 1, "{stats}");
    let (s, _) = t.call("GET", "/v1/stats?days=0", None, None).await;
    assert_eq!(s.as_u16(), 400);
}

#[tokio::test]
async fn stats_give_each_board_its_wait_throughput_collision_and_limit_numbers() {
    const REPO: &str = "github.com/acme/app";
    let t = T::new();
    let at = |minutes: i64| super::iso(super::start() + TimeDelta::minutes(minutes));
    let (a, b, c) = ("t3:alpha", "t3:beta", "t3:gamma");
    for agent in [a, b, c] {
        t.register(agent, Some(REPO), None).await;
    }
    t.board("factory", &[REPO]).await;
    // Minute 0: two auto tasks, the second blocked by the first; b and c join the board.
    let first = t
        .task_with(
            a,
            json!({ "board": "factory", "title": "First", "auto": true }),
        )
        .await;
    let second = t
        .task_with(
            a,
            json!({ "board": "factory", "title": "Second", "auto": true, "blockers": [first] }),
        )
        .await;
    let theirs = t.task(b, "factory", "B's").await;
    t.task(c, "factory", "C's").await;

    // Minute 30: the first is claimed after waiting 30 minutes; b's lease overlaps a's and b
    // touches a file a leased.
    t.advance(TimeDelta::minutes(30));
    assert!(t.claim(a, &first).await.0.is_success());
    for (agent, glob, task) in [(a, "src/**", &first), (b, "src/lib.rs", &theirs)] {
        t.ok(
            "POST",
            "/v1/leases",
            Some(agent),
            Some(json!({ "agent": agent, "repo": REPO, "globs": [glob], "task": task })),
        )
        .await;
    }
    t.ok(
        "POST",
        "/v1/touches",
        Some(b),
        Some(json!({ "agent": b, "repo": REPO, "paths": ["src/lib.rs"] })),
    )
    .await;
    // Minutes 50-60 in review, then back to doing: reopened once, and those 10 minutes are
    // not time in doing. Done at minute 70 with a PR: 30 minutes in doing.
    t.advance(TimeDelta::minutes(20));
    assert!(
        t.patch(a, &first, json!({ "status": "review" }))
            .await
            .0
            .is_success()
    );
    t.advance(TimeDelta::minutes(10));
    assert!(
        t.patch(a, &first, json!({ "status": "doing" }))
            .await
            .0
            .is_success()
    );
    t.advance(TimeDelta::minutes(10));
    let evidence = json!([
        { "kind": "pr", "value": "https://github.com/acme/app/pull/1" },
        { "kind": "check", "value": "cargo test", "exit_code": 0 },
    ]);
    assert!(t.done(a, &first, evidence).await.0.is_success());
    // A task created claimed never waited.
    t.task_with(
        a,
        json!({ "board": "factory", "title": "Mine", "auto": true, "claim": true }),
    )
    .await;

    // Minute 90 b goes idle; minute 100 a pauses for a usage limit while a and c run, and
    // runs again at minute 130: half an hour paused.
    t.advance(TimeDelta::minutes(20));
    let state = |agent: &str, body: serde_json::Value| {
        let path = format!("/v1/agents/{agent}/state");
        let agent = agent.to_owned();
        let t = &t;
        async move { t.ok("POST", &path, Some(&agent), Some(body)).await }
    };
    state(b, json!({ "state": "idle" })).await;
    t.advance(TimeDelta::minutes(10));
    state(a, json!({ "state": "paused", "paused_until": at(160) })).await;
    t.advance(TimeDelta::minutes(30));
    t.register(a, Some(REPO), None).await;

    // Minute 160: the second, ready since its blocker was done at minute 70, is claimed.
    t.advance(TimeDelta::minutes(30));
    assert!(t.claim(b, &second).await.0.is_success());

    let stats = t.ok("GET", "/v1/stats?days=7", None, None).await;
    assert_eq!(
        stats["since"],
        at(0),
        "the oldest event, later than 7 days back"
    );
    let boards = stats["boards"].as_array().unwrap();
    assert_eq!(boards.len(), 1, "{stats}");
    let f = &boards[0];
    assert_eq!(f["board"], "factory");
    assert_eq!(f["claims"], 2);
    assert_eq!(f["wait_minutes_median"], 60.0);
    assert_eq!(f["wait_minutes_p90"], 90.0);
    assert_eq!(f["tasks_done"], 1);
    // 160 minutes covered: one task is 9 a day.
    assert_eq!(f["done_per_day"], 9.0);
    assert_eq!(f["doing_minutes_median"], 30.0);
    assert_eq!(f["lease_conflicts"], 2);
    assert_eq!(f["reopened_after_review"], 1);
    assert_eq!(f["pr_links"], 1);
    assert_eq!(f["limit_pauses"], 1);
    assert_eq!(f["limit_pauses_per_day"], 9.0);
    assert_eq!(f["paused_hours"], 0.5);
    assert_eq!(f["peak_running_at_pause"], 2);
}

/// One board's numbers from `GET /v1/stats?days=7`.
async fn board_stats(t: &T, slug: &str) -> Value {
    let stats = t.ok("GET", "/v1/stats?days=7", None, None).await;
    stats["boards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["board"] == slug)
        .unwrap_or_else(|| panic!("no board {slug} in {stats}"))
        .clone()
}

#[tokio::test]
async fn board_waits_count_from_when_the_task_last_became_ready() {
    const REPO: &str = "github.com/acme/app";
    let t = T::new();
    let at = |minutes: i64| super::iso(super::start() + TimeDelta::minutes(minutes));
    let (a, b, c) = ("t3:alpha", "t3:beta", "t3:gamma");
    for slug in ["cleared", "earlier", "created"] {
        t.board(slug, &[REPO]).await;
    }
    for agent in [a, b, c] {
        t.register(agent, Some(REPO), None).await;
    }
    // Minute 0: three auto tasks; the last may start at minute 40, set when it was created.
    let auto = |board: &str| json!({ "board": board, "title": "Work", "auto": true });
    let cleared = t.task_with(a, auto("cleared")).await;
    let earlier = t.task_with(a, auto("earlier")).await;
    let mut body = auto("created");
    body["not_before"] = json!(at(40));
    let created = t.task_with(a, body).await;
    // Minute 5: the first two may not start before minute 200; at minute 10 the first's
    // date is cleared and the second's moved to minute 20.
    t.advance(TimeDelta::minutes(5));
    for task in [&cleared, &earlier] {
        let (s, _) = t.patch(a, task, json!({ "not_before": at(200) })).await;
        assert!(s.is_success());
    }
    t.advance(TimeDelta::minutes(5));
    for (task, not_before) in [(&cleared, json!(null)), (&earlier, json!(at(20)))] {
        let (s, _) = t.patch(a, task, json!({ "not_before": not_before })).await;
        assert!(s.is_success());
    }
    // Minute 30: the first is claimed, ready for 20 minutes.
    t.advance(TimeDelta::minutes(20));
    assert!(t.claim(a, &cleared).await.0.is_success());
    // Minute 45: the second, ready since minute 20, and the third, ready since minute 40
    // though the sweeper has not announced that yet.
    t.advance(TimeDelta::minutes(15));
    assert!(t.claim(b, &earlier).await.0.is_success());
    assert!(t.claim(c, &created).await.0.is_success());

    for (slug, wait) in [("cleared", 20.0), ("earlier", 25.0), ("created", 5.0)] {
        let s = board_stats(&t, slug).await;
        assert_eq!(
            (s["claims"].clone(), s["wait_minutes_median"].clone()),
            (json!(1), json!(wait)),
            "{slug}: {s}"
        );
    }
}

#[tokio::test]
async fn board_lease_conflicts_count_every_lease_on_the_boards_repos() {
    const APP: &str = "github.com/acme/app";
    const SITE: &str = "github.com/acme/site";
    let t = T::new();
    let (a, b) = ("t3:alpha", "t3:beta");
    t.board("app", &[APP]).await;
    t.board("site", &[SITE]).await;
    for agent in [a, b] {
        t.register(agent, Some(APP), None).await;
    }
    t.task(a, "app", "App work").await;
    let site_task = t.task(b, "site", "Site work").await;
    // a leases without a task; b's overlapping lease names a task of the site board, but
    // the conflict is on the app board's repo.
    for (agent, glob, task) in [(a, "src/**", None), (b, "src/lib.rs", Some(&site_task))] {
        t.ok(
            "POST",
            "/v1/leases",
            Some(agent),
            Some(json!({ "agent": agent, "repo": APP, "globs": [glob], "task": task })),
        )
        .await;
    }
    assert_eq!(board_stats(&t, "app").await["lease_conflicts"], 1);
    assert_eq!(board_stats(&t, "site").await["lease_conflicts"], 0);
}

#[tokio::test]
async fn board_doing_time_counts_each_cycle_once() {
    let t = T::new();
    let a = "t3:alpha";
    t.board("factory", &[super::REPO]).await;
    t.register(a, Some(super::REPO), None).await;
    let task = t.task(a, "factory", "Twice").await;
    // Ten minutes in doing, done, reopened, then five more and done again.
    assert!(t.claim(a, &task).await.0.is_success());
    t.advance(TimeDelta::minutes(10));
    assert!(t.done(a, &task, check(0)).await.0.is_success());
    let (s, _) = t.patch(a, &task, json!({ "status": "open" })).await;
    assert!(s.is_success());
    t.advance(TimeDelta::minutes(10));
    assert!(t.claim(a, &task).await.0.is_success());
    t.advance(TimeDelta::minutes(5));
    assert!(t.done(a, &task, check(0)).await.0.is_success());

    let s = board_stats(&t, "factory").await;
    assert_eq!(s["tasks_done"], 2);
    assert_eq!(s["doing_minutes_median"], 7.5, "{s}");
}

#[tokio::test]
async fn board_pauses_follow_resets_sessions_membership_and_who_was_running() {
    const REPO: &str = "github.com/acme/app";
    let t = T::new();
    let (a, c, d, e, g, h) = (
        "t3:alpha", "t3:gamma", "t3:delta", "t3:echo", "t3:golf", "t3:hotel",
    );
    t.board("factory", &[REPO]).await;
    t.board("quiet", &["github.com/acme/quiet"]).await;
    for agent in [a, c, e, g] {
        t.register(agent, Some(REPO), None).await;
    }
    for agent in [d, h] {
        t.register(agent, None, None).await;
    }
    let state = |agent: &str, body: Value| {
        let path = format!("/v1/agents/{agent}/state");
        let agent = agent.to_owned();
        let t = &t;
        async move { t.ok("POST", &path, Some(&agent), Some(body)).await }
    };
    // Minute 0: d pauses before it joins factory, which it does at minute 1. h works on
    // quiet alone and never pauses.
    state(d, json!({ "state": "paused" })).await;
    t.advance(TimeDelta::minutes(1));
    t.task(d, "factory", "D's").await;
    t.task(h, "quiet", "Quiet").await;
    // g's registration has left the history: it has no state on record.
    t.exec("DELETE FROM events WHERE type = 'agent_registered' AND actor = 't3:golf'");
    // A day later e, silent since it registered, is stale; a, c and g are seen working.
    t.advance(super::stale_after());
    let paused_at = super::start() + TimeDelta::minutes(1) + super::stale_after();
    let until = |minutes: i64| super::iso(paused_at + TimeDelta::minutes(minutes));
    for agent in [a, c, g] {
        t.task(agent, "factory", "Seen").await;
    }
    // a pauses with only c also running, until +30; at +20 it learns the reset is +60, and
    // it runs again at +90: an hour paused.
    state(a, json!({ "state": "paused", "paused_until": until(30) })).await;
    t.advance(TimeDelta::minutes(20));
    state(a, json!({ "state": "paused", "paused_until": until(60) })).await;
    t.advance(TimeDelta::minutes(70));
    t.register(a, Some(REPO), None).await;
    // +100: c pauses with no reset named, with a running again; its session ends at +130,
    // which ends the pause: half an hour.
    t.advance(TimeDelta::minutes(10));
    state(c, json!({ "state": "paused" })).await;
    t.advance(TimeDelta::minutes(30));
    state(c, json!({ "state": "ended" })).await;
    t.advance(TimeDelta::minutes(70));

    let f = board_stats(&t, "factory").await;
    assert_eq!(f["limit_pauses"], 2, "{f}");
    assert_eq!(f["paused_hours"], 1.5, "{f}");
    assert_eq!(f["peak_running_at_pause"], 2, "{f}");
    let q = board_stats(&t, "quiet").await;
    assert_eq!(q["limit_pauses"], 0);
    assert_eq!(q["paused_hours"].to_string(), "0.0", "not -0.0");
    assert_eq!(q["peak_running_at_pause"], Value::Null);
}

#[tokio::test]
async fn web_view_falls_back_to_index() {
    let t = T::new();
    let (s, v) = t.send("GET", "/", &[], None).await;
    assert_eq!(s.as_u16(), 200);
    assert!(v.as_str().unwrap().contains("<html"), "{v}");
    let (s, v) = t.send("GET", "/boards/factory", &[], None).await;
    assert_eq!(s.as_u16(), 200);
    assert!(v.as_str().unwrap().contains("<html"));
    let (s, _) = t.send("GET", "/missing-file.js", &[], None).await;
    assert_eq!(s.as_u16(), 404);
    let (s, v) = t.send("POST", "/boards", &[], None).await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (404, Some("not_found")));
}

#[tokio::test]
async fn the_named_coordinator_keeps_its_role_when_it_registers_again() {
    let t = T::new();
    let lead = "t3:lead";
    let worker = "t3:worker";
    t.register(lead, None, None).await;
    t.register(worker, None, None).await;
    t.board("auto", &[]).await;
    t.ok(
        "PATCH",
        "/v1/boards/auto",
        None,
        Some(json!({ "autopilot": true })),
    )
    .await;
    t.ok(
        "POST",
        "/v1/boards/auto/coordinator",
        Some(lead),
        Some(json!({ "device": "mac", "agent": lead })),
    )
    .await;
    let role = |v: serde_json::Value| v["role"].as_str().unwrap().to_owned();
    assert_eq!(
        role(t.ok("GET", "/v1/agents/t3:lead", None, None).await),
        "coordinator"
    );
    // The session-start hook registers every agent as a worker.
    assert_eq!(role(t.register(lead, None, None).await), "coordinator");
    assert_eq!(role(t.register(worker, None, None).await), "worker");
}

#[tokio::test]
async fn html_carries_security_headers() {
    use tower::ServiceExt as _;
    let t = T::new();
    // `/index.html` is the file itself when `web/dist` is built, else a 404; both carry
    // the headers.
    for path in ["/", "/asks/3", "/index.html"] {
        let req = axum::http::Request::get(path)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = t.app.clone().oneshot(req).await.unwrap();
        let headers = response.headers();
        let csp = headers["content-security-policy"].to_str().unwrap();
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert!(csp.contains("script-src 'self'"), "{csp}");
        assert_eq!(headers["x-content-type-options"], "nosniff");
    }
}

#[tokio::test]
async fn mixed_case_board_repos_match_agents_from_any_url_form() {
    let t = T::new();
    let board = t.board("mixed", &["github.com/MyOrg/Repo"]).await;
    assert_eq!(board["repos"], json!(["github.com/myorg/repo"]));
    t.board("digits", &["git@github.com:12345/Repo.git"]).await;
    let agent = t
        .register("t3:alpha", Some("git@github.com:MyOrg/Repo.git"), None)
        .await;
    assert_eq!(agent["repo"], "github.com/myorg/repo");
    t.register("t3:beta", Some("https://github.com/12345/repo"), None)
        .await;
    assert_eq!(
        t.count(
            "SELECT count(*) FROM board_members WHERE \
             (board = 'mixed' AND agent = 't3:alpha') OR (board = 'digits' AND agent = 't3:beta')"
        ),
        2
    );
}

#[tokio::test]
async fn only_the_user_raises_parallel_tasks_and_anyone_lowers_it() {
    let t = T::new();
    let board = t.board("factory", &[]).await;
    assert_eq!(board["parallel_tasks"], 1, "one task at a time by default");

    // Out of range: the error says what to send.
    for n in [0, 5] {
        let (code, err, v) = t
            .fail(
                "PATCH",
                "/v1/boards/factory",
                None,
                Some(json!({ "parallel_tasks": n })),
            )
            .await;
        assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");
        assert!(
            v["next"].as_str().unwrap().contains("between 1 and 4"),
            "{v}"
        );
    }

    // An agent may not raise it: more tasks at once spend the shared usage limit.
    let (code, err, v) = t
        .fail(
            "PATCH",
            "/v1/boards/factory",
            None,
            Some(json!({ "parallel_tasks": 2 })),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // The user may; the event names the field.
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/factory",
            Some(json!({ "parallel_tasks": 3 })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["parallel_tasks"], 3);
    let page = t
        .ok("GET", "/v1/events?since=0&limit=1000", None, None)
        .await;
    let last = page["events"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        (&last["type"], &last["payload"]["fields"]),
        (&json!("board_updated"), &json!(["parallel_tasks"]))
    );

    // Restating or lowering it is open to anyone.
    for n in [3, 2] {
        let v = t
            .ok(
                "PATCH",
                "/v1/boards/factory",
                None,
                Some(json!({ "parallel_tasks": n })),
            )
            .await;
        assert_eq!(v["parallel_tasks"], n);
    }
}

#[tokio::test]
async fn resuming_any_board_resumes_the_feedback_board_first() {
    let t = T::new();
    let status = |s: &str| Some(json!({ "status": s }));
    let status_of = |board: Value| board["status"].as_str().unwrap().to_owned();
    for slug in ["board-feedback", "factory", "other"] {
        t.board(slug, &[]).await;
        let path = format!("/v1/boards/{slug}");
        t.ok("PATCH", &path, None, status("paused")).await;
    }

    let resumed = t
        .ok("PATCH", "/v1/boards/factory", None, status("active"))
        .await;
    assert_eq!(status_of(resumed), "active");
    let feedback = t.ok("GET", "/v1/boards/board-feedback", None, None).await;
    assert_eq!(status_of(feedback), "active");
    let other = t.ok("GET", "/v1/boards/other", None, None).await;
    assert_eq!(
        status_of(other),
        "paused",
        "only the feedback board comes along"
    );
    let page = t
        .ok("GET", "/v1/events?since=0&limit=1000", None, None)
        .await;
    let resumes: Vec<Value> = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["payload"]["status"] == "active")
        .map(|e| e["board"].clone())
        .collect();
    assert_eq!(resumes, [json!("board-feedback"), json!("factory")]);

    // Pausing the feedback board alone holds, and resuming it alone resumes nothing else.
    t.ok("PATCH", "/v1/boards/board-feedback", None, status("paused"))
        .await;
    let feedback = t.ok("GET", "/v1/boards/board-feedback", None, None).await;
    assert_eq!(status_of(feedback), "paused");
    t.ok("PATCH", "/v1/boards/board-feedback", None, status("active"))
        .await;
    let other = t.ok("GET", "/v1/boards/other", None, None).await;
    assert_eq!(status_of(other), "paused");

    // Changing an active board, or reopening a closed one, leaves the feedback board paused.
    t.ok("PATCH", "/v1/boards/board-feedback", None, status("paused"))
        .await;
    let goal = Some(json!({ "goal": "ship" }));
    t.ok("PATCH", "/v1/boards/factory", None, goal).await;
    t.ok("PATCH", "/v1/boards/other", None, status("closed"))
        .await;
    t.ok("PATCH", "/v1/boards/other", None, status("active"))
        .await;
    let feedback = t.ok("GET", "/v1/boards/board-feedback", None, None).await;
    assert_eq!(status_of(feedback), "paused");
}

#[tokio::test]
async fn resuming_a_board_works_without_a_feedback_board() {
    let t = T::new();
    t.board("factory", &[]).await;
    let status = |s: &str| Some(json!({ "status": s }));
    t.ok("PATCH", "/v1/boards/factory", None, status("paused"))
        .await;
    let resumed = t
        .ok("PATCH", "/v1/boards/factory", None, status("active"))
        .await;
    assert_eq!(resumed["status"], "active");
}
