use serde_json::json;

use super::{T, USER};

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
        body(&"q".repeat(1001), vec![]),
        body("ok?", (0..9).map(|i| i.to_string()).collect()),
        body("ok?", vec!["x".repeat(61)]),
        body("ok?", vec!["two\nlines".to_owned()]),
    ] {
        let (status, code, _) = t.fail("POST", "/v1/asks", Some(A), Some(bad)).await;
        assert_eq!((status, code.as_str()), (400, "invalid"));
    }
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
