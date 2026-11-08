use board_proto::Timestamp;
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
async fn ended_agents_release_file_leases_and_keep_the_coordinator_lease() {
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
    // T3 ends an idle session while its thread lives on: the coordinator lease runs on
    // until the daemon renews or releases it, or it expires.
    let coordinator =
        async || t.ok("GET", "/v1/boards/auto", None, None).await["coordinator"].clone();
    assert_eq!(coordinator().await, json!(a));
    let events = t.ok("GET", "/v1/events?board=auto", None, None).await["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "board_updated")
        .count();
    assert_eq!(events, 1, "only the claim: ending changes no coordinator");
    t.advance(TimeDelta::minutes(46));
    assert_eq!(coordinator().await, json!(null), "not renewed while ended");
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
async fn a_boards_coordinator_keeps_its_title_against_a_workers_registration() {
    let t = T::new();
    let a = "t3:lead";
    let register = async |role: &str, title: &str| {
        t.ok(
            "POST",
            "/v1/agents",
            Some(a),
            Some(json!({
                "id": a, "runtime": "claude", "device": "mac", "role": role, "title": title,
                "repo": REPO, "native_ids": { "t3_link": format!("https://t3.example.test/{}", title.replace(' ', "-")) },
            })),
        )
        .await
    };
    // Before it coordinates, the thread's own title (the hooks') wins.
    assert_eq!(
        register("worker", "Fix the parser").await["title"],
        "Fix the parser"
    );
    t.ok(
        "POST",
        "/v1/boards",
        None,
        Some(json!({ "slug": "auto", "title": "Auto", "autopilot": true })),
    )
    .await;
    // The daemon registers its coordinator with the board's name, then claims the lease.
    assert_eq!(
        register("coordinator", "auto coordinator").await["title"],
        "auto coordinator"
    );
    t.ok(
        "POST",
        "/v1/boards/auto/coordinator",
        Some(a),
        Some(json!({ "device": "mac", "agent": a })),
    )
    .await;
    // The hooks register it as a worker with the thread's title: role and title stay, the
    // rest is taken.
    let hooked = register("worker", "Renamed by T3").await;
    assert_eq!(
        (&hooked["role"], &hooked["title"]),
        (&json!("coordinator"), &json!("auto coordinator"))
    );
    assert_eq!(
        hooked["native_ids"]["t3_link"],
        "https://t3.example.test/Renamed-by-T3"
    );
}

#[tokio::test]
async fn a_workers_registration_between_the_daemons_and_its_claim_keeps_the_coordinator_title() {
    let t = T::new();
    let a = "t3:lead";
    let register = async |role: &str, title: &str| {
        t.ok(
            "POST",
            "/v1/agents",
            Some(a),
            Some(json!({
                "id": a, "runtime": "claude", "device": "mac", "role": role, "title": title,
                "repo": REPO,
            })),
        )
        .await
    };
    t.ok(
        "POST",
        "/v1/boards",
        None,
        Some(json!({ "slug": "auto", "title": "Auto", "autopilot": true })),
    )
    .await;
    register("coordinator", "auto coordinator").await;
    // The thread's session-start hook lands before the daemon claims the lease.
    assert_eq!(
        register("worker", "Thread title").await["title"],
        "auto coordinator"
    );
    let claimed = t
        .ok(
            "POST",
            "/v1/boards/auto/coordinator",
            Some(a),
            Some(json!({ "device": "mac", "agent": a })),
        )
        .await;
    assert_eq!(claimed["coordinator"], a);
    let agent = register("worker", "Thread title").await;
    assert_eq!(
        (&agent["role"], &agent["title"]),
        (&json!("coordinator"), &json!("auto coordinator"))
    );
}

#[tokio::test]
async fn a_t3_link_must_be_an_https_link() {
    let t = T::new();
    for link in [
        "javascript:alert(1)",
        "http://t3.example.test/x",
        "https://",
        "https://a b",
    ] {
        let (s, v) = t
            .call(
                "POST",
                "/v1/agents",
                Some("t3:alpha"),
                Some(json!({
                    "id": "t3:alpha", "runtime": "claude", "device": "mac",
                    "native_ids": { "t3_link": link },
                })),
            )
            .await;
        assert_eq!(s, 400, "{link}: {v}");
        assert!(v["message"].as_str().unwrap().contains("t3_link"), "{v}");
    }
    let ok = t
        .ok(
            "POST",
            "/v1/agents",
            Some("t3:alpha"),
            Some(json!({
                "id": "t3:alpha", "runtime": "claude", "device": "mac",
                "native_ids": { "t3_link": "https://t3.example.test/thread-1" },
            })),
        )
        .await;
    assert_eq!(
        ok["native_ids"]["t3_link"],
        "https://t3.example.test/thread-1"
    );
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
async fn board_listings_leave_out_agents_unseen_for_more_than_a_week() {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    let (coord, crashed, gone, limited) = ("t3:coord", "t3:crashed", "t3:gone", "t3:limited");
    let (seen, stopped, waiting) = ("t3:seen", "t3:stopped", "t3:waiting");
    for agent in [coord, crashed, gone, limited, seen, stopped, waiting] {
        t.register(agent, Some(REPO), None).await;
    }
    let set = |agent: &'static str, body: serde_json::Value| {
        let t = &t;
        async move {
            t.ok(
                "POST",
                &format!("/v1/agents/{agent}/state"),
                Some(agent),
                Some(body),
            )
            .await;
        }
    };
    // `crashed` stays running: a session that dies never sends `ended`.
    set(gone, json!({ "state": "ended" })).await;
    // A weekly usage limit: the reset comes after a week of silence. `stopped` waits for
    // it too, though T3 has ended its session; `waiting` has no reset, so waits for the user.
    let reset = (t.state.now() + TimeDelta::days(8)).to_rfc3339();
    for agent in [limited, stopped] {
        set(agent, json!({ "state": "paused", "paused_until": reset })).await;
    }
    set(stopped, json!({ "state": "ended" })).await;
    set(waiting, json!({ "state": "paused" })).await;
    // The daemon renews a coordinator lease without marking the agent seen.
    let lease_until = (t.state.now() + TimeDelta::days(30)).timestamp_millis();
    t.exec(&format!(
        "UPDATE boards SET coordinator = '{coord}', coordinator_device = 'mac', \
         coordinator_expires_at = {lease_until} WHERE slug = 'factory'"
    ));
    // Sorted by id: the server orders agents by handle, a hash.
    let listed = |path: &'static str| {
        let t = &t;
        async move {
            let agents = t.ok("GET", path, None, None).await;
            let mut ids = agents
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            ids.sort();
            ids
        }
    };
    t.advance(TimeDelta::days(7) - TimeDelta::minutes(1));
    set(seen, json!({ "state": "idle" })).await;
    assert_eq!(
        listed("/v1/agents?board=factory").await,
        [coord, crashed, gone, limited, seen, stopped, waiting]
    );

    // Past a week unseen agents drop out of listings whatever their state, also for a
    // state filter. Kept are the one seen since, those waiting for a reset still ahead or
    // for the user, and the coordinator holding the lease. Asking for ended agents still
    // lists them all, and they stay members of the board.
    t.advance(TimeDelta::minutes(2));
    assert_eq!(
        listed("/v1/agents?board=factory").await,
        [coord, limited, seen, stopped, waiting]
    );
    assert_eq!(
        listed("/v1/agents?board=factory&state=running").await,
        [coord]
    );
    assert_eq!(
        listed("/v1/agents?board=factory&state=paused").await,
        [limited, waiting]
    );
    assert_eq!(
        listed("/v1/agents?board=factory&state=ended").await,
        [gone, stopped]
    );
    // Without a board the listing stays complete: clients use it to name agents.
    assert_eq!(
        listed("/v1/agents").await,
        [coord, crashed, gone, limited, seen, stopped, waiting]
    );
    for agent in [crashed, gone] {
        let agent = t
            .ok("GET", &format!("/v1/agents/{agent}"), None, None)
            .await;
        assert_eq!(agent["boards"], json!(["factory"]));
    }

    // Once the reset has passed, the agents that waited for it drop out too; so does the
    // coordinator once its lease has lapsed.
    t.advance(TimeDelta::days(1));
    assert_eq!(
        listed("/v1/agents?board=factory").await,
        [coord, seen, waiting]
    );
    t.exec("UPDATE boards SET coordinator_expires_at = 0 WHERE slug = 'factory'");
    assert_eq!(listed("/v1/agents?board=factory").await, [seen, waiting]);

    // Any newer sign of life brings an agent back: a state update or a registration.
    set(crashed, json!({ "state": "idle" })).await;
    t.register(gone, Some(REPO), None).await;
    assert_eq!(
        listed("/v1/agents?board=factory").await,
        [crashed, gone, seen, waiting]
    );
}

#[tokio::test]
async fn an_ended_agent_that_calls_again_is_running_again() {
    let t = T::new();
    let a = "t3:alpha";
    t.register(a, Some(REPO), None).await;
    t.board("factory", &[REPO]).await;
    let path = format!("/v1/agents/{a}/state");
    let ended = json!({ "state": "ended" });
    t.ok("POST", &path, Some(a), Some(ended.clone())).await;
    // Ending again, as a second end-of-session hook does, changes nothing.
    t.ok("POST", &path, Some(a), Some(ended)).await;
    let changes = "SELECT COUNT(*) FROM events WHERE type = 'agent_state_changed'";
    assert_eq!(t.count(changes), 1);

    // Any call by the ended agent marks it running, announced once.
    t.task(a, "factory", "Still working").await;
    t.task(a, "factory", "And more").await;
    let agent = t.ok("GET", &format!("/v1/agents/{a}"), None, None).await;
    assert_eq!(agent["state"], "running", "{agent}");
    assert_eq!(t.count(changes), 2);
    assert_eq!(
        t.strings(
            "SELECT json_extract(payload, '$.from') || '>' || json_extract(payload, '$.to') \
             FROM events WHERE type = 'agent_state_changed' ORDER BY seq DESC LIMIT 1"
        ),
        ["ended>running"]
    );
    // A device claiming the coordinator lease for an ended agent is no sign of it working.
    t.ok("POST", &path, Some(a), Some(json!({ "state": "ended" })))
        .await;
    t.ok(
        "POST",
        "/v1/boards",
        None,
        Some(json!({ "slug": "auto", "title": "Auto", "autopilot": true })),
    )
    .await;
    t.ok(
        "POST",
        "/v1/boards/auto/coordinator",
        Some(a),
        Some(json!({ "device": "mac", "agent": a })),
    )
    .await;
    let agent = t.ok("GET", &format!("/v1/agents/{a}"), None, None).await;
    assert_eq!(agent["state"], "ended", "{agent}");
}

#[tokio::test]
async fn registering_an_agent_that_is_not_running_announces_it_running() {
    let t = T::new();
    let a = "t3:alpha";
    t.register(a, Some(REPO), None).await;
    let path = format!("/v1/agents/{a}/state");
    let changes = "SELECT COUNT(*) FROM events WHERE type = 'agent_state_changed'";
    let last = "SELECT json_extract(payload, '$.from') || '>' || json_extract(payload, '$.to') \
                FROM events WHERE type = 'agent_state_changed' ORDER BY seq DESC LIMIT 1";
    // Registering a running agent again is no state change.
    t.register(a, Some(REPO), None).await;
    assert_eq!(t.count(changes), 0);

    // A T3 thread resumed after its idle session was closed registers on session start:
    // running again, announced once.
    t.ok("POST", &path, Some(a), Some(json!({ "state": "ended" })))
        .await;
    t.register(a, Some(REPO), None).await;
    t.register(a, Some(REPO), None).await;
    let agent = t.ok("GET", &format!("/v1/agents/{a}"), None, None).await;
    assert_eq!(agent["state"], "running", "{agent}");
    assert_eq!(t.count(changes), 2);
    assert_eq!(t.strings(last), ["ended>running"]);

    let until = (t.state.now() + TimeDelta::hours(6)).to_rfc3339();
    t.ok(
        "POST",
        &path,
        Some(a),
        Some(json!({ "state": "paused", "paused_until": until })),
    )
    .await;
    t.register(a, Some(REPO), None).await;
    assert_eq!(t.count(changes), 4);
    assert_eq!(t.strings(last), ["paused>running"]);
}

#[tokio::test]
async fn a_usage_limit_outlasts_the_session_that_stopped_on_it() {
    let t = T::new();
    let a = "t3:limited";
    t.register(a, Some(REPO), None).await;
    let path = format!("/v1/agents/{a}/state");
    let agent = format!("/v1/agents/{a}");
    let last_until = "SELECT json_extract(payload, '$.to') || ' ' || \
                      COALESCE(json_extract(payload, '$.paused_until'), '-') \
                      FROM events WHERE type = 'agent_state_changed' ORDER BY seq DESC LIMIT 1";
    let reset = t.state.now() + TimeDelta::hours(6);
    t.ok(
        "POST",
        &path,
        Some(a),
        Some(json!({ "state": "paused", "paused_until": reset.to_rfc3339() })),
    )
    .await;

    // T3 closes the idle session: ended, still waiting for the reset, and announced so.
    t.advance(TimeDelta::minutes(30));
    t.ok("POST", &path, Some(a), Some(json!({ "state": "ended" })))
        .await;
    let ended = t.ok("GET", &agent, None, None).await;
    assert_eq!(ended["state"], "ended", "{ended}");
    let kept: Timestamp = serde_json::from_value(ended["paused_until"].clone()).unwrap();
    assert_eq!(kept, reset);
    let row = t.strings(last_until);
    let (to, until) = row[0].split_once(' ').unwrap();
    assert_eq!(to, "ended");
    assert_eq!(
        serde_json::from_value::<Timestamp>(json!(until)).unwrap(),
        reset
    );

    // A second session end keeps it too; once the reset is past, an end clears it.
    t.ok("POST", &path, Some(a), Some(json!({ "state": "ended" })))
        .await;
    let again = t.ok("GET", &agent, None, None).await;
    assert_eq!(again["paused_until"], ended["paused_until"], "{again}");
    t.advance(TimeDelta::hours(6));
    t.ok("POST", &path, Some(a), Some(json!({ "state": "ended" })))
        .await;
    let past = t.ok("GET", &agent, None, None).await;
    assert!(past["paused_until"].is_null(), "{past}");

    // Resumed: a session start clears the reset.
    t.ok(
        "POST",
        &path,
        Some(a),
        Some(json!({ "state": "paused", "paused_until": (t.state.now() + TimeDelta::hours(1)).to_rfc3339() })),
    )
    .await;
    t.ok("POST", &path, Some(a), Some(json!({ "state": "ended" })))
        .await;
    t.register(a, Some(REPO), None).await;
    let resumed = t.ok("GET", &agent, None, None).await;
    assert_eq!(resumed["state"], "running", "{resumed}");
    assert!(resumed["paused_until"].is_null(), "{resumed}");
}

#[tokio::test]
async fn task_detail_names_each_agent_it_mentions_by_handle() {
    let t = T::new();
    let long = "t3:thread:delegated-task:command-abc";
    let registered = t.register(long, Some(REPO), None).await;
    t.board("factory", &[REPO]).await;
    let task = t.task(long, "factory", "Delegated work").await;
    t.claim(long, &task).await;
    t.ok(
        "POST",
        &format!("/v1/tasks/{task}/notes"),
        Some(long),
        Some(json!({ "agent": long, "kind": "note", "text": "note: started" })),
    )
    .await;
    let detail = t.ok("GET", &format!("/v1/tasks/{task}"), None, None).await;
    assert_eq!(
        detail["handles"],
        json!({ long: registered["handle"] }),
        "{detail}"
    );
    // A handle copied from task_show works where an agent id is expected.
    let handle = registered["handle"].as_str().unwrap();
    let listed = t
        .ok("GET", &format!("/v1/tasks?owner={handle}"), None, None)
        .await;
    assert_eq!(listed[0]["id"], task.as_str(), "{listed}");
    let peer = "t3:peer";
    let peer_handle = t.register(peer, Some(REPO), None).await["handle"].clone();
    let (status, v) = t.patch(long, &task, json!({ "owner": peer_handle })).await;
    assert!(status.is_success(), "{v}");
    assert_eq!(v["owner"], peer, "{v}");
}
