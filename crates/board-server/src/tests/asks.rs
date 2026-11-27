use chrono::TimeDelta;
use serde_json::json;

use super::{T, USER, defaults};

const A: &str = "t3:alpha";

async fn ask(t: &T, question: &str) -> i64 {
    t.register(A, None, None).await;
    if t.call("GET", "/v1/boards/factory", None, None)
        .await
        .0
        .as_u16()
        == 404
    {
        t.board("factory", &[]).await;
    }
    let ask = t
        .ok(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "kind": "decision", "question": question,
                "options": ["yes", "no"],
            })),
        )
        .await;
    assert_eq!(ask["status"], "open");
    ask["id"].as_i64().unwrap()
}

#[tokio::test]
async fn only_the_user_answers_and_the_channel_is_recorded() {
    let t = T::new();
    let id = ask(&t, "Ship it?").await;
    let path = format!("/v1/asks/{id}/answer");
    let (s, v) = t
        .call("POST", &path, Some(A), Some(json!({ "answer": "yes" })))
        .await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (403, Some("forbidden")));
    let (s, v) = t
        .as_user("POST", &path, Some(json!({ "answer": "yes" })))
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert_eq!(
        (v["answer"].as_str(), v["answered_via"].as_str()),
        (Some("yes"), Some("cli"))
    );
    let (s, v) = t
        .as_user("POST", &path, Some(json!({ "answer": "no" })))
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition"))
    );

    let id = ask(&t, "Which port?").await;
    let (s, v) = t
        .send(
            "POST",
            &format!("/v1/asks/{id}/answer"),
            &[("tailscale-user-login", USER)],
            Some(json!({ "answer": "7377" })),
        )
        .await;
    assert_eq!((s.as_u16(), v["answered_via"].as_str()), (200, Some("web")));
    let events = t.ok("GET", "/v1/events?since=0", None, None).await;
    let answered: Vec<_> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "ask_answered")
        .collect();
    assert_eq!(answered.len(), 2);
    assert_eq!(answered[1]["actor"], "user");
    assert_eq!(answered[1]["payload"]["via"], "web");
    assert_eq!(answered[1]["payload"]["agent"], A);
}

#[tokio::test]
async fn only_the_asker_withdraws_an_open_ask() {
    let t = T::new();
    let id = ask(&t, "Rename the crate?").await;
    t.register("t3:beta", None, None).await;
    let path = format!("/v1/asks/{id}/withdraw");
    let (s, _) = t
        .call(
            "POST",
            &path,
            Some("t3:beta"),
            Some(json!({ "agent": "t3:beta" })),
        )
        .await;
    assert_eq!(s.as_u16(), 403);
    let withdrawn = t
        .ok("POST", &path, Some(A), Some(json!({ "agent": A })))
        .await;
    assert_eq!(withdrawn["status"], "withdrawn");
    let (_, code, _) = t
        .fail("POST", &path, Some(A), Some(json!({ "agent": A })))
        .await;
    assert_eq!(code, "invalid_transition");
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{id}/answer"),
            Some(json!({ "answer": "x" })),
        )
        .await;
    assert_eq!(s.as_u16(), 400);
    assert_eq!(
        t.ok("GET", "/v1/asks?status=open", None, None).await,
        json!([])
    );
    assert_eq!(
        t.ok("GET", "/v1/asks?status=withdrawn&board=factory", None, None)
            .await[0]["id"],
        id
    );
}

#[tokio::test]
async fn a_limit_lists_the_newest_asks_oldest_first() {
    let t = T::new();
    let ids = [
        ask(&t, "first").await,
        ask(&t, "second").await,
        ask(&t, "third").await,
    ];
    t.board("other", &[]).await;
    let listed = |path: &'static str| async {
        let asks = t.ok("GET", path, None, None).await;
        let asks: Vec<i64> = asks
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_i64().unwrap())
            .collect();
        asks
    };
    // Without a limit every ask is listed, as before.
    assert_eq!(listed("/v1/asks").await, ids);
    assert_eq!(listed("/v1/asks?limit=2").await, ids[1..]);
    assert_eq!(listed("/v1/asks?limit=3").await, ids);
    assert_eq!(listed("/v1/asks?limit=9").await, ids);
    // A limit of 0 reads as 1.
    assert_eq!(listed("/v1/asks?limit=0").await, ids[2..]);
    // The limit counts the asks that pass the filters.
    let path = format!("/v1/asks/{}/withdraw", ids[2]);
    t.ok("POST", &path, Some(A), Some(json!({ "agent": A })))
        .await;
    assert_eq!(listed("/v1/asks?status=open&limit=1").await, ids[1..2]);
    assert_eq!(
        listed("/v1/asks?status=open&board=factory&limit=5").await,
        ids[..2]
    );
    assert_eq!(listed("/v1/asks?board=other&limit=5").await, [0i64; 0]);
    let (status, code, _) = t.fail("GET", "/v1/asks?limit=x", None, None).await;
    assert_eq!((status, code.as_str()), (400, "invalid"));
}

