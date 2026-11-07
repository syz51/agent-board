use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{REPO, T, iso, lease_ttl, stale_after, start, touch_window};
use crate::ops::leases::sweep_expired;

const A: &str = "t3:alpha";
const B: &str = "t3:beta";

async fn lease(t: &T, agent: &str, globs: &[&str]) -> Value {
    t.ok(
        "POST",
        "/v1/leases",
        Some(agent),
        Some(json!({
            "agent": agent, "repo": REPO, "globs": globs,
        })),
    )
    .await
}

fn sweep(t: &T) -> usize {
    t.state
        .db()
        .write(t.state.now(), t.state.settings(), sweep_expired)
        .unwrap()
}

#[tokio::test]
async fn overlapping_leases_report_live_peers() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    t.register(B, Some(REPO), None).await;
    let first = lease(&t, A, &["crates/board-server/**"]).await;
    assert_eq!(first["conflicts"], json!([]));
    assert_eq!(first["leases"][0]["expires_at"], iso(start() + lease_ttl()));

    let second = lease(&t, B, &["crates/board-server/src/*.rs", "web/**"]).await;
    assert_eq!(second["leases"].as_array().unwrap().len(), 2);
    let conflicts = second["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["agent"], A);
    // Leases are advisory: B holds its leases anyway.
    assert_eq!(
        t.count("SELECT COUNT(*) FROM leases WHERE agent = 't3:beta'"),
        2
    );

    let disjoint = lease(&t, B, &["docs/**"]).await;
    assert_eq!(disjoint["conflicts"], json!([]));

    // A stale holder does not conflict.
    t.advance(stale_after() + TimeDelta::hours(1));
    t.register(B, Some(REPO), None).await;
    t.exec(&format!(
        "UPDATE leases SET expires_at = expires_at + {}",
        (stale_after() * 2).num_milliseconds()
    ));
    let later = lease(&t, B, &["crates/board-server/src/lib.rs"]).await;
    assert_eq!(later["conflicts"], json!([]));
}

