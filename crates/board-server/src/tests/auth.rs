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

    let t = T::with(|c| {
        c.commit = "0123456789ab".into();
        c.built = "1762588800".into();
    });
    let (s, v) = t.send("GET", "/healthz", &[], None).await;
    assert_eq!(s.as_u16(), 200);
    assert_eq!(v["version"], board_proto::VERSION);
    assert_eq!(v["commit"], "0123456789ab");
    assert_eq!(v["schema_version"], crate::db::MIGRATIONS.len(), "{v}");

    // Answers and refusals alike carry the build headers.
    for path in ["/healthz", "/v1/boards"] {
        let req = Request::builder().uri(path).body(Body::empty()).unwrap();
        let resp = t.app.clone().oneshot(req).await.unwrap();
        let headers = resp.headers();
        assert_eq!(
            (
                headers[board_proto::VERSION_HEADER].to_str().unwrap(),
                headers[board_proto::COMMIT_HEADER].to_str().unwrap(),
                headers[board_proto::BUILT_HEADER].to_str().unwrap()
            ),
            (board_proto::VERSION, "0123456789ab", "1762588800"),
            "{path}"
        );
    }

    // A server started without a commit or stamp reports `unknown`.
    let t = T::new();
    assert_eq!(
        t.ok("GET", "/healthz", None, None).await["commit"],
        "unknown"
    );
    let req = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let resp = t.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.headers()[board_proto::BUILT_HEADER], "unknown");
}

