use serde_json::json;

use super::{T, USER};

const A: &str = "t3:alpha";

#[tokio::test]
async fn requests_without_credentials_are_refused() {
    let t = T::new();
    let (s, v) = t.send("GET", "/v1/boards", &[], None).await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (401, Some("unauthorized"))
    );
    assert!(v["next"].as_str().unwrap().contains("Bearer"));
    let (s, v) = t
        .send(
            "GET",
            "/v1/boards",
            &[("authorization", "Bearer not-a-token")],
            None,
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (401, Some("unauthorized"))
    );
    let (s, _) = t
        .send("GET", "/v1/boards", &[("authorization", "Basic abc")], None)
        .await;
    assert_eq!(s.as_u16(), 401);
    // Health needs no credentials.
    let (s, v) = t.send("GET", "/healthz", &[], None).await;
    assert_eq!((s.as_u16(), v["ok"].as_bool()), (200, Some(true)));
}

#[tokio::test]
async fn revoked_tokens_stop_working() {
    let t = T::new();
    let created = t
        .as_user(
            "POST",
            "/v1/admin/devices",
            Some(json!({ "name": "laptop" })),
        )
        .await
        .1;
    let token = created["token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 64);
    let bearer = format!("Bearer {token}");
    let (s, _) = t
        .send(
            "GET",
            "/v1/boards",
            &[("authorization", bearer.as_str())],
            None,
        )
        .await;
    assert_eq!(s.as_u16(), 200);

    let list = t.as_user("GET", "/v1/admin/devices", None).await.1;
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["laptop", "mac", "phone"]);
    assert!(
        !list.to_string().contains(&token),
        "tokens are never listed"
    );

    let (s, _) = t.as_user("DELETE", "/v1/admin/devices/laptop", None).await;
    assert_eq!(s.as_u16(), 204);
    let (s, v) = t
        .send(
            "GET",
            "/v1/boards",
            &[("authorization", bearer.as_str())],
            None,
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (401, Some("unauthorized"))
    );
    let (s, _) = t.as_user("DELETE", "/v1/admin/devices/laptop", None).await;
    assert_eq!(s.as_u16(), 404);
    // A revoked name can be reused with a new token.
    let again = t
        .as_user(
            "POST",
            "/v1/admin/devices",
            Some(json!({ "name": "laptop" })),
        )
        .await
        .1;
    assert_ne!(again["token"], created["token"]);
    let (s, _) = t
        .as_user(
            "POST",
            "/v1/admin/devices",
            Some(json!({ "name": "laptop" })),
        )
        .await;
    assert_eq!(s.as_u16(), 400);
}

#[tokio::test]
async fn identity_header_is_trusted_only_for_configured_users() {
    let t = T::new();
    let (s, _) = t
        .send("GET", "/v1/boards", &[("tailscale-user-login", USER)], None)
        .await;
    assert_eq!(s.as_u16(), 200);
    let (s, v) = t
        .send(
            "GET",
            "/v1/boards",
            &[("tailscale-user-login", "stranger@example.com")],
            None,
        )
        .await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (403, Some("forbidden")));
    // The user may call user-only endpoints through the header.
    let (s, _) = t
        .send(
            "GET",
            "/v1/admin/devices",
            &[("tailscale-user-login", USER)],
            None,
        )
        .await;
    assert_eq!(s.as_u16(), 200);

    let untrusting = T::with(|c| c.trust_identity_header = false);
    let (s, v) = untrusting
        .send("GET", "/v1/boards", &[("tailscale-user-login", USER)], None)
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (401, Some("unauthorized"))
    );
}

#[tokio::test]
async fn user_only_endpoints_refuse_device_tokens() {
    let t = T::new();
    let (s, v) = t
        .call(
            "POST",
            "/v1/admin/devices",
            None,
            Some(json!({ "name": "evil" })),
        )
        .await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (403, Some("forbidden")));
    let (s, _) = t.call("GET", "/v1/admin/devices", None, None).await;
    assert_eq!(s.as_u16(), 403);
    let (s, _) = t
        .call("DELETE", "/v1/admin/devices/phone", None, None)
        .await;
    assert_eq!(s.as_u16(), 403);
}

#[tokio::test]
async fn agent_header_must_match_body_agent() {
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let body = json!({ "agent": A, "board": "factory", "title": "Mine" });
    let (s, v) = t
        .call("POST", "/v1/tasks", Some("t3:mallory"), Some(body.clone()))
        .await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (403, Some("forbidden")));
    assert!(v["next"].as_str().unwrap().contains("X-Board-Agent"));
    let (s, v) = t.call("POST", "/v1/tasks", None, Some(body)).await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (400, Some("invalid")));
    let (s, v) = t
        .call(
            "POST",
            "/v1/agents",
            Some("t3:mallory"),
            Some(json!({
                "id": A, "runtime": "claude", "device": "mac",
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 403, "{v}");
    let (s, _) = t
        .call(
            "PUT",
            &format!("/v1/agents/{A}/handoff"),
            Some("t3:mallory"),
            Some(json!({
                "now": "x", "next": "y",
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 403);
    let (s, v) = t
        .send(
            "GET",
            "/v1/boards",
            &[
                ("authorization", &format!("Bearer {}", t.token)),
                ("x-board-agent", "gpt:1"),
            ],
            None,
        )
        .await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (400, Some("invalid")));
}

#[tokio::test]
async fn unknown_endpoints_and_bad_bodies_answer_with_api_errors() {
    let t = T::new();
    let (s, v) = t.call("GET", "/v1/nothing", None, None).await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (404, Some("not_found")));
    let (s, v) = t
        .call(
            "POST",
            "/v1/boards",
            None,
            Some(json!({ "slug": "Bad Slug", "title": "x" })),
        )
        .await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (400, Some("invalid")));
    assert!(v["message"].as_str().unwrap().contains("board slug"), "{v}");
}

#[tokio::test]
async fn health_and_every_answer_name_the_build() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    let t = T::new();
    let (s, v) = t.send("GET", "/healthz", &[], None).await;
    assert_eq!(s.as_u16(), 200);
    assert_eq!(v["version"], board_proto::VERSION);
    assert_eq!(v["commit"], board_proto::COMMIT);
    let migrations = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))
        .unwrap()
        .count();
    assert_eq!(v["schema_version"], migrations, "{v}");
    let commit = board_proto::COMMIT;
    assert!(
        commit == "unknown"
            || (commit.len() == 12 && commit.chars().all(|c| c.is_ascii_hexdigit())),
        "{commit}"
    );

    // Answers and refusals alike carry the build headers.
    for path in ["/healthz", "/v1/boards"] {
        let req = Request::builder().uri(path).body(Body::empty()).unwrap();
        let resp = t.app.clone().oneshot(req).await.unwrap();
        let headers = resp.headers();
        assert_eq!(
            (
                headers[board_proto::VERSION_HEADER].to_str().unwrap(),
                headers[board_proto::COMMIT_HEADER].to_str().unwrap()
            ),
            (board_proto::VERSION, board_proto::COMMIT),
            "{path}"
        );
    }
}
