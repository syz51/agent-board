use chrono::TimeDelta;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{REPO, T, defaults};
use crate::auth::hex;

#[tokio::test]
async fn registration_is_idempotent_and_handles_are_stable() {
    let t = T::new();
    let first = t
        .register(
            "t3:alpha",
            Some("git@github.com:example/app.git"),
            Some("main"),
        )
        .await;
    assert_eq!(first["repo"], REPO);
    assert_eq!(first["state"], "running");
    let digest = hex(&Sha256::digest(b"t3:alpha"));
    assert_eq!(first["handle"], format!("t3:{}", &digest[..4]));

    t.advance(TimeDelta::minutes(5));
    let again = t.register("t3:alpha", Some(REPO), Some("feature")).await;
    assert_eq!(again["handle"], first["handle"]);
    assert_eq!(again["branch"], "feature");
    assert_ne!(again["last_seen"], first["last_seen"]);
    assert_eq!(again["created_at"], first["created_at"]);
    assert_eq!(
        t.count("SELECT COUNT(*) FROM events WHERE type = 'agent_registered'"),
        2,
        "a new agent and a branch change are recorded"
    );
    t.register("t3:alpha", Some(REPO), Some("feature")).await;
    assert_eq!(
        t.count("SELECT COUNT(*) FROM events WHERE type = 'agent_registered'"),
        2
    );
}

#[tokio::test]
async fn handles_grow_until_unique() {
    let t = T::new();
    let id = "claude:session-1";
    let digest = hex(&Sha256::digest(id.as_bytes()));
    // Occupy the 4- and 5-character handles this id would get.
    t.register("claude:other-1", None, None).await;
    t.register("claude:other-2", None, None).await;
    t.exec(&format!(
        "UPDATE agents SET handle = 'claude:{}' WHERE id = 'claude:other-1';
         UPDATE agents SET handle = 'claude:{}' WHERE id = 'claude:other-2';",
        &digest[..4],
        &digest[..5]
    ));
    let agent = t.register(id, None, None).await;
    assert_eq!(agent["handle"], format!("claude:{}", &digest[..6]));
    let codex = t.register("codex:abc", None, None).await;
    assert!(codex["handle"].as_str().unwrap().starts_with("codex:"));
}