#[tokio::test]
async fn ask_fields_are_validated() {
    let t = T::new();
    ask(&t, "first").await;
    let body = |question: &str, options: Vec<String>| json!({ "agent": A, "board": "factory", "kind": "action", "question": question, "options": options });
    for bad in [
        body("  ", vec![]),
        body(&"q".repeat(defaults().question_chars + 1), vec![]),
        body("ok?", (0..9).map(|i| i.to_string()).collect()),
        body("ok?", vec!["x".repeat(61)]),
        body("ok?", vec!["two\nlines".to_owned()]),
    ] {
        let (status, code, _) = t.fail("POST", "/v1/asks", Some(A), Some(bad)).await;
        assert_eq!((status, code.as_str()), (400, "invalid"));
    }
    // A question takes 1,500 characters (ask #98).
    assert_eq!(defaults().question_chars, 1500);
    t.ok(
        "POST",
        "/v1/asks",
        Some(A),
        Some(body(&"q".repeat(1500), vec![])),
    )
    .await;
    let (status, _, _) = t
        .fail(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "nope", "kind": "action", "question": "x",
            })),
        )
        .await;
    assert_eq!(status, 404);
    let (s, _) = t
        .as_user(
            "POST",
            "/v1/asks/999/answer",
            Some(json!({ "answer": "x" })),
        )
        .await;
    assert_eq!(s.as_u16(), 404);
}

#[tokio::test]
async fn one_ask_is_read_by_id_in_any_status() {
    let t = T::new();
    let id = ask(&t, "Ship it?").await;
    let path = format!("/v1/asks/{id}");
    let v = t.ok("GET", &path, None, None).await;
    assert_eq!(
        (v["id"].as_i64(), v["status"].as_str()),
        (Some(id), Some("open"))
    );
    t.as_user(
        "POST",
        &format!("{path}/answer"),
        Some(json!({ "answer": "yes" })),
    )
    .await;
    let v = t.ok("GET", &path, None, None).await;
    assert_eq!(
        (v["status"].as_str(), v["answer"].as_str()),
        (Some("answered"), Some("yes"))
    );
    let (status, code, _) = t.fail("GET", "/v1/asks/999", None, None).await;
    assert_eq!((status, code.as_str()), (404, "not_found"));
}

