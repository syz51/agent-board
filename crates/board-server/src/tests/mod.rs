//! In-process tests through the HTTP layer, over an in-memory database and a manual clock.

mod agents;
mod asks;
mod auth;
mod boards;
mod events;
mod feedback;
mod leases;
mod query_cost;
mod snapshot;
mod tasks;
mod telegram;
mod temporary;
mod updates;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use board_proto::Timestamp;
use chrono::{SecondsFormat, TimeDelta, TimeZone, Utc};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{AppState, Db, ManualClock, ServerConfig, Settings, app};

pub(crate) const USER: &str = "user@example.com";
pub(crate) const REPO: &str = "github.com/example/app";

pub(crate) fn start() -> Timestamp {
    Utc.with_ymd_and_hms(2026, 11, 6, 12, 0, 0).unwrap()
}

// Durations from the server defaults, so a change to a default in `config.rs` alone keeps
// the tests meaningful and green.

fn secs(n: u64) -> TimeDelta {
    TimeDelta::seconds(i64::try_from(n).unwrap())
}

pub(crate) fn defaults() -> Settings {
    Settings::default()
}

/// An owner unseen this long is stale.
pub(crate) fn stale_after() -> TimeDelta {
    secs(defaults().owner_stale_after_secs)
}

/// The default file lease TTL.
pub(crate) fn lease_ttl() -> TimeDelta {
    secs(defaults().lease_ttl_secs)
}

/// The default coordinator lease TTL.
pub(crate) fn coordinator_ttl() -> TimeDelta {
    secs(defaults().coordinator_ttl_secs)
}

/// Touches this recent count as overlapping.
pub(crate) fn touch_window() -> TimeDelta {
    secs(defaults().touch_window_secs)
}

/// A timestamp as the API writes it.
pub(crate) fn iso(at: Timestamp) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A server over an in-memory database, with a device token (`mac`) and a user token
/// (`phone`).
pub(crate) struct T {
    pub app: Router,
    pub state: AppState,
    pub clock: ManualClock,
    pub token: String,
    pub user_token: String,
}

impl T {
    pub fn new() -> Self {
        Self::with(|_| {})
    }

    pub fn with(configure: impl FnOnce(&mut ServerConfig)) -> Self {
        Self::over(Db::open_in_memory().unwrap(), configure)
    }

    fn config(configure: impl FnOnce(&mut ServerConfig)) -> ServerConfig {
        let mut config = ServerConfig::new("unused.db");
        config.users = vec![USER.to_owned()];
        configure(&mut config);
        config
    }

    /// A server over `db`, which must be new.
    pub fn over(db: Db, configure: impl FnOnce(&mut ServerConfig)) -> Self {
        let config = Self::config(configure);
        let clock = ManualClock::new(start());
        let state = AppState::new(db, &config, Arc::new(clock.clone()));
        let token = state.create_device(&"mac".parse().unwrap(), false).unwrap();
        let user_token = state
            .create_device(&"phone".parse().unwrap(), true)
            .unwrap();
        Self {
            app: app(state.clone()),
            state,
            clock,
            token,
            user_token,
        }
    }

    /// The server started again, at the clock's current time, over `db`: this server's
    /// database reopened. Tokens and the clock carry over.
    pub fn restart(&self, db: Db) -> Self {
        let state = AppState::new(db, &Self::config(|_| {}), Arc::new(self.clock.clone()));
        Self {
            app: app(state.clone()),
            state,
            clock: self.clock.clone(),
            token: self.token.clone(),
            user_token: self.user_token.clone(),
        }
    }