#[tokio::test]
async fn browser_user_session_can_patch_tasks_with_user_or_omitted_agent() {
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let t1 = t.task(A, "factory", "First task").await;
    let t2 = t.task(A, "factory", "Second task").await;
    let t3 = t.task(A, "factory", "Third task").await;
    let t4 = t.task(A, "factory", "Owned doing task").await;
    assert_eq!(t.claim(A, &t4).await.0.as_u16(), 200);

    let user_headers = [("tailscale-user-login", USER)];

    // Pin rank with `agent: "user"` (matching Web UI payload, no `X-Board-Agent`).
    let v1 = t.get_task(&t1).await["version"].as_i64().unwrap();
    let (s, patched) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{t1}"),
            &user_headers,
            Some(json!({ "agent": "user", "version": v1, "rank": 1 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{patched}");
    assert_eq!(patched["rank"], 1);

    // Relative rank with `agent` omitted shifts `t1` down; both events attribute to `"user"`.
    let v2 = t.get_task(&t2).await["version"].as_i64().unwrap();
    let (s, patched) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{t2}"),
            &user_headers,
            Some(json!({ "version": v2, "before": t1 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{patched}");
    assert_eq!(patched["rank"], 1);
    assert_eq!(t.get_task(&t1).await["rank"], 2);

    // Priority change with `agent: "user"`.
    let v1 = t.get_task(&t1).await["version"].as_i64().unwrap();
    let (s, patched) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{t1}"),
            &user_headers,
            Some(json!({ "agent": "user", "version": v1, "priority": "p1" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{patched}");
    assert_eq!(patched["priority"], "p1");
    assert!(patched.get("rank").is_none(), "{patched}");

    // Cool into Icebox with `agent` omitted, then revive with `agent: "user"`.
    let v1 = patched["version"].as_i64().unwrap();
    let (s, cooled) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{t1}"),
            &user_headers,
            Some(json!({ "version": v1, "cooled": true })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{cooled}");
    assert!(cooled["cooled_at"].is_string(), "{cooled}");
    assert_eq!(cooled["ready"], false);

    let v1 = cooled["version"].as_i64().unwrap();
    let (s, revived) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{t1}"),
            &user_headers,
            Some(json!({ "agent": "user", "version": v1, "cooled": false })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{revived}");
    assert!(revived.get("cooled_at").is_none(), "{revived}");
    assert_eq!(revived["ready"], true);

    // Drop with reason (`agent: "user"`), then send a late reason on the already-dropped task
    // (`agent` omitted). `GET /v1/tasks/{id}` stays 200 OK and history records both with `"user"`.
    let v3 = t.get_task(&t3).await["version"].as_i64().unwrap();
    let (s, dropped) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{t3}"),
            &user_headers,
            Some(json!({
                "agent": "user",
                "version": v3,
                "status": "dropped",
                "reason": "No longer needed"
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{dropped}");
    assert_eq!(dropped["status"], "dropped");

    let v3 = dropped["version"].as_i64().unwrap();
    let (s, late) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{t3}"),
            &user_headers,
            Some(json!({
                "version": v3,
                "status": "dropped",
                "reason": "Superseded by v2"
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{late}");

    let detail = t.ok("GET", &format!("/v1/tasks/{t3}"), None, None).await;
    let history = detail["history"].as_array().unwrap();
    let drop_events: Vec<&serde_json::Value> = history
        .iter()
        .filter(|e| e["type"] == "task_updated")
        .collect();
    assert_eq!(drop_events.len(), 2, "{history:?}");
    assert_eq!(drop_events[0]["actor"], "user");
    assert_eq!(drop_events[0]["payload"]["to"], "dropped");
    assert_eq!(drop_events[0]["payload"]["reason"], "No longer needed");
    assert_eq!(drop_events[1]["actor"], "user");
    assert_eq!(drop_events[1]["payload"]["fields"], json!(["reason"]));
    assert_eq!(drop_events[1]["payload"]["reason"], "Superseded by v2");

    // Browser user can drop an agent-owned doing task without `X-Board-Agent`.
    let v4 = t.get_task(&t4).await["version"].as_i64().unwrap();
    let (s, dropped_owned) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{t4}"),
            &user_headers,
            Some(json!({
                "agent": "user",
                "version": v4,
                "status": "dropped",
                "reason": "Cancelled by operator"
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{dropped_owned}");
    assert_eq!(dropped_owned["status"], "dropped");

    // Moving a blocked task to `doing` as `"user"` requires naming a live agent `owner`.
    let v2 = t.get_task(&t2).await["version"].as_i64().unwrap();
    let (s, blocked) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{t2}"),
            &user_headers,
            Some(json!({
                "version": v2,
                "status": "blocked",
                "blocked_reason": "waiting"
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{blocked}");
    let v2 = blocked["version"].as_i64().unwrap();
    let (s, refused) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{t2}"),
            &user_headers,
            Some(json!({ "agent": "user", "version": v2, "status": "doing" })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), refused["code"].as_str()),
        (400, Some("invalid")),
        "{refused}"
    );
    let (s, assigned) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{t2}"),
            &user_headers,
            Some(json!({ "agent": "user", "version": v2, "status": "doing", "owner": A })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{assigned}");
    assert_eq!(assigned["status"], "doing");
    assert_eq!(assigned["owner"], A);

    // Every `task_updated` event on `t1` was attributed to `"user"`.
    let detail_t1 = t.ok("GET", &format!("/v1/tasks/{t1}"), None, None).await;
    let actors: Vec<&str> = detail_t1["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "task_updated")
        .map(|e| e["actor"].as_str().unwrap())
        .collect();
    assert!(!actors.is_empty());
    assert!(actors.iter().all(|a| *a == "user"), "{actors:?}");

    // `--user` bearer token without `X-Board-Agent` also succeeds as `"user"`.
    let v1 = t.get_task(&t1).await["version"].as_i64().unwrap();
    let (s, by_token) = t
        .as_user(
            "PATCH",
            &format!("/v1/tasks/{t1}"),
            Some(json!({ "version": v1, "priority": "p2" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{by_token}");
    assert_eq!(by_token["priority"], "p2");

    // Browser user sending a conflicting `X-Board-Agent` with `agent: "user"` or omitted is refused.
    let v1 = by_token["version"].as_i64().unwrap();
    for body in [
        json!({ "agent": "user", "version": v1, "priority": "p3" }),
        json!({ "version": v1, "priority": "p3" }),
    ] {
        let (s, err) = t
            .send(
                "PATCH",
                &format!("/v1/tasks/{t1}"),
                &[("tailscale-user-login", USER), ("x-board-agent", A)],
                Some(body),
            )
            .await;
        assert_eq!(
            (s.as_u16(), err["code"].as_str()),
            (403, Some("forbidden")),
            "{err}"
        );
    }
}

#[tokio::test]
async fn browser_user_session_can_rank_board_tasks_with_user_or_omitted_agent() {
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let t1 = t.task(A, "factory", "One").await;
    let t2 = t.task(A, "factory", "Two").await;
    let t3 = t.task(A, "factory", "Three").await;

    let user_headers = [("tailscale-user-login", USER)];

    // Rank with `agent: "user"` and no `X-Board-Agent`.
    let (s, ranked) = t
        .send(
            "POST",
            "/v1/boards/factory/rank",
            &user_headers,
            Some(json!({ "agent": "user", "tasks": [t3, t1], "clear_others": false })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{ranked}");
    assert_eq!(ranked["tasks"][0]["id"], t3);
    assert_eq!(ranked["tasks"][0]["rank"], 1);
    assert_eq!(ranked["tasks"][1]["id"], t1);
    assert_eq!(ranked["tasks"][1]["rank"], 2);

    // Rank with `agent` omitted and `clear_others: true`.
    let (s, ranked) = t
        .send(
            "POST",
            "/v1/boards/factory/rank",
            &user_headers,
            Some(json!({ "tasks": [t2], "clear_others": true })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{ranked}");
    assert_eq!(ranked["tasks"][0]["id"], t2);
    assert_eq!(ranked["tasks"][0]["rank"], 1);
    assert!(t.get_task(&t1).await.get("rank").is_none());
    assert!(t.get_task(&t3).await.get("rank").is_none());

    // All rank-update events were attributed to `"user"`.
    for id in [&t1, &t2, &t3] {
        let detail = t.ok("GET", &format!("/v1/tasks/{id}"), None, None).await;
        let actors: Vec<&str> = detail["history"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "task_updated")
            .map(|e| e["actor"].as_str().unwrap())
            .collect();
        assert!(!actors.is_empty(), "{id}: {detail}");
        assert!(actors.iter().all(|a| *a == "user"), "{id}: {actors:?}");
    }

    // Browser user sending a conflicting `X-Board-Agent` is refused.
    for body in [
        json!({ "agent": "user", "tasks": [t1] }),
        json!({ "tasks": [t1] }),
    ] {
        let (s, err) = t
            .send(
                "POST",
                "/v1/boards/factory/rank",
                &[("tailscale-user-login", USER), ("x-board-agent", A)],
                Some(body),
            )
            .await;
        assert_eq!(
            (s.as_u16(), err["code"].as_str()),
            (403, Some("forbidden")),
            "{err}"
        );
    }
}

#[tokio::test]
async fn device_token_still_requires_valid_agent_and_matching_header_on_patch_and_rank() {
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let t1 = t.task(A, "factory", "Guarded").await;
    let v1 = t.get_task(&t1).await["version"].as_i64().unwrap();

    // PATCH /v1/tasks/{id}: device token cannot use `agent: "user"`, omit `agent`, omit
    // `X-Board-Agent`, mismatch `X-Board-Agent`, or send an invalid agent id.
    let patch_cases = [
        (
            None,
            json!({ "agent": "user", "version": v1, "priority": "p1" }),
            403,
            "forbidden",
        ),
        (
            Some(A),
            json!({ "agent": "user", "version": v1, "priority": "p1" }),
            403,
            "forbidden",
        ),
        (
            None,
            json!({ "version": v1, "priority": "p1" }),
            400,
            "invalid",
        ),
        (
            Some(A),
            json!({ "version": v1, "priority": "p1" }),
            400,
            "invalid",
        ),
        (
            None,
            json!({ "agent": A, "version": v1, "priority": "p1" }),
            400,
            "invalid",
        ),
        (
            Some("t3:other"),
            json!({ "agent": A, "version": v1, "priority": "p1" }),
            403,
            "forbidden",
        ),
        (
            Some(A),
            json!({ "agent": "not-an-agent", "version": v1, "priority": "p1" }),
            400,
            "invalid",
        ),
    ];
    for (header, body, expected_status, expected_code) in patch_cases {
        let (s, err) = t
            .call("PATCH", &format!("/v1/tasks/{t1}"), header, Some(body))
            .await;
        assert_eq!(
            (s.as_u16(), err["code"].as_str()),
            (expected_status, Some(expected_code)),
            "PATCH header={header:?}: {err}"
        );
    }

    // POST /v1/boards/{slug}/rank: same strict device-token enforcement.
    let rank_cases = [
        (
            None,
            json!({ "agent": "user", "tasks": [t1] }),
            403,
            "forbidden",
        ),
        (
            Some(A),
            json!({ "agent": "user", "tasks": [t1] }),
            403,
            "forbidden",
        ),
        (None, json!({ "tasks": [t1] }), 400, "invalid"),
        (Some(A), json!({ "tasks": [t1] }), 400, "invalid"),
        (None, json!({ "agent": A, "tasks": [t1] }), 400, "invalid"),
        (
            Some("t3:other"),
            json!({ "agent": A, "tasks": [t1] }),
            403,
            "forbidden",
        ),
        (
            Some(A),
            json!({ "agent": "gpt:1", "tasks": [t1] }),
            400,
            "invalid",
        ),
    ];
    for (header, body, expected_status, expected_code) in rank_cases {
        let (s, err) = t
            .call("POST", "/v1/boards/factory/rank", header, Some(body))
            .await;
        assert_eq!(
            (s.as_u16(), err["code"].as_str()),
            (expected_status, Some(expected_code)),
            "RANK header={header:?}: {err}"
        );
    }

    // Valid device-token call with matching `X-Board-Agent` and `AgentId` succeeds as that agent.
    let (s, ok_patch) = t
        .call(
            "PATCH",
            &format!("/v1/tasks/{t1}"),
            Some(A),
            Some(json!({ "agent": A, "version": v1, "priority": "p1" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{ok_patch}");
    let (s, ok_rank) = t
        .call(
            "POST",
            "/v1/boards/factory/rank",
            Some(A),
            Some(json!({ "agent": A, "tasks": [t1] })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{ok_rank}");
    let detail = t.ok("GET", &format!("/v1/tasks/{t1}"), None, None).await;
    let update_actors: Vec<&str> = detail["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "task_updated")
        .map(|e| e["actor"].as_str().unwrap())
        .collect();
    assert_eq!(update_actors, [A, A]);
}