#[tokio::test]
async fn forks_start_without_tasks_and_join_repo_boards() {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    let parent = t.register("t3:parent", Some(REPO), None).await;
    assert_eq!(parent["boards"], json!(["factory"]));
    let id = t.task("t3:parent", "factory", "Parent work").await;
    assert!(t.claim("t3:parent", &id).await.0.is_success());
    let fork = t
        .ok(
            "POST",
            "/v1/agents",
            Some("t3:child"),
            Some(json!({
                "id": "t3:child", "runtime": "claude", "device": "mac", "repo": REPO,
                "forked_from": "t3:parent",
            })),
        )
        .await;
    assert_eq!(fork["forked_from"], "t3:parent");
    let mine = t
        .ok("GET", "/v1/tasks?mine=true", Some("t3:child"), None)
        .await;
    assert_eq!(mine, json!([]));
    let listed = t
        .ok("GET", "/v1/agents?board=factory&state=running", None, None)
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn handoffs_are_replaced_whole_with_versions() {
    let t = T::new();
    let a = "t3:alpha";
    t.register(a, None, None).await;
    let path = format!("/v1/agents/{a}/handoff");
    let (s, _) = t.call("GET", &path, Some(a), None).await;
    assert_eq!(s.as_u16(), 404);
    let h = t
        .ok(
            "PUT",
            &path,
            Some(a),
            Some(json!({ "now": "parsing", "next": "run tests" })),
        )
        .await;
    assert_eq!(
        (h["version"].as_i64(), h["blockers"].as_str()),
        (Some(1), Some(""))
    );
    let (s, v) = t
        .call(
            "PUT",
            &path,
            Some(a),
            Some(json!({ "now": "x", "next": "y" })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (409, Some("version_conflict"))
    );
    let h = t
        .ok(
            "PUT",
            &path,
            Some(a),
            Some(json!({ "now": "testing", "next": "push", "version": 1 })),
        )
        .await;
    assert_eq!(h["version"], 2);
    assert_eq!(t.ok("GET", &path, None, None).await["now"], "testing");
    assert_eq!(defaults().handoff_field_chars, 300);
    let long = "x".repeat(280);
    let h = t
        .ok(
            "PUT",
            &path,
            Some(a),
            Some(json!({ "now": long, "next": "push", "version": 2 })),
        )
        .await;
    assert_eq!(h["version"], 3);
    let (s, _) = t
        .call(
            "PUT",
            &path,
            Some(a),
            Some(json!({
                "now": "x".repeat(defaults().handoff_field_chars + 1),
                "next": "y",
                "version": 3,
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 400);
}

#[tokio::test]
async fn handoffs_carry_the_agents_last_change() {
    let t = T::new();
    let (a, b) = ("t3:alpha", "t3:beta");
    t.register(a, Some(REPO), None).await;
    t.register(b, Some(REPO), None).await;
    t.board("factory", &[REPO]).await;
    let task = t.task(a, "factory", "Parse it").await;
    assert!(t.claim(a, &task).await.0.is_success());
    let path = format!("/v1/agents/{a}/handoff");
    let put = |now: &str| json!({ "now": now, "next": "y" });
    t.advance(TimeDelta::minutes(1));
    let h = t.ok("PUT", &path, Some(a), Some(put("claimed"))).await;
    let claimed_at = super::iso(super::start());
    assert_eq!(h["last_change_at"], claimed_at.as_str(), "{h}");
    assert!(h["updated_at"].as_str() > h["last_change_at"].as_str());

    // Presence, a newer handoff and other agents' changes are not this agent's work.
    t.advance(TimeDelta::minutes(1));
    t.ok(
        "POST",
        &format!("/v1/agents/{a}/state"),
        Some(a),
        Some(json!({ "state": "idle" })),
    )
    .await;
    t.register(a, Some(REPO), None).await;
    t.ok(
        "POST",
        &format!("/v1/tasks/{task}/notes"),
        Some(b),
        Some(json!({ "agent": b, "kind": "note", "text": "note: from b" })),
    )
    .await;
    let h = t.ok("GET", &path, None, None).await;
    assert_eq!(h["last_change_at"], claimed_at.as_str());

    // A note or a file touch is a change, dated on the server's clock.
    t.advance(TimeDelta::minutes(1));
    t.ok(
        "POST",
        "/v1/touches",
        Some(a),
        Some(json!({ "agent": a, "repo": REPO, "paths": ["src/lib.rs"] })),
    )
    .await;
    let h = t.ok("GET", &path, None, None).await;
    let touched_at = super::iso(super::start() + TimeDelta::minutes(3));
    assert_eq!(h["last_change_at"], touched_at.as_str());
    assert!(h["last_change_at"].as_str() > h["updated_at"].as_str());
}

#[tokio::test]
async fn ended_agents_release_leases_and_coordinator() {
    let t = T::new();
    let a = "t3:alpha";
    t.register(a, Some(REPO), None).await;
    t.ok(
        "POST",
        "/v1/boards",
        None,
        Some(json!({
            "slug": "auto", "title": "Auto", "autopilot": true,
        })),
    )
    .await;
    t.ok(
        "POST",
        "/v1/leases",
        Some(a),
        Some(json!({
            "agent": a, "repo": REPO, "globs": ["src/**"],
        })),
    )
    .await;
    t.ok(
        "POST",
        "/v1/boards/auto/coordinator",
        Some(a),
        Some(json!({
            "device": "mac", "agent": a,
        })),
    )
    .await;
    let ended = t
        .ok(
            "POST",
            &format!("/v1/agents/{a}/state"),
            Some(a),
            Some(json!({ "state": "ended" })),
        )
        .await;
    assert_eq!(ended["state"], "ended");
    assert_eq!(t.count("SELECT COUNT(*) FROM leases"), 0);
    assert_eq!(
        t.ok("GET", "/v1/boards/auto", None, None).await["coordinator"],
        json!(null)
    );
    let (s, _) = t
        .call(
            "POST",
            &format!("/v1/agents/{a}/state"),
            Some(a),
            Some(json!({
                "state": "idle", "paused_until": "2026-11-07T00:00:00Z",
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 400);
}

#[tokio::test]
async fn re_registration_keeps_the_title_and_native_ids_it_leaves_out() {
    let t = T::new();
    t.ok(
        "POST",
        "/v1/agents",
        Some("t3:lead"),
        Some(json!({
            "id": "t3:lead", "runtime": "claude", "device": "mac", "title": "factory coordinator",
            "repo": REPO, "native_ids": { "t3_thread": "thread-1" },
        })),
    )
    .await;
    // The session-start hook sends no title and only the session id.
    let again = t
        .ok(
            "POST",
            "/v1/agents",
            Some("t3:lead"),
            Some(json!({
                "id": "t3:lead", "runtime": "claude", "device": "mac", "repo": REPO,
                "native_ids": { "claude_session": "session-1" },
            })),
        )
        .await;
    assert_eq!(again["title"], "factory coordinator");
    assert_eq!(
        again["native_ids"],
        json!({ "claude_session": "session-1", "t3_thread": "thread-1" })
    );
    // A sent title replaces it; repo, branch and cwd describe the present, so they clear.
    let renamed = t
        .ok(
            "POST",
            "/v1/agents",
            Some("t3:lead"),
            Some(json!({ "id": "t3:lead", "runtime": "claude", "device": "mac", "title": "renamed" })),
        )
        .await;
    assert_eq!(renamed["title"], "renamed");
    assert_eq!(renamed["repo"], json!(null));
}

#[tokio::test]
async fn devices_register_agents_only_as_themselves() {
    let t = T::new();
    let body = json!({ "id": "t3:alpha", "runtime": "claude", "device": "laptop" });
    let (code, kind, v) = t
        .fail("POST", "/v1/agents", Some("t3:alpha"), Some(body.clone()))
        .await;
    assert_eq!((code, kind.as_str()), (403, "forbidden"), "{v}");
    assert!(v["next"].as_str().unwrap().contains("device=mac"), "{v}");
    assert_eq!(t.count("SELECT count(*) FROM agents"), 0);

    // The user may register an agent for any device.
    let bearer = format!("Bearer {}", t.user_token);
    let (s, v) = t
        .send(
            "POST",
            "/v1/agents",
            &[
                ("authorization", bearer.as_str()),
                ("x-board-agent", "t3:alpha"),
            ],
            Some(body),
        )
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["device"], "laptop");
}

#[tokio::test]
async fn board_listings_leave_out_agents_ended_more_than_a_week_ago() {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    let (live, gone) = ("t3:live", "t3:gone");
    for agent in [live, gone] {
        t.register(agent, Some(REPO), None).await;
    }
    t.ok(
        "POST",
        &format!("/v1/agents/{gone}/state"),
        Some(gone),
        Some(json!({ "state": "ended" })),
    )
    .await;
    let listed = |path: &'static str| {
        let t = &t;
        async move {
            let agents = t.ok("GET", path, None, None).await;
            agents
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    t.advance(TimeDelta::days(7) - TimeDelta::minutes(1));
    assert_eq!(listed("/v1/agents?board=factory").await, [gone, live]);

    // Past a week the ended agent drops out of listings, also when the live one is quiet;
    // asking for ended agents still lists it, and it stays a member of the board.
    t.advance(TimeDelta::minutes(2));
    assert_eq!(listed("/v1/agents?board=factory").await, [live]);
    assert_eq!(listed("/v1/agents?board=factory&state=ended").await, [gone]);
    // Without a board the listing stays complete: clients use it to name agents.
    assert_eq!(listed("/v1/agents").await, [gone, live]);
    let agent = t.ok("GET", &format!("/v1/agents/{gone}"), None, None).await;
    assert_eq!(agent["boards"], json!(["factory"]));

    // Resuming brings it back.
    t.register(gone, Some(REPO), None).await;
    assert_eq!(listed("/v1/agents?board=factory").await, [gone, live]);
}