#[tokio::test]
async fn leases_renew_expire_and_release() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    t.register(B, Some(REPO), None).await;
    let id = lease(&t, A, &["src/**"]).await["leases"][0]["id"]
        .as_i64()
        .unwrap();
    let half = TimeDelta::seconds(lease_ttl().num_seconds() / 2);
    t.advance(half);
    // Re-leasing the same glob renews the same lease.
    let renewed = lease(&t, A, &["src/**"]).await;
    assert_eq!(renewed["leases"][0]["id"], id);
    assert_eq!(
        renewed["leases"][0]["expires_at"],
        iso(start() + half + lease_ttl())
    );
    // A touch in the repo renews it too.
    t.advance(half);
    t.ok(
        "POST",
        "/v1/touches",
        Some(A),
        Some(json!({
            "agent": A, "repo": REPO, "paths": ["src/lib.rs"],
        })),
    )
    .await;
    assert_eq!(sweep(&t), 0);
    t.advance(lease_ttl() - TimeDelta::minutes(1));
    assert_eq!(sweep(&t), 0);
    assert_eq!(
        lease(&t, B, &["src/main.rs"]).await["conflicts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    t.advance(TimeDelta::minutes(1));
    assert_eq!(sweep(&t), 1);
    assert_eq!(
        t.count("SELECT COUNT(*) FROM leases WHERE agent = 't3:alpha'"),
        0
    );
    let expired = t
        .ok("GET", "/v1/events?since=0&limit=1000", None, None)
        .await;
    let expired: Vec<_> = expired["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "lease_expired")
        .collect();
    assert_eq!(expired.len(), 1);
    assert_eq!(
        (
            expired[0]["actor"].as_str(),
            expired[0]["payload"]["agent"].as_str()
        ),
        (Some("server"), Some(A))
    );

    let mine = lease(&t, A, &["docs/*.md"]).await["leases"][0]["id"]
        .as_i64()
        .unwrap();
    let path = format!("/v1/leases/{mine}");
    let (s, _) = t.call("DELETE", &path, Some(B), None).await;
    assert_eq!(s.as_u16(), 403);
    let (s, _) = t.call("DELETE", &path, Some(A), None).await;
    assert_eq!(s.as_u16(), 204);
    let (s, _) = t.call("DELETE", &path, Some(A), None).await;
    assert_eq!(s.as_u16(), 404);

    let (s, _) = t
        .call(
            "POST",
            "/v1/leases",
            Some(A),
            Some(json!({
                "agent": A, "repo": REPO, "globs": ["src/**"], "ttl_secs": 600,
            })),
        )
        .await;
    assert!(s.is_success());
    for bad in [
        json!([]),
        json!(["../etc/**"]),
        json!(["/abs/**"]),
        json!(["src/[x"]),
    ] {
        let (status, code, _) = t
            .fail(
                "POST",
                "/v1/leases",
                Some(A),
                Some(json!({
                    "agent": A, "repo": REPO, "globs": bad,
                })),
            )
            .await;
        assert_eq!((status, code.as_str()), (400, "invalid"), "{bad}");
    }
}

#[tokio::test]
async fn touches_report_peer_leases_and_recent_touches() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    t.register(B, Some(REPO), None).await;
    let touch = |agent: &'static str, paths: &'static [&'static str]| {
        let t = &t;
        async move {
            t.ok(
                "POST",
                "/v1/touches",
                Some(agent),
                Some(json!({
                    "agent": agent, "repo": REPO, "paths": paths,
                })),
            )
            .await
        }
    };
    assert_eq!(touch(A, &["src/parse.rs"]).await["overlaps"], json!([]));
    lease(&t, A, &["src/*.rs"]).await;
    t.advance(TimeDelta::minutes(5));
    let overlaps = touch(B, &["src/parse.rs", "README.md"]).await["overlaps"].clone();
    let overlaps = overlaps.as_array().unwrap();
    assert_eq!(overlaps.len(), 2, "{overlaps:?}");
    assert_eq!(
        (overlaps[0]["via"].as_str(), overlaps[0]["path"].as_str()),
        (Some("lease"), Some("src/parse.rs"))
    );
    assert_eq!(overlaps[1]["via"], "touch");
    assert_eq!(overlaps[1]["at"], "2026-11-06T12:00:00Z");
    assert!(overlaps[0]["handle"].as_str().unwrap().starts_with("t3:"));

    // Once the touch is older than the window and A's lease has expired, nothing overlaps.
    t.advance(touch_window().max(lease_ttl()) + TimeDelta::minutes(1));
    let overlaps = touch(B, &["src/parse.rs"]).await["overlaps"].clone();
    assert_eq!(
        overlaps,
        json!([]),
        "A's lease expired and its touch is old"
    );
    let (status, _, _) = t
        .fail(
            "POST",
            "/v1/touches",
            Some(A),
            Some(json!({
                "agent": A, "repo": REPO, "paths": ["../outside"],
            })),
        )
        .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn repeated_globs_are_leased_once() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    let result = lease(&t, A, &["src/**", "docs/**", "src/**"]).await;
    let globs: Vec<&str> = result["leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["glob"].as_str().unwrap())
        .collect();
    assert_eq!(globs, ["src/**", "docs/**"]);
    assert_eq!(t.count("SELECT count(*) FROM leases"), 2);
}

#[tokio::test]
async fn a_peers_edit_found_by_a_shell_scan_is_not_the_scanners() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    t.register(B, Some(REPO), None).await;
    let send = |agent: &'static str, path: &'static str, scanned: bool, modified: &str| {
        let t = &t;
        let body = json!({
            "agent": agent, "repo": REPO, "paths": [path], "scanned": scanned,
            "modified": { path: modified },
        });
        async move { t.ok("POST", "/v1/touches", Some(agent), Some(body)).await }
    };
    let paths = |t: &T, agent: &str| {
        t.count(&format!(
            "SELECT COUNT(*) FROM touches WHERE agent = '{agent}'"
        ))
    };
    let edited = "2026-11-06T12:00:00.250Z";

    // B's shell scan reports first, then A's Edit of the same write: A is not warned
    // about B, and B's scanned touch is gone.
    send(B, "hooks/shell.js", true, edited).await;
    let result = send(A, "hooks/shell.js", false, edited).await;
    assert_eq!(result["overlaps"], json!([]), "{result}");
    assert_eq!(paths(&t, B), 0);

    // A's Edit reports first, then B's scan finds the same write: not recorded for B, and
    // B learns A touched it.
    send(A, "hooks/run.js", false, edited).await;
    let since = t.count("SELECT MAX(seq) FROM events");
    let result = send(B, "hooks/run.js", true, edited).await;
    assert_eq!(result["overlaps"][0]["agent"], A, "{result}");
    assert_eq!(paths(&t, B), 0);
    assert_eq!(
        t.count("SELECT MAX(seq) FROM events"),
        since,
        "no files_touched"
    );

    // A file B's command wrote itself has its own time: recorded for B, and A's later edit
    // is a real overlap.
    send(B, "hooks/build.js", true, "2026-11-06T12:00:01Z").await;
    assert_eq!(paths(&t, B), 1);
    let result = send(A, "hooks/build.js", false, "2026-11-06T12:00:05Z").await;
    assert_eq!(result["overlaps"][0]["agent"], B, "{result}");

    // A's alerts: B's real write of build.js, not B's scan of A's shell.js edit, whose
    // files_touched event came before A's touch removed it.
    let alerts = t.ok("GET", "/v1/snapshot?kind=alerts", Some(A), None).await;
    let alerts = alerts["text"].as_str().unwrap();
    assert!(alerts.contains("edited hooks/build.js"), "{alerts}");
    assert!(!alerts.contains("hooks/shell.js"), "{alerts}");

    // A scan never replaces the scanner's own earlier edit-tool touch, so a peer's matching
    // write cannot remove it.
    send(B, "src/own.rs", false, "2026-11-06T11:00:00Z").await;
    send(B, "src/own.rs", true, edited).await;
    send(A, "src/own.rs", false, edited).await;
    assert_eq!(
        t.count("SELECT COUNT(*) FROM touches WHERE agent = 't3:beta' AND path = 'src/own.rs'"),
        1
    );

    // A peer on another device cannot share the work tree: its equal time proves nothing.
    const C: &str = "t3:gamma";
    t.register(C, Some(REPO), None).await;
    send(C, "src/far.rs", true, edited).await;
    t.exec("UPDATE agents SET device = 'pc' WHERE id = 't3:gamma'");
    let result = send(A, "src/far.rs", false, edited).await;
    assert_eq!(result["overlaps"][0]["agent"], C, "{result}");

    // Times only for paths in the request.
    let (status, _, _) = t
        .fail(
            "POST",
            "/v1/touches",
            Some(A),
            Some(json!({
                "agent": A, "repo": REPO, "paths": ["a.rs"], "modified": { "b.rs": edited },
            })),
        )
        .await;
    assert_eq!(status, 400);
}

async fn touch(t: &T, agent: &str, paths: &[&str]) -> Value {
    t.ok(
        "POST",
        "/v1/touches",
        Some(agent),
        Some(json!({"agent": agent, "repo": REPO, "paths": paths})),
    )
    .await
}

fn agents_in(overlaps: &Value) -> Vec<String> {
    let mut agents: Vec<String> = overlaps
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["agent"].as_str().unwrap().to_owned())
        .collect();
    agents.sort();
    agents.dedup();
    agents
}