#[tokio::test]
async fn an_ask_warns_of_the_boards_other_open_asks_under_30_minutes_old() {
    const B: &str = "t3:beta";
    let t = T::new();
    t.register(A, None, None).await;
    t.register(B, None, None).await;
    t.board("factory", &[]).await;
    t.board("docs", &[]).await;
    let task = t.task(A, "factory", "Pick a storage engine").await;
    let ask = |agent: &'static str, board: &'static str, task: Option<String>| {
        let t = &t;
        async move {
            t.ok(
                "POST",
                "/v1/asks",
                Some(agent),
                Some(json!({
                    "agent": agent, "board": board, "task": task, "kind": "decision",
                    "question": "Which one?",
                })),
            )
            .await
        }
    };

    // Alone on its board: no warning, and the answer is the plain ask.
    let first = ask(A, "factory", Some(task.clone())).await;
    assert!(first.get("warning").is_none(), "{first}");
    let first_id = first["id"].as_i64().unwrap();

    // Another board's ask is not a duplicate.
    t.advance(TimeDelta::minutes(10));
    let docs = ask(B, "docs", None).await;
    assert!(docs.get("warning").is_none(), "{docs}");

    // Ten minutes later on the same board: made, with a warning naming the first ask.
    let second = ask(B, "factory", None).await;
    assert_eq!(second["status"], "open");
    let second_id = second["id"].as_i64().unwrap();
    let warning = second["warning"].as_str().unwrap();
    assert!(
        warning.starts_with(&format!(
            "board factory has 1 other open ask under 30 minutes old: #{first_id} on {task} by {A}, 10m ago."
        )),
        "{warning}"
    );
    assert!(
        warning.ends_with(&format!(
            "If ask #{second_id} repeats one of them, keep one and withdraw the other \
             (ask_withdraw; CLI: board withdraw <id>)."
        )),
        "{warning}"
    );
    assert_eq!(
        t.count("SELECT COUNT(*) FROM asks WHERE status = 'open'"),
        3
    );

    // The first ask is now 35 minutes old: only the second is named, by its asker.
    t.advance(TimeDelta::minutes(25));
    let third = ask(A, "factory", None).await;
    let warning = third["warning"].as_str().unwrap();
    assert!(
        warning.contains(&format!("#{second_id} on no task by {B}, 25m ago")),
        "{warning}"
    );
    assert!(!warning.contains(&format!("#{first_id} ")), "{warning}");

    // Answered and withdrawn asks are not named; the asker's own ask is "you".
    t.as_user(
        "POST",
        &format!("/v1/asks/{second_id}/answer"),
        Some(json!({ "answer": "SQLite" })),
    )
    .await;
    t.advance(TimeDelta::minutes(1));
    let fourth = ask(A, "factory", None).await;
    let warning = fourth["warning"].as_str().unwrap();
    let third_id = third["id"].as_i64().unwrap();
    assert!(
        warning.contains(&format!("#{third_id} on no task by you, 1m ago")),
        "{warning}"
    );
    assert!(!warning.contains(&format!("#{second_id} ")), "{warning}");

    // An ask exactly 30 minutes old is no longer named.
    t.advance(TimeDelta::minutes(29));
    let sixth = ask(A, "factory", None).await;
    let warning = sixth["warning"].as_str().unwrap();
    assert!(
        warning.starts_with("board factory has 1 other open ask under 30 minutes old: #"),
        "{warning}"
    );
    assert!(!warning.contains(&format!("#{third_id} ")), "{warning}");

    // Once the others are 30 minutes old, nothing is named.
    t.advance(TimeDelta::minutes(31));
    let fifth = ask(A, "factory", None).await;
    assert!(fifth.get("warning").is_none(), "{fifth}");
}

/// The newest `board_updated` event.
async fn last_board_update(t: &T) -> serde_json::Value {
    let page = t
        .ok("GET", "/v1/events?since=0&limit=1000", None, None)
        .await;
    page["events"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["type"] == "board_updated")
        .cloned()
        .unwrap_or_default()
}

async fn ask_with_change(
    t: &T,
    change: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    t.register(A, None, None).await;
    t.call(
        "POST",
        "/v1/asks",
        Some(A),
        Some(json!({
            "agent": A, "board": "factory", "kind": "decision",
            "question": "Run two tasks at once?", "options": ["Keep 1"], "change": change,
        })),
    )
    .await
}

