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