    /// Sends a request with exactly these headers.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let req = match body {
            Some(body) => req
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let response = self.app.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value)
    }

    /// Sends a request as the `mac` device, speaking for `agent` when given.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        agent: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let bearer = format!("Bearer {}", self.token);
        let mut headers = vec![("authorization", bearer.as_str())];
        if let Some(agent) = agent {
            headers.push(("x-board-agent", agent));
        }
        self.send(method, path, &headers, body).await
    }

    /// Like [`T::call`], asserting success.
    pub async fn ok(
        &self,
        method: &str,
        path: &str,
        agent: Option<&str>,
        body: Option<Value>,
    ) -> Value {
        let (status, value) = self.call(method, path, agent, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }

    /// Like [`T::call`], asserting failure; returns the status and error code.
    pub async fn fail(
        &self,
        method: &str,
        path: &str,
        agent: Option<&str>,
        body: Option<Value>,
    ) -> (u16, String, Value) {
        let (status, value) = self.call(method, path, agent, body).await;
        assert!(!status.is_success(), "{method} {path} succeeded: {value}");
        assert!(value["next"].is_string(), "error without next: {value}");
        (
            status.as_u16(),
            value["code"].as_str().unwrap_or_default().to_owned(),
            value,
        )
    }

    /// Sends a request as the user, through the user token.
    pub async fn as_user(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let bearer = format!("Bearer {}", self.user_token);
        self.send(method, path, &[("authorization", bearer.as_str())], body)
            .await
    }

    pub async fn register(&self, id: &str, repo: Option<&str>, branch: Option<&str>) -> Value {
        self.register_as(id, "claude", repo, branch).await
    }

    pub async fn register_as(
        &self,
        id: &str,
        runtime: &str,
        repo: Option<&str>,
        branch: Option<&str>,
    ) -> Value {
        self.ok(
            "POST",
            "/v1/agents",
            Some(id),
            Some(json!({
                "id": id, "runtime": runtime, "device": "mac", "repo": repo, "branch": branch,
            })),
        )
        .await
    }

    pub async fn board(&self, slug: &str, repos: &[&str]) -> Value {
        self.ok(
            "POST",
            "/v1/boards",
            None,
            Some(json!({ "slug": slug, "title": format!("Board {slug}"), "repos": repos })),
        )
        .await
    }

    /// Creates a task and returns its id.
    pub async fn task(&self, agent: &str, board: &str, title: &str) -> String {
        self.task_with(agent, json!({ "board": board, "title": title }))
            .await
    }

    pub async fn task_with(&self, agent: &str, mut body: Value) -> String {
        body["agent"] = json!(agent);
        let task = self.ok("POST", "/v1/tasks", Some(agent), Some(body)).await;
        task["id"].as_str().unwrap().to_owned()
    }

    pub async fn claim(&self, agent: &str, task: &str) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/v1/tasks/{task}/claim"),
            Some(agent),
            Some(json!({ "agent": agent })),
        )
        .await
    }

    pub async fn get_task(&self, task: &str) -> Value {
        self.ok("GET", &format!("/v1/tasks/{task}"), None, None)
            .await["task"]
            .clone()
    }

    pub async fn patch(&self, agent: &str, task: &str, mut body: Value) -> (StatusCode, Value) {
        let version = self.get_task(task).await["version"].clone();
        body["agent"] = json!(agent);
        if body.get("version").is_none() {
            body["version"] = version;
        }
        self.call(
            "PATCH",
            &format!("/v1/tasks/{task}"),
            Some(agent),
            Some(body),
        )
        .await
    }

    pub async fn done(&self, agent: &str, task: &str, evidence: Value) -> (StatusCode, Value) {
        let version = self.get_task(task).await["version"].clone();
        self.call(
            "POST",
            &format!("/v1/tasks/{task}/done"),
            Some(agent),
            Some(json!({ "agent": agent, "version": version, "evidence": evidence })),
        )
        .await
    }

    pub fn advance(&self, by: TimeDelta) {
        self.clock.advance(by);
    }

    /// Runs raw SQL against the database.
    pub fn exec(&self, sql: &str) {
        self.state
            .db()
            .write(
                || self.state.now(),
                self.state.settings(),
                |cx| {
                    cx.db.execute_batch(sql)?;
                    Ok(())
                },
            )
            .unwrap();
    }

    /// The first column of every row of a query, as strings.
    pub fn strings(&self, sql: &str) -> Vec<String> {
        self.state
            .db()
            .read(self.state.now(), self.state.settings(), |cx| {
                let mut stmt = cx.db.prepare(sql)?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .unwrap()
    }

    pub fn count(&self, sql: &str) -> i64 {
        self.state
            .db()
            .read(self.state.now(), self.state.settings(), |cx| {
                Ok(cx.db.query_row(sql, [], |r| r.get(0))?)
            })
            .unwrap()
    }

    /// Event types after `since`, in order.
    pub async fn event_types(&self, since: i64) -> Vec<String> {
        let page = self
            .ok(
                "GET",
                &format!("/v1/events?since={since}&limit=1000"),
                None,
                None,
            )
            .await;
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["type"].as_str().unwrap().to_owned())
            .collect()
    }
}

pub(crate) fn check(exit_code: i32) -> Value {
    json!([{ "kind": "check", "value": "cargo test", "exit_code": exit_code }])
}