#[tokio::test]
async fn the_approving_option_of_an_ask_applies_its_board_change() {
    let t = T::new();
    t.board("factory", &[]).await;
    let (s, ask) = ask_with_change(&t, json!({ "parallel_tasks": 2, "usage_stop_at": 95 })).await;
    assert_eq!(s.as_u16(), 200, "{ask}");
    // The server writes the approving option, first, so the user reads what it applies.
    assert_eq!(
        ask["options"],
        json!(["Apply: parallel_tasks=2, usage_stop_at=95", "Keep 1"])
    );
    assert_eq!(
        ask["change"],
        json!({ "parallel_tasks": 2, "usage_stop_at": 95 })
    );
    let id = ask["id"].as_i64().unwrap();
    // Asking changes nothing.
    assert_eq!(
        t.ok("GET", "/v1/boards/factory", None, None).await["parallel_tasks"],
        1
    );

    let (s, v) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{id}/answer"),
            Some(json!({ "answer": "Apply: parallel_tasks=2, usage_stop_at=95" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    assert_eq!(
        (
            board["parallel_tasks"].as_u64(),
            board["usage_stop_at"].as_u64()
        ),
        (Some(2), Some(95))
    );
    // The event names the user, the ask, its asker and the values.
    let event = last_board_update(&t).await;
    assert_eq!(event["actor"], "user", "{event}");
    assert_eq!(
        event["payload"],
        json!({
            "fields": ["parallel_tasks", "usage_stop_at"], "parallel_tasks": 2,
            "usage_stop_at": 95, "approved": "ask", "ask": id, "agent": A,
        })
    );
}

#[tokio::test]
async fn another_answer_to_an_ask_with_a_change_applies_nothing() {
    let t = T::new();
    t.board("factory", &[]).await;
    for answer in ["Keep 1", "yes", "Apply: parallel_tasks=3"] {
        let (_, ask) = ask_with_change(&t, json!({ "parallel_tasks": 2 })).await;
        let id = ask["id"].as_i64().unwrap();
        let (s, v) = t
            .as_user(
                "POST",
                &format!("/v1/asks/{id}/answer"),
                Some(json!({ "answer": answer })),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "{v}");
        assert_eq!(
            t.ok("GET", "/v1/boards/factory", None, None).await["parallel_tasks"],
            1,
            "{answer}"
        );
    }
    // An ask without a change whose option copies the approving text applies nothing.
    let id = ask(&t, "Ship it?").await;
    t.ok(
        "POST",
        &format!("/v1/asks/{id}/withdraw"),
        Some(A),
        Some(json!({ "agent": A })),
    )
    .await;
    let plain = t
        .ok(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "kind": "decision", "question": "Two?",
                "options": ["Apply: parallel_tasks=2"],
            })),
        )
        .await;
    let (s, _) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{}/answer", plain["id"]),
            Some(json!({ "answer": "Apply: parallel_tasks=2" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    assert_eq!(
        t.ok("GET", "/v1/boards/factory", None, None).await["parallel_tasks"],
        1
    );
}

#[tokio::test]
async fn an_ask_carries_only_the_user_only_board_settings() {
    let t = T::new();
    t.board("factory", &[]).await;
    for (change, code) in [
        (json!({}), "invalid"),
        (json!({ "parallel_tasks": 9 }), "invalid"),
        (json!({ "usage_stop_at": 0 }), "invalid"),
        // No other board setting, and nothing else, passes through an approval.
        (
            json!({ "parallel_tasks": 2, "status": "closed" }),
            "invalid",
        ),
        (json!({ "autopilot": true }), "invalid"),
    ] {
        let (s, v) = ask_with_change(&t, change.clone()).await;
        assert_eq!(
            (s.as_u16(), v["code"].as_str()),
            (400, Some(code)),
            "{change} {v}"
        );
    }
    // The longest approving option fits an option.
    let (s, v) = ask_with_change(
        &t,
        json!({ "parallel_tasks": 4, "usage_stop_at": 100, "temporary": false }),
    )
    .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert!(
        v["options"][0].as_str().unwrap().chars().count() <= board_proto::ASK_OPTION_CHARS_MAX,
        "{v}"
    );
}

#[tokio::test]
async fn a_late_approving_answer_never_lowers_a_setting_and_typed_text_counts() {
    let t = T::new();
    t.board("factory", &[]).await;
    let (s, ask) = ask_with_change(&t, json!({ "parallel_tasks": 2, "usage_stop_at": 95 })).await;
    assert_eq!(s.as_u16(), 200, "{ask}");
    let id = ask["id"].as_i64().unwrap();
    // The user raises the board past the ask before answering it.
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/factory",
            Some(json!({ "parallel_tasks": 4 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    // The option typed as text, with spaces around it, is the option.
    let (s, v) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{id}/answer"),
            Some(json!({ "answer": " Apply: parallel_tasks=2, usage_stop_at=95 " })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    assert_eq!(
        (
            board["parallel_tasks"].as_u64(),
            board["usage_stop_at"].as_u64()
        ),
        (Some(4), Some(95))
    );
    let payload = last_board_update(&t).await["payload"].clone();
    assert_eq!(payload["fields"], json!(["usage_stop_at"]), "{payload}");
    assert_eq!(payload["not_lowered"], json!(["parallel_tasks"]));
    assert!(payload.get("parallel_tasks").is_none(), "{payload}");
}

#[tokio::test]
async fn an_agents_own_option_may_not_read_as_the_approving_one() {
    let t = T::new();
    t.board("factory", &[]).await;
    t.register(A, None, None).await;
    for option in ["Apply: parallel_tasks=2", " apply: parallel_tasks=4"] {
        let (code, err, v) = t
            .fail(
                "POST",
                "/v1/asks",
                Some(A),
                Some(json!({
                    "agent": A, "board": "factory", "kind": "decision",
                    "question": "Run two tasks at once?", "options": [option],
                    "change": { "parallel_tasks": 2 },
                })),
            )
            .await;
        assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");
    }
}

#[tokio::test]
async fn timed_board_change_on_ask_validates_until_and_skips_if_answered_late() {
    let t = T::new();
    let at = |minutes: i64| super::iso(super::start() + TimeDelta::minutes(minutes));
    t.board("factory", &[]).await;

    // Non-future until is refused at ask creation.
    let (s, v) = ask_with_change(&t, json!({ "parallel_tasks": 3, "until": at(0) })).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid")),
        "{v}"
    );

    // Answering before `until` applies the timed raise with its approving option.
    let (s, ask) = ask_with_change(&t, json!({ "parallel_tasks": 3, "until": at(60) })).await;
    assert_eq!(s.as_u16(), 200, "{ask}");
    let opt = ask["options"][0].as_str().unwrap().to_owned();
    assert!(opt.starts_with("Apply: parallel_tasks=3 until "), "{opt}");
    let id = ask["id"].as_i64().unwrap();
    let (s, v) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{id}/answer"),
            Some(json!({ "answer": opt })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    assert_eq!(board["parallel_tasks"], 3);
    assert_eq!(board["parallel_tasks_prev"], 1);
    assert_eq!(board["parallel_tasks_until"], at(60));

    // Expire it back to 1.
    t.advance(TimeDelta::minutes(60));
    t.state
        .write(crate::ops::boards::sweep_expired_limits)
        .await
        .unwrap();
    assert_eq!(
        t.ok("GET", "/v1/boards/factory", None, None).await["parallel_tasks"],
        1
    );

    // An ask answered after `until` has passed does not apply the expired limit, sets
    // `expired_before_answer: true`, omits `until` from the event, and still applies `temporary`.
    let (s, late_ask) = ask_with_change(
        &t,
        json!({ "parallel_tasks": 3, "temporary": true, "until": at(90) }),
    )
    .await;
    assert_eq!(s.as_u16(), 200, "{late_ask}");
    let late_opt = late_ask["options"][0].as_str().unwrap().to_owned();
    let late_id = late_ask["id"].as_i64().unwrap();
    t.advance(TimeDelta::minutes(40)); // now at +100m > +90m
    let (s, v) = t
        .as_user(
            "POST",
            &format!("/v1/asks/{late_id}/answer"),
            Some(json!({ "answer": late_opt })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    assert_eq!(board["parallel_tasks"], 1);
    assert!(board.get("parallel_tasks_until").is_none());
    assert_eq!(board["temporary"], true);
    let payload = last_board_update(&t).await["payload"].clone();
    assert_eq!(payload["expired_before_answer"], true, "{payload}");
    assert!(payload.get("until").is_none(), "{payload}");
    assert_eq!(payload["fields"], json!(["temporary"]), "{payload}");
}

#[tokio::test]
async fn approved_ask_answer_records_agent_and_words_and_applies_change() {
    const B: &str = "t3:beta";
    let t = T::new();
    let id = ask(&t, "Unblock deploy?").await;
    t.register(B, None, None).await;
    let path = format!("/v1/asks/{id}/approved");
    let user_bearer = format!("Bearer {}", t.user_token);

    // Regular device token is refused with 403 and a hint naming `board approved answer`.
    let (s, v) = t
        .call(
            "POST",
            &path,
            Some(B),
            Some(json!({ "said": "go ahead", "answer": "yes" })),
        )
        .await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (403, Some("forbidden")));
    assert!(
        v["next"]
            .as_str()
            .unwrap_or("")
            .contains("board approved answer"),
        "{v}"
    );

    // Missing or unregistered X-Board-Agent is refused.
    let (s, v) = t
        .as_user(
            "POST",
            &path,
            Some(json!({ "said": "go ahead", "answer": "yes" })),
        )
        .await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (400, Some("invalid")));
    let (s, v) = t
        .send(
            "POST",
            &path,
            &[
                ("authorization", user_bearer.as_str()),
                ("x-board-agent", "t3:ghost"),
            ],
            Some(json!({ "said": "go ahead", "answer": "yes" })),
        )
        .await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (404, Some("not_found")));

    // Invalid payloads (empty said, overlong said, empty answer, unknown field) are refused.
    for bad in [
        json!({ "said": "   ", "answer": "yes" }),
        json!({ "said": "x".repeat(board_proto::APPROVAL_SAID_CHARS_MAX + 1), "answer": "yes" }),
        json!({ "said": "go ahead", "answer": "   " }),
        json!({ "said": "go ahead", "answer": "yes", "extra": true }),
    ] {
        let (s, v) = t
            .send(
                "POST",
                &path,
                &[
                    ("authorization", user_bearer.as_str()),
                    ("x-board-agent", B),
                ],
                Some(bad),
            )
            .await;
        assert_eq!(
            (s.as_u16(), v["code"].as_str()),
            (400, Some("invalid")),
            "{v}"
        );
    }

    // Valid approved answer records agent, said, and device, and refuses a repeat answer.
    let (s, v) = t
        .send(
            "POST",
            &path,
            &[
                ("authorization", user_bearer.as_str()),
                ("x-board-agent", B),
            ],
            Some(json!({ "said": "go ahead and unblock them", "answer": "yes" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert_eq!(v["status"], "answered");
    assert_eq!(v["answer"], "yes");
    assert_eq!(v["answered_via"], "cli");

    let events = t
        .ok("GET", "/v1/events?since=0&limit=1000", None, None)
        .await;
    let answered = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["type"] == "ask_answered")
        .unwrap();
    assert_eq!(answered["actor"], B);
    assert_eq!(answered["payload"]["id"], id);
    assert_eq!(answered["payload"]["agent"], A);
    assert_eq!(answered["payload"]["approved"], "said");
    assert_eq!(answered["payload"]["said"], "go ahead and unblock them");
    assert_eq!(answered["payload"]["device"], "phone");

    let (s, v) = t
        .send(
            "POST",
            &path,
            &[
                ("authorization", user_bearer.as_str()),
                ("x-board-agent", B),
            ],
            Some(json!({ "said": "again", "answer": "no" })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid_transition"))
    );

    // Answering an ask that carries a BoardChange applies the change with approved="said".
    let (s, ask_change) = ask_with_change(&t, json!({ "parallel_tasks": 2 })).await;
    assert_eq!(s.as_u16(), 200, "{ask_change}");
    let cid = ask_change["id"].as_i64().unwrap();
    let opt = ask_change["options"][0].as_str().unwrap().to_owned();
    let (s, v) = t
        .send(
            "POST",
            &format!("/v1/asks/{cid}/approved"),
            &[
                ("authorization", user_bearer.as_str()),
                ("x-board-agent", B),
            ],
            Some(json!({ "said": "yes raise it to 2", "answer": opt })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    assert_eq!(board["parallel_tasks"], 2);
    let bu = last_board_update(&t).await;
    assert_eq!(bu["actor"], B);
    assert_eq!(bu["payload"]["approved"], "said");
    assert_eq!(bu["payload"]["ask"], cid);
    assert_eq!(bu["payload"]["agent"], A);
    assert_eq!(bu["payload"]["said"], "yes raise it to 2");

    // Approved ask answer preserves ask-change staleness semantics: never lowers a higher
    // board value and skips an expired `until` without failing the answer.
    let until = super::iso(super::start() + TimeDelta::minutes(30));
    let (s, stale_ask) = ask_with_change(
        &t,
        json!({ "parallel_tasks": 1, "temporary": true, "until": until }),
    )
    .await;
    assert_eq!(s.as_u16(), 200, "{stale_ask}");
    let sid = stale_ask["id"].as_i64().unwrap();
    let sopt = stale_ask["options"][0].as_str().unwrap().to_owned();
    t.advance(TimeDelta::minutes(40));
    let (s, v) = t
        .send(
            "POST",
            &format!("/v1/asks/{sid}/approved"),
            &[
                ("authorization", user_bearer.as_str()),
                ("x-board-agent", B),
            ],
            Some(json!({ "said": "go ahead anyway", "answer": sopt })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let board = t.ok("GET", "/v1/boards/factory", None, None).await;
    assert_eq!(board["parallel_tasks"], 2);
    assert_eq!(board["temporary"], true);
    let bu = last_board_update(&t).await;
    assert_eq!(bu["payload"]["expired_before_answer"], true, "{bu}");
    assert_eq!(bu["payload"]["approved"], "said", "{bu}");
    assert_eq!(bu["payload"]["ask"], sid, "{bu}");
}
