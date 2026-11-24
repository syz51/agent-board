//! Device capability registry, plane governance auth boundaries, injectable-clock staleness,
//! `X-Board-Features` header, and task routing fields.

use axum::body::Body;
use axum::http::Request;
use chrono::TimeDelta;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::{REPO, T, USER};

const ME: &str = "t3:me";

#[tokio::test]
async fn devices_enforce_admin_vs_self_auth_and_compute_status_with_clock() {
    let t = T::new();

    // Create a second device (`linux-runner`) as user.
    let created = t
        .as_user(
            "POST",
            "/v1/admin/devices",
            Some(json!({ "name": "linux-runner" })),
        )
        .await
        .1;
    let linux_token = created["token"].as_str().unwrap().to_owned();
    let linux_bearer = format!("Bearer {linux_token}");

    // Before any heartbeat, active devices are `offline` with default `runner` role.
    let list = t.ok("GET", "/v1/devices", None, None).await;
    let entries = list.as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["linux-runner", "mac", "phone"]);
    for d in entries {
        assert_eq!(d["role"], "runner");
        assert_eq!(d["status"], "offline");
        assert_eq!(d["drain"], false);
        assert_eq!(d["preference"], 0);
        assert_eq!(d["taints"], json!([]));
    }

    // Device token (`mac`) cannot call PATCH /v1/admin/devices/{name} — even on itself.
    let (code, err, v) = t
        .fail(
            "PATCH",
            "/v1/admin/devices/mac",
            None,
            Some(json!({ "role": "control", "taints": [], "preference": 50 })),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // User sets control plane role + NoSchedule taint + preference + drain via admin PATCH.
    let (s, updated) = t
        .as_user(
            "PATCH",
            "/v1/admin/devices/mac",
            Some(json!({
                "role": "control",
                "taints": [
                    { "key": "role", "value": "control", "effect": "no_schedule" }
                ],
                "preference": -10,
                "drain": true
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{updated}");
    assert_eq!(updated["role"], "control");
    assert_eq!(updated["preference"], -10);
    assert_eq!(updated["drain"], true);
    assert_eq!(
        updated["taints"],
        json!([{ "key": "role", "value": "control", "effect": "no_schedule" }])
    );

    // Trail Rule: admin update wrote a durable `device_updated` event.
    let events = t
        .ok("GET", "/v1/events?since=0&limit=100", None, None)
        .await["events"]
        .as_array()
        .unwrap()
        .clone();
    let audit = events
        .iter()
        .find(|e| e["type"] == "device_updated")
        .expect("device_updated event recorded");
    assert_eq!(audit["actor"], "user");
    assert_eq!(audit["payload"]["device"], "mac");
    assert_eq!(
        audit["payload"]["fields"],
        json!(["role", "taints", "preference", "drain"])
    );

    // Tailscale browser user can also PATCH /v1/admin/devices/{name}.
    let (s, ts_updated) = t
        .send(
            "PATCH",
            "/v1/admin/devices/linux-runner",
            &[("tailscale-user-login", USER)],
            Some(json!({ "preference": 20 })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{ts_updated}");
    assert_eq!(ts_updated["preference"], 20);

    // Device token (`mac`) cannot heartbeat another device (`linux-runner`).
    let (code, err, v) = t
        .fail(
            "PUT",
            "/v1/devices/linux-runner/heartbeat",
            None,
            Some(json!({ "os": "linux", "arch": "x86_64" })),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    // Device token (`mac`) heartbeats itself, including rogue admin fields in payload:
    // live fields update, admin fields (`role`, `taints`, `preference`, `drain`) stay untouched!
    let hb = t
        .ok(
            "PUT",
            "/v1/devices/mac/heartbeat",
            None,
            Some(json!({
                "os": "macos",
                "arch": "arm64",
                "capacity": { "weight_units": 12, "mem_gib": 48, "cpu_cores": 14 },
                "free_units": 8,
                "capabilities": ["os:macos", "tool:xcode", "os:macos"],
                "runners": ["acp", "deterministic", "t3"],
                "repos": ["git@github.com:Example/App.git", "https://github.com/example/app"],
                "protocol_features": ["v1-json", "sse-v1"],
                "role": "runner",
                "taints": [],
                "preference": 999,
                "drain": false
            })),
        )
        .await;
    assert_eq!(hb["status"], "online");
    assert_eq!(hb["os"], "macos");
    assert_eq!(hb["arch"], "arm64");
    assert_eq!(hb["capacity"]["weight_units"], 12);
    assert_eq!(hb["free_units"], 8);
    assert_eq!(hb["capabilities"], json!(["os:macos", "tool:xcode"]));
    assert_eq!(hb["repos"], json!(["github.com/example/app"]));
    assert_eq!(hb["role"], "control", "heartbeat must never modify role");
    assert_eq!(
        hb["taints"],
        json!([{ "key": "role", "value": "control", "effect": "no_schedule" }]),
        "heartbeat must never clear taints"
    );
    assert_eq!(
        hb["preference"], -10,
        "heartbeat must never modify preference"
    );
    assert_eq!(hb["drain"], true, "heartbeat must never modify drain");
    assert!(hb["heartbeat_at"].is_string());

    // `linux-runner` heartbeats with its own token.
    let (s, lhb) = t
        .send(
            "PUT",
            "/v1/devices/linux-runner/heartbeat",
            &[("authorization", linux_bearer.as_str())],
            Some(json!({
                "os": "linux",
                "arch": "x86_64",
                "capacity": { "weight_units": 8 },
                "free_units": 8,
                "runners": ["acp"]
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{lhb}");
    assert_eq!(lhb["status"], "online");

    // Injectable clock: at +90s still online; at +91s transitions to stale.
    let status_of = |list: &Value, name: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == name)
            .unwrap()["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    t.advance(TimeDelta::seconds(90));
    let list = t.ok("GET", "/v1/devices", None, None).await;
    assert_eq!(status_of(&list, "mac"), "online");
    assert_eq!(status_of(&list, "phone"), "offline");

    t.advance(TimeDelta::seconds(1));
    let list = t.ok("GET", "/v1/devices", None, None).await;
    assert_eq!(status_of(&list, "mac"), "stale");
    assert_eq!(status_of(&list, "linux-runner"), "stale");

    // A `--user` token may also heartbeat a named device.
    let (s, uhb) = t
        .as_user(
            "PUT",
            "/v1/devices/mac/heartbeat",
            Some(json!({ "os": "macos", "arch": "arm64" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{uhb}");
    assert_eq!(uhb["status"], "online");

    // Revoked devices are hidden from GET /v1/devices, and recreating resets metadata.
    let (s, _) = t
        .as_user("DELETE", "/v1/admin/devices/linux-runner", None)
        .await;
    assert_eq!(s.as_u16(), 204);
    let list = t.ok("GET", "/v1/devices", None, None).await;
    assert!(
        !list
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == "linux-runner")
    );
    let (s, _) = t
        .as_user(
            "POST",
            "/v1/admin/devices",
            Some(json!({ "name": "linux-runner" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200);
    let list = t.ok("GET", "/v1/devices", None, None).await;
    let recreated = list
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "linux-runner")
        .unwrap();
    assert_eq!(recreated["status"], "offline");
    assert_eq!(recreated["preference"], 0);
    assert!(recreated.get("os").is_none());
}

#[tokio::test]
async fn server_refuses_unknown_open_enum_values_and_invalid_device_payloads() {
    let t = T::new();

    // Unknown DeviceRole -> 400 invalid
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/admin/devices/mac",
            Some(json!({ "role": "quantum_node" })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid")),
        "{v}"
    );

    // Unknown TaintEffect -> 400 invalid
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/admin/devices/mac",
            Some(json!({
                "taints": [{ "key": "role", "value": "control", "effect": "evict_now" }]
            })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid")),
        "{v}"
    );

    // Out-of-range preference -> 400 invalid
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/admin/devices/mac",
            Some(json!({ "preference": 5000 })),
        )
        .await;
    assert_eq!(
        (s.as_u16(), v["code"].as_str()),
        (400, Some("invalid")),
        "{v}"
    );

    // free_units > capacity.weight_units -> 400 invalid
    let (code, err, v) = t
        .fail(
            "PUT",
            "/v1/devices/mac/heartbeat",
            None,
            Some(json!({ "capacity": { "weight_units": 4 }, "free_units": 5 })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");

    // Local repo path in heartbeat -> 400 invalid
    let (code, err, v) = t
        .fail(
            "PUT",
            "/v1/devices/mac/heartbeat",
            None,
            Some(json!({ "repos": ["/tmp/local-repo"] })),
        )
        .await;
    assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");
}

#[tokio::test]
async fn every_response_and_healthz_advertise_x_board_features() {
    let t = T::new();
    let health = t.ok("GET", "/healthz", None, None).await;
    assert_eq!(
        health["features"],
        json!([
            "v1-json",
            "sse-v1",
            "device-caps-v1",
            "usage-buckets-v1",
            "runs-v1",
            "dispatch-claim-v1"
        ])
    );

    for path in ["/healthz", "/v1/devices", "/v1/boards"] {
        let req = Request::builder().uri(path).body(Body::empty()).unwrap();
        let resp = t.app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.headers()[board_proto::FEATURES_HEADER]
                .to_str()
                .unwrap(),
            board_proto::SERVER_FEATURES,
            "{path}"
        );
    }
}

#[tokio::test]
async fn tasks_persist_validate_and_update_routing_fields_and_search_snippets() {
    let t = T::new();
    t.register(ME, Some(REPO), None).await;
    t.board("factory", &[REPO]).await;

    // Create task with all routing fields; repo URL normalizes cleanly.
    let created = t
        .ok(
            "POST",
            "/v1/tasks",
            Some(ME),
            Some(json!({
                "agent": ME,
                "board": "factory",
                "title": "Compile release binary",
                "body": "Build the workspace crate with release optimizations",
                "repo": "git@github.com:Example/App.git",
                "weight": "heavy",
                "resource_units": 4,
                "requires": ["os:macos", "tool:xcode", "os:macos"],
                "tolerations": [
                    { "key": "weight", "value": "heavy", "effect": "prefer_no_schedule" }
                ],
                "conflict_keys": [
                    { "key": "repo:github.com/example/app:write", "max": 2 }
                ],
                "affinity": {
                    "prefer_device": "mac",
                    "prefer_warm_repo": true,
                    "preferred_devices": ["mac", "phone", "mac"]
                },
                "preferred_runner": "acp"
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["repo"], "github.com/example/app");
    assert_eq!(created["weight"], "heavy");
    assert_eq!(created["resource_units"], 4);
    assert_eq!(created["requires"], json!(["os:macos", "tool:xcode"]));
    assert_eq!(
        created["tolerations"],
        json!([{ "key": "weight", "value": "heavy", "effect": "prefer_no_schedule" }])
    );
    assert_eq!(
        created["conflict_keys"],
        json!([{ "key": "repo:github.com/example/app:write", "max": 2 }])
    );
    assert_eq!(
        created["affinity"],
        json!({
            "prefer_device": "mac",
            "prefer_warm_repo": true,
            "preferred_devices": ["mac", "phone"]
        })
    );
    assert_eq!(created["preferred_runner"], "acp");

    // GET /v1/tasks/{id} and GET /v1/tasks?q=... return routing fields + FTS snippet.
    let detail = t.ok("GET", &format!("/v1/tasks/{id}"), None, None).await;
    assert_eq!(detail["task"], created);
    let searched = t
        .ok("GET", "/v1/tasks?board=factory&q=optimizations", None, None)
        .await;
    let first = &searched.as_array().unwrap()[0];
    assert_eq!(first["id"], id);
    assert_eq!(first["weight"], "heavy");
    assert!(
        first["snippet"]
            .as_str()
            .unwrap()
            .contains("[optimizations]"),
        "{first}"
    );

    // PATCH updates subset of routing fields, clears others with null / [].
    let (s, patched) = t
        .patch(
            ME,
            &id,
            json!({
                "weight": "standard",
                "resource_units": null,
                "repo": null,
                "requires": [],
                "preferred_runner": "t3"
            }),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{patched}");
    assert_eq!(patched["weight"], "standard");
    assert!(patched.get("resource_units").is_none());
    assert!(patched.get("repo").is_none());
    assert!(patched.get("requires").is_none());
    assert_eq!(patched["preferred_runner"], "t3");
    // Omitted routing fields stay intact.
    assert_eq!(
        patched["conflict_keys"],
        json!([{ "key": "repo:github.com/example/app:write", "max": 2 }])
    );

    // Validation refusals -> 400 invalid
    for bad in [
        json!({ "agent": ME, "board": "factory", "title": "Bad weight", "weight": "mega" }),
        json!({ "agent": ME, "board": "factory", "title": "Zero units", "resource_units": 0 }),
        json!({ "agent": ME, "board": "factory", "title": "Huge units", "resource_units": 65 }),
        json!({ "agent": ME, "board": "factory", "title": "Local repo", "repo": "/tmp/repo" }),
        json!({
            "agent": ME, "board": "factory", "title": "Bad tol",
            "tolerations": [{ "key": "role", "effect": "evict" }]
        }),
        json!({
            "agent": ME, "board": "factory", "title": "Zero conflict max",
            "conflict_keys": [{ "key": "k", "max": 0 }]
        }),
        json!({
            "agent": ME, "board": "factory", "title": "Dup conflict key",
            "conflict_keys": [{ "key": "k", "max": 1 }, { "key": "k", "max": 2 }]
        }),
        json!({
            "agent": ME, "board": "factory", "title": "Bad req",
            "requires": ["has space"]
        }),
    ] {
        let (code, err, v) = t.fail("POST", "/v1/tasks", Some(ME), Some(bad)).await;
        assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");
    }
}

#[tokio::test]
async fn unrelated_updates_preserve_unknown_future_open_enum_values_on_rollback() {
    let t = T::new();
    t.register(ME, Some(REPO), None).await;
    t.board("factory", &[REPO]).await;
    let id = t.task(ME, "factory", "Future task").await;

    // Simulate a rolled-back server reading future enum values written by a newer server.
    t.exec(&format!(
        "UPDATE tasks SET weight = 'xlarge', \
         tolerations_json = '[{{\"key\":\"gpu\",\"effect\":\"no_execute\"}}]' \
         WHERE id = '{id}'; \
         UPDATE devices SET role = 'edge_gpu', \
         taints_json = '[{{\"key\":\"gpu\",\"effect\":\"no_execute\"}}]' \
         WHERE name = 'mac';"
    ));

    // Unrelated PATCH to the task (title only) must not overwrite weight or tolerations_json.
    let (s, patched) = t.patch(ME, &id, json!({ "title": "Renamed" })).await;
    assert_eq!(s.as_u16(), 200, "{patched}");
    assert_eq!(patched["weight"], "unknown");
    assert_eq!(
        t.strings(&format!("SELECT weight FROM tasks WHERE id = '{id}'")),
        ["xlarge"]
    );
    assert_eq!(
        t.strings(&format!(
            "SELECT tolerations_json FROM tasks WHERE id = '{id}'"
        )),
        [r#"[{"key":"gpu","effect":"no_execute"}]"#]
    );

    // Unrelated PATCH to the device (drain only) must not overwrite role or taints_json,
    // nor emit "unknown" in the device_updated audit event.
    let (s, updated) = t
        .as_user(
            "PATCH",
            "/v1/admin/devices/mac",
            Some(json!({ "drain": true })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{updated}");
    assert_eq!(updated["drain"], true);
    assert_eq!(
        t.strings("SELECT role FROM devices WHERE name = 'mac'"),
        ["edge_gpu"]
    );
    assert_eq!(
        t.strings("SELECT taints_json FROM devices WHERE name = 'mac'"),
        [r#"[{"key":"gpu","effect":"no_execute"}]"#]
    );
    let events = t
        .ok("GET", "/v1/events?since=0&limit=100", None, None)
        .await["events"]
        .as_array()
        .unwrap()
        .clone();
    let audit = events
        .iter()
        .rev()
        .find(|e| e["type"] == "device_updated")
        .unwrap();
    assert_eq!(audit["payload"]["fields"], json!(["drain"]));
    assert!(audit["payload"].get("role").is_none(), "{audit}");
    assert!(audit["payload"].get("taints").is_none(), "{audit}");
}