async fn alerts(t: &T, agent: &str) -> String {
    let snap = t
        .ok(
            "GET",
            &format!(
                "/v1/snapshot?agent={}&kind=alerts",
                agent.replace('%', "%25")
            ),
            Some(agent),
            None,
        )
        .await;
    snap["text"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_delegated_child_and_its_parent_never_overlap_each_other() {
    const PARENT: &str = "t3:b8745db9-8b9a-4047-8e1d-15b7ac1b2f40";
    const CHILD: &str = "t3:thread:delegated-task:command%3Amcp%3Ac1%3Adelegate-task%3At1";
    const SIBLING: &str = "t3:thread:delegated-task:command%3Amcp%3Ac2%3Adelegate-task%3At2";
    let t = T::new();
    t.board("factory", &[REPO]).await;
    t.register(PARENT, Some(REPO), Some("main")).await;
    for child in [CHILD, SIBLING] {
        t.ok(
            "POST",
            "/v1/agents",
            Some(child),
            Some(json!({
                "id": child, "runtime": "claude", "device": "mac", "repo": REPO,
                "branch": "main",
                "native_ids": {"t3_thread": &child[3..], "t3_parent": &PARENT[3..]},
            })),
        )
        .await;
    }
    // A re-registration without it keeps the parent: native ids are merged.
    t.register(CHILD, Some(REPO), Some("main")).await;
    alerts(&t, PARENT).await;
    alerts(&t, CHILD).await;

    // The parent prepared the files and leased them before delegating.
    touch(&t, PARENT, &["src/lib.rs"]).await;
    lease(&t, PARENT, &["src/**"]).await;

    // Its child edits and leases them without a warning either way.
    assert_eq!(
        touch(&t, CHILD, &["src/lib.rs"]).await["overlaps"],
        json!([])
    );
    assert_eq!(
        lease(&t, CHILD, &["src/*.rs"]).await["conflicts"],
        json!([])
    );
    let child_alerts = alerts(&t, CHILD).await;
    assert!(!child_alerts.contains("! "), "{child_alerts}");
    let parent_alerts = alerts(&t, PARENT).await;
    assert!(!parent_alerts.contains("! "), "{parent_alerts}");

    // Two children of one parent are peers: their overlaps are real.
    let overlaps = touch(&t, SIBLING, &["src/lib.rs"]).await["overlaps"].clone();
    assert_eq!(agents_in(&overlaps), [CHILD], "{overlaps}");
    let child_alerts = alerts(&t, CHILD).await;
    assert!(child_alerts.contains("edited src/lib.rs"), "{child_alerts}");
    // And any other thread is a peer of the parent.
    let parent_alerts = alerts(&t, PARENT).await;
    assert!(!parent_alerts.contains("! "), "{parent_alerts}");
    t.register(B, Some(REPO), Some("main")).await;
    let overlaps = touch(&t, B, &["src/lib.rs"]).await["overlaps"].clone();
    assert_eq!(agents_in(&overlaps), [PARENT, CHILD, SIBLING], "{overlaps}");
    let parent_alerts = alerts(&t, PARENT).await;
    assert!(
        parent_alerts.contains("edited src/lib.rs"),
        "{parent_alerts}"
    );
}
