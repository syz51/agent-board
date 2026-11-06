//! The library as the `board` binary uses it: a database file, `create_device`, `app` and
//! `backup`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use board_server::{AppState, Db, ServerConfig, SystemClock, app, backup, create_device};
use serde_json::{Value, json};
use tower::ServiceExt;

const TABLES: [&str; 6] = [
    "devices",
    "agents",
    "boards",
    "tasks",
    "events",
    "board_members",
];

async fn call(router: &axum::Router, token: &str, method: &str, path: &str, body: Value) -> Value {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("x-board-agent", "t3:alpha")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(status.is_success(), "{method} {path}: {status} {value}");
    value
}

fn counts(path: &std::path::Path) -> Vec<i64> {
    let conn = rusqlite::Connection::open(path).unwrap();
    TABLES
        .iter()
        .map(|table| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        })
        .collect()
}

#[tokio::test]
async fn a_file_database_serves_and_backs_up() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("board.db");
    let token = create_device(&db_path, &"mac".parse().unwrap(), false).unwrap();
    assert_eq!(token.len(), 64);
    assert!(create_device(&db_path, &"mac".parse().unwrap(), false).is_err());

    let config = ServerConfig::new(&db_path);
    let state = AppState::new(Db::open(&db_path).unwrap(), &config, Arc::new(SystemClock));
    let router = app(state.clone());
    call(&router, &token, "POST", "/v1/agents", json!({
        "id": "t3:alpha", "runtime": "claude", "device": "mac", "repo": "github.com/example/app",
    }))
    .await;
    call(
        &router,
        &token,
        "POST",
        "/v1/boards",
        json!({
            "slug": "factory", "title": "Factory", "repos": ["github.com/example/app"],
        }),
    )
    .await;
    for title in ["First", "Second", "Third"] {
        call(
            &router,
            &token,
            "POST",
            "/v1/tasks",
            json!({
                "agent": "t3:alpha", "board": "factory", "title": title,
            }),
        )
        .await;
    }

    let out = dir.path().join("backup.db");
    backup(&db_path, &out).unwrap();
    let live = counts(&db_path);
    assert_eq!(live[..4], [1, 1, 1, 3]);
    assert_eq!(counts(&out), live);
    let error = backup(&db_path, &out).unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error}");

    // The backup opens as a board database and serves the same tasks.
    drop(router);
    state.shutdown();
    let restored = AppState::new(Db::open(&out).unwrap(), &config, Arc::new(SystemClock));
    let tasks = call(
        &app(restored),
        &token,
        "GET",
        "/v1/tasks?board=factory",
        json!(null),
    )
    .await;
    let titles: Vec<&str> = tasks
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["First", "Second", "Third"]);
}
