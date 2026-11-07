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
