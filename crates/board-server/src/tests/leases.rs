use chrono::TimeDelta;
use serde_json::{Value, json};

use super::{REPO, T, check, iso, lease_ttl, stale_after, start, touch_window};
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

async fn lease_for(t: &T, agent: &str, task: &str, globs: &[&str]) -> Value {
    t.ok(
        "POST",
        "/v1/leases",
        Some(agent),
        Some(json!({
            "agent": agent, "repo": REPO, "globs": globs, "task": task,
        })),
    )
    .await
}

/// The `lease_released` events that name `task`.
async fn released(t: &T, task: &str) -> Vec<Value> {
    let page = t
        .ok("GET", "/v1/events?since=0&limit=1000", None, None)
        .await;
    page["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "lease_released" && e["task"] == task)
        .cloned()
        .collect()
}

/// The holders of the `conflicts` in a lease answer, sorted.
fn holders(answer: &Value) -> Vec<String> {
    let mut out: Vec<String> = answer["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["agent"].as_str().unwrap().to_owned())
        .collect();
    out.sort();
    out
}

fn sweep(t: &T) -> usize {
    t.state
        .db()
        .write(|| t.state.now(), t.state.settings(), sweep_expired)
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
    const GRANDCHILD: &str = "t3:thread:delegated-task:command%3Amcp%3Ac3%3Adelegate-task%3At3";
    let t = T::new();
    t.board("factory", &[REPO]).await;
    t.register(PARENT, Some(REPO), Some("main")).await;
    for (child, parent) in [(CHILD, PARENT), (SIBLING, PARENT), (GRANDCHILD, CHILD)] {
        t.ok(
            "POST",
            "/v1/agents",
            Some(child),
            Some(json!({
                "id": child, "runtime": "claude", "device": "mac", "repo": REPO,
                "branch": "main",
                "native_ids": {"t3_thread": &child[3..], "t3_parent": &parent[3..]},
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

    // The child's own child is family to both, all the way up.
    alerts(&t, GRANDCHILD).await;
    assert_eq!(
        touch(&t, GRANDCHILD, &["src/lib.rs"]).await["overlaps"],
        json!([])
    );
    assert_eq!(
        lease(&t, GRANDCHILD, &["src/lib.rs"]).await["conflicts"],
        json!([])
    );
    for agent in [PARENT, CHILD, GRANDCHILD] {
        let text = alerts(&t, agent).await;
        assert!(!text.contains("! "), "{agent}: {text}");
    }

    // Two children of one parent are peers: their overlaps are real, down their lines.
    let overlaps = touch(&t, SIBLING, &["src/lib.rs"]).await["overlaps"].clone();
    assert_eq!(agents_in(&overlaps), [CHILD, GRANDCHILD], "{overlaps}");
    let child_alerts = alerts(&t, CHILD).await;
    assert!(child_alerts.contains("edited src/lib.rs"), "{child_alerts}");
    // And any other thread is a peer of the parent.
    let parent_alerts = alerts(&t, PARENT).await;
    assert!(!parent_alerts.contains("! "), "{parent_alerts}");
    t.register(B, Some(REPO), Some("main")).await;
    let overlaps = touch(&t, B, &["src/lib.rs"]).await["overlaps"].clone();
    assert_eq!(
        agents_in(&overlaps),
        [PARENT, CHILD, SIBLING, GRANDCHILD],
        "{overlaps}"
    );
    let parent_alerts = alerts(&t, PARENT).await;
    assert!(
        parent_alerts.contains("edited src/lib.rs"),
        "{parent_alerts}"
    );
}

#[tokio::test]
async fn one_write_found_by_two_peers_shell_scans_is_neither_ones_overlap() {
    let t = T::new();
    const C: &str = "t3:gamma";
    for agent in [A, B, C] {
        t.register(agent, Some(REPO), None).await;
    }
    let scan = |agent: &'static str, path: &'static str, modified: &'static str| {
        let t = &t;
        let body = json!({
            "agent": agent, "repo": REPO, "paths": [path], "scanned": true,
            "modified": { path: modified },
        });
        async move { t.ok("POST", "/v1/touches", Some(agent), Some(body)).await }
    };
    let written = "2026-11-06T12:00:00.250Z";
    // C leases the area, so it hears about every edit there.
    lease(&t, C, &["src/**"]).await;
    for agent in [A, B, C] {
        alerts(&t, agent).await;
    }

    // A's script writes src/a.rs during B's read-only shell call: both scans find the same
    // write. Whichever report comes first, neither is warned about the other.
    let first = scan(A, "src/a.rs", written).await;
    let second = scan(B, "src/a.rs", written).await;
    assert_eq!(agents_in(&first["overlaps"]), vec![C], "{first}");
    assert_eq!(agents_in(&second["overlaps"]), vec![C], "{second}");
    // A third peer still hears of the write, from both scans.
    let text = alerts(&t, C).await;
    assert_eq!(text.matches(" edited src/a.rs").count(), 2, "{text}");
    let first = scan(B, "src/b.rs", written).await;
    let second = scan(A, "src/b.rs", written).await;
    assert_eq!(agents_in(&first["overlaps"]), vec![C], "{first}");
    assert_eq!(agents_in(&second["overlaps"]), vec![C], "{second}");
    for agent in [A, B] {
        let text = alerts(&t, agent).await;
        assert!(!text.contains("edited"), "{agent}: {text}");
    }

    // A shell write of A's alone, with no peer scan finding it, is still A's: B's later
    // write of the file overlaps it, and B's alerts say so.
    scan(A, "src/c.rs", written).await;
    let later = scan(B, "src/c.rs", "2026-11-06T12:00:09Z").await;
    assert_eq!(agents_in(&later["overlaps"]), vec![A, C], "{later}");
    let text = alerts(&t, B).await;
    assert!(
        text.contains(" edited src/c.rs <1m ago; you edited it"),
        "{text}"
    );
    let text = alerts(&t, A).await;
    assert!(
        text.contains(" edited src/c.rs <1m ago; you edited it"),
        "{text}"
    );

    // A peer on another device cannot share the work tree: an equal time is a real overlap.
    t.exec("UPDATE agents SET device = 'pc' WHERE id = 't3:beta'");
    scan(A, "src/d.rs", written).await;
    let far = scan(B, "src/d.rs", written).await;
    assert_eq!(agents_in(&far["overlaps"]), vec![A, C], "{far}");
}

#[tokio::test]
async fn a_task_lease_outside_its_boards_repos_warns_and_stays_put() {
    const OTHER: &str = "github.com/acme/dotfiles";
    const SECOND: &str = "github.com/acme/site";
    let t = T::new();
    t.register(A, Some(OTHER), None).await;
    t.board("factory", &[REPO]).await;
    t.board("pair", &[REPO, SECOND]).await;
    t.board("loose", &[]).await;
    let one = t.task(A, "factory", "One repo").await;
    let two = t.task(A, "pair", "Two repos").await;
    let free = t.task(A, "loose", "No repos").await;
    let lease = |repo: &str, task: Option<&str>| json!({ "agent": A, "repo": repo, "globs": ["src/**"], "task": task });

    // From a checkout of a repo the task's board does not list, the lease is still the
    // caller's, in the repo it named, and the answer warns, naming the board's repos.
    for (task, listed) in [(&one, vec![REPO]), (&two, vec![REPO, SECOND])] {
        let v = t
            .ok(
                "POST",
                "/v1/leases",
                Some(A),
                Some(lease(OTHER, Some(task))),
            )
            .await;
        assert_eq!(v["leases"][0]["repo"], OTHER, "{v}");
        let warning = v["warning"].as_str().unwrap();
        assert!(
            warning.contains(task.as_str()) && warning.contains(OTHER),
            "{v}"
        );
        for repo in listed {
            assert!(warning.contains(repo), "{v}");
        }
    }
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM leases WHERE repo = '{OTHER}'"
        )),
        1
    );

    // A listed repo, a board without repos, and no task: no warning.
    for body in [
        lease(SECOND, Some(&two)),
        lease(OTHER, Some(&free)),
        lease(OTHER, None),
    ] {
        // Each starts from no lease, so re-leasing src/** for another task does not warn.
        t.exec("DELETE FROM leases");
        let v = t.ok("POST", "/v1/leases", Some(A), Some(body)).await;
        assert!(v.get("warning").is_none(), "{v}");
    }
}

#[tokio::test]
async fn leases_end_with_their_task() {
    const C: &str = "t3:gamma";
    const D: &str = "t3:delta";
    const E: &str = "t3:epsilon";
    let t = T::new();
    t.board("app", &[REPO]).await;
    for agent in [A, B, C, D, E] {
        t.register(agent, Some(REPO), None).await;
    }
    let finished = t.task(A, "app", "Finished").await;
    let dropped = t.task(C, "app", "Dropped").await;
    let working = t.task(D, "app", "Working").await;
    let parked = t.task(E, "app", "Parked").await;
    for (agent, task) in [(A, &finished), (C, &dropped), (D, &working), (E, &parked)] {
        assert!(t.claim(agent, task).await.0.is_success());
    }
    lease_for(&t, A, &finished, &["src/done.rs"]).await;
    // A lease naming the task by another agent, such as its child, ends with it too.
    lease_for(&t, B, &finished, &["src/child.rs"]).await;
    lease(&t, A, &["src/untasked.rs"]).await;
    lease_for(&t, C, &dropped, &["src/dropped.rs"]).await;
    lease_for(&t, D, &working, &["src/working.rs"]).await;
    lease_for(&t, E, &parked, &["src/parked.rs"]).await;

    // Done by its owner: both leases naming it are released, each with an event.
    assert!(t.done(A, &finished, check(0)).await.0.is_success());
    let events = released(&t, &finished).await;
    let mut seen: Vec<(&str, &str, &str)> = events
        .iter()
        .map(|e| {
            (
                e["actor"].as_str().unwrap(),
                e["payload"]["agent"].as_str().unwrap(),
                e["payload"]["glob"].as_str().unwrap(),
            )
        })
        .collect();
    seen.sort();
    assert_eq!(seen, [(A, A, "src/done.rs"), (A, B, "src/child.rs")]);
    assert_eq!(
        t.count(&format!(
            "SELECT COUNT(*) FROM leases WHERE task = '{finished}'"
        )),
        0
    );

    // Dropped by the user (acting as B), not its owner: its lease ends too.
    let version = t.get_task(&dropped).await["version"].clone();
    let bearer = format!("Bearer {}", t.user_token);
    let (s, v) = t
        .send(
            "PATCH",
            &format!("/v1/tasks/{dropped}"),
            &[("authorization", bearer.as_str()), ("x-board-agent", B)],
            Some(json!({ "agent": B, "version": version, "status": "dropped" })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let events = released(&t, &dropped).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["actor"], B);
    assert_eq!(events[0]["payload"]["agent"], C);
    assert_eq!(events[0]["payload"]["repo"], REPO);

    // Moved to review and back, or to blocked: its owner's work may resume, so its leases stay.
    assert!(
        t.patch(D, &working, json!({ "status": "review" }))
            .await
            .0
            .is_success()
    );
    assert!(
        t.patch(D, &working, json!({ "status": "doing" }))
            .await
            .0
            .is_success()
    );
    assert!(
        t.patch(
            E,
            &parked,
            json!({ "status": "blocked", "blocked_reason": "waiting" })
        )
        .await
        .0
        .is_success()
    );
    assert!(released(&t, &working).await.is_empty());
    assert!(released(&t, &parked).await.is_empty());

    // Another agent leasing every path is warned only by leases of work still going: the
    // doing task's, the blocked task's and the lease that names no task.
    let answer = lease(
        &t,
        B,
        &[
            "src/done.rs",
            "src/dropped.rs",
            "src/working.rs",
            "src/parked.rs",
            "src/untasked.rs",
        ],
    )
    .await;
    assert_eq!(holders(&answer), [A, D, E]);
    let globs: Vec<&str> = answer["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["glob"].as_str().unwrap())
        .collect();
    assert_eq!(
        globs,
        ["src/untasked.rs", "src/working.rs", "src/parked.rs"]
    );

    // A touch of the closed tasks' paths reports only B's new leases, none of the ended ones.
    let touched = t
        .ok(
            "POST",
            "/v1/touches",
            Some(C),
            Some(json!({
                "agent": C, "repo": REPO, "paths": ["src/done.rs", "src/dropped.rs"],
            })),
        )
        .await;
    let leased: Vec<&str> = touched["overlaps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["via"] == "lease")
        .map(|o| o["agent"].as_str().unwrap())
        .collect();
    assert_eq!(leased, [B, B]);
}

#[tokio::test]
async fn resolving_a_feedback_item_ends_its_leases() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    t.register(B, Some(REPO), None).await;
    let item = t
        .ok(
            "POST",
            "/v1/feedback",
            Some(A),
            Some(json!({ "about": "board", "text": "leases outlive tasks" })),
        )
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(t.claim(A, &item).await.0.is_success());
    lease_for(&t, A, &item, &["src/fix.rs"]).await;
    let (s, _) = t
        .call(
            "POST",
            &format!("/v1/feedback/{item}/resolve"),
            Some(A),
            Some(json!({ "kind": "declined", "reason": "works as designed" })),
        )
        .await;
    assert!(s.is_success());
    assert_eq!(released(&t, &item).await.len(), 1);
    assert!(holders(&lease(&t, B, &["src/fix.rs"]).await).is_empty());
}

#[tokio::test]
async fn leases_end_on_release_review_done_and_unowned_drop_but_not_for_another_task() {
    const C: &str = "t3:gamma";
    const D: &str = "t3:delta";
    let t = T::new();
    t.board("app", &[REPO]).await;
    for agent in [A, B, C, D] {
        t.register(agent, Some(REPO), None).await;
    }
    let lease_ids = |answer: &Value| -> Vec<Value> {
        answer["leases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["task"].clone())
            .collect()
    };

    // Done from review.
    let reviewed = t.task(A, "app", "Reviewed").await;
    assert!(t.claim(A, &reviewed).await.0.is_success());
    lease_for(&t, A, &reviewed, &["src/reviewed.rs"]).await;
    assert!(
        t.patch(A, &reviewed, json!({ "status": "review" }))
            .await
            .0
            .is_success()
    );
    assert!(released(&t, &reviewed).await.is_empty());
    assert!(t.done(A, &reviewed, check(0)).await.0.is_success());
    let events = released(&t, &reviewed).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["board"], "app");

    // Released to open: the next owner is not warned about its own task's files.
    let handed = t.task(B, "app", "Handed back").await;
    assert!(t.claim(B, &handed).await.0.is_success());
    lease_for(&t, B, &handed, &["src/handed.rs"]).await;
    let (s, v) = t
        .call(
            "POST",
            &format!("/v1/tasks/{handed}/release"),
            Some(B),
            Some(json!({ "agent": B })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(released(&t, &handed).await.len(), 1);
    assert!(t.claim(C, &handed).await.0.is_success());
    assert!(holders(&lease_for(&t, C, &handed, &["src/handed.rs"]).await).is_empty());

    // An unowned open task leased by one agent and dropped by another.
    let idea = t.task(D, "app", "Idea").await;
    lease_for(&t, D, &idea, &["src/idea.rs"]).await;
    assert!(
        t.patch(A, &idea, json!({ "status": "dropped" }))
            .await
            .0
            .is_success()
    );
    let events = released(&t, &idea).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        (
            events[0]["actor"].as_str(),
            events[0]["payload"]["agent"].as_str()
        ),
        (Some(A), Some(D))
    );

    // A lease for a task already closed names no task, and says so.
    let late = lease_for(&t, D, &idea, &["src/late.rs"]).await;
    assert_eq!(lease_ids(&late), [Value::Null]);
    assert!(
        late["warning"].as_str().unwrap().contains("is dropped"),
        "{late}"
    );

    // One glob leased for two pieces of work names neither, so closing one keeps it.
    let first = t.task(A, "app", "First").await;
    assert!(t.claim(A, &first).await.0.is_success());
    let plain = lease(&t, A, &["src/shared.rs"]).await;
    assert_eq!(lease_ids(&plain), [Value::Null]);
    let again = lease_for(&t, A, &first, &["src/shared.rs", "src/first.rs"]).await;
    assert_eq!(lease_ids(&again), [Value::Null, json!(first)]);
    assert!(
        again["warning"].as_str().unwrap().contains("src/shared.rs"),
        "{again}"
    );
    // Leasing it for the same task again is a plain renewal, without a warning.
    let renewed = lease_for(&t, A, &first, &["src/first.rs"]).await;
    assert_eq!(lease_ids(&renewed), [json!(first)]);
    assert!(renewed.get("warning").is_none(), "{renewed}");
    assert!(t.done(A, &first, check(0)).await.0.is_success());
    let globs: Vec<String> = released(&t, &first)
        .await
        .iter()
        .map(|e| e["payload"]["glob"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(globs, ["src/first.rs"]);
    assert_eq!(holders(&lease(&t, B, &["src/shared.rs"]).await), [A]);
}

#[tokio::test]
async fn a_feedback_item_resolved_by_the_user_ends_its_leases() {
    let t = T::new();
    t.register(A, Some(REPO), None).await;
    let item = t
        .ok(
            "POST",
            "/v1/feedback",
            Some(A),
            Some(json!({ "about": "board", "text": "leases outlive tasks" })),
        )
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(t.claim(A, &item).await.0.is_success());
    lease_for(&t, A, &item, &["src/fix.rs"]).await;
    let (s, v) = t
        .as_user(
            "POST",
            &format!("/v1/feedback/{item}/resolve"),
            Some(json!({ "kind": "declined", "reason": "works as designed" })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let events = released(&t, &item).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        (
            events[0]["actor"].as_str(),
            events[0]["payload"]["agent"].as_str()
        ),
        (Some("user"), Some(A))
    );
}
