//! Interactive session unification into the `Run` model (`docs/SCHEDULER_DESIGN.md`,
//! Section 5.1; Slice 4.1).

use chrono::TimeDelta;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{REPO, T, check, defaults};
use crate::auth::hex;

const T3_AGENT: &str = "t3:thread-interactive-1";
const CLAUDE_AGENT: &str = "claude:term-sess-1";
const CODEX_AGENT: &str = "codex:term-sess-2";
const OPENCODE_AGENT: &str = "opencode:ses_3";
const AGY_AGENT: &str = "antigravity:conv-4";
const DISPATCH_MAC: &str = "dispatch:mac";

fn expected_interactive_id(agent: &str) -> String {
    let digest = hex(&Sha256::digest(agent.as_bytes()));
    format!("run-interactive-{}", &digest[..12])
}

#[tokio::test]
async fn interactive_sessions_across_launchers_appear_and_transition_cleanly() {
    let t = T::new();
    t.board("factory", &[REPO]).await;

    // 1. Register T3, standalone Claude, Codex, OpenCode, and Antigravity sessions, plus
    //    `dispatch:mac` (which must NOT create an interactive run).
    t.register(DISPATCH_MAC, None, None).await;

    let t3_link = "https://app.example.test/threads/env-1/thread-interactive-1";
    t.ok(
        "POST",
        "/v1/agents",
        Some(T3_AGENT),
        Some(json!({
            "id": T3_AGENT,
            "runtime": "claude",
            "device": "mac",
            "title": "Build interactive cockpit",
            "repo": REPO,
            "branch": "feat/cockpit",
            "cwd": "/Users/dev/work/app",
            "native_ids": {
                "t3_thread": "thread-interactive-1",
                "t3_link": t3_link,
            }
        })),
    )
    .await;

    for (agent, runtime, cwd, branch, title) in [
        (
            CLAUDE_AGENT,
            "claude",
            "/Users/dev/work/app",
            "feat/term-claude",
            Some("Terminal Claude"),
        ),
        (
            CODEX_AGENT,
            "codex",
            "/Users/dev/work/app",
            "feat/term-codex",
            None,
        ),
        (
            OPENCODE_AGENT,
            "opencode",
            "/Users/dev/work/app",
            "feat/opencode",
            Some("OpenCode session"),
        ),
        (
            AGY_AGENT,
            "antigravity",
            "/Users/dev/work/app",
            "feat/agy",
            Some("Antigravity IDE"),
        ),
    ] {
        t.ok(
            "POST",
            "/v1/agents",
            Some(agent),
            Some(json!({
                "id": agent,
                "runtime": runtime,
                "device": "mac",
                "title": title,
                "repo": REPO,
                "branch": branch,
                "cwd": cwd,
            })),
        )
        .await;
    }

    let interactive = t
        .ok("GET", "/v1/runs?origin=interactive&active=true", None, None)
        .await;
    let runs = interactive.as_array().unwrap();
    assert_eq!(
        runs.len(),
        5,
        "all 5 user sessions appear, dispatch:mac excluded: {interactive}"
    );
    assert!(
        runs.iter().all(|r| r["agent_id"] != DISPATCH_MAC),
        "dispatch agent must never create an interactive run"
    );

    let find = |agent: &str| -> &Value {
        runs.iter()
            .find(|r| r["agent_id"] == agent)
            .unwrap_or_else(|| panic!("missing interactive run for {agent}: {interactive}"))
    };

    let t3_run = find(T3_AGENT);
    assert_eq!(t3_run["id"], expected_interactive_id(T3_AGENT));
    assert_eq!(t3_run["kind"], "interactive");
    assert_eq!(t3_run["origin"], "interactive");
    assert_eq!(t3_run["device"], "mac");
    assert_eq!(t3_run["runner"], "t3");
    assert_eq!(t3_run["board"], "factory");
    assert_eq!(t3_run["state"], "running");
    assert_eq!(t3_run["worktree_branch"], "feat/cockpit");
    assert_eq!(t3_run["cwd"], "/Users/dev/work/app");
    assert_eq!(t3_run["title"], "Build interactive cockpit");
    assert_eq!(t3_run["attach_hint"], t3_link);
    assert_eq!(t3_run["cost_units"], 2);
    assert!(t3_run["started_at"].is_string());
    assert!(t3_run.get("ended_at").is_none());
    assert!(t3_run.get("lease_expires_at").is_none());

    for (agent, expected_runner, expected_branch) in [
        (CLAUDE_AGENT, "claude", "feat/term-claude"),
        (CODEX_AGENT, "codex", "feat/term-codex"),
        (OPENCODE_AGENT, "opencode", "feat/opencode"),
        (AGY_AGENT, "antigravity", "feat/agy"),
    ] {
        let r = find(agent);
        assert_eq!(r["id"], expected_interactive_id(agent));
        assert_eq!(r["runner"], expected_runner);
        assert_eq!(r["worktree_branch"], expected_branch);
        assert_eq!(r["cwd"], "/Users/dev/work/app");
        assert_eq!(r["attach_hint"], "ssh://mac/Users/dev/work/app");
        assert_eq!(r["board"], "factory");
        assert_eq!(r["cost_units"], 2);
        assert_eq!(r["state"], "running");
    }

    // 2. Repeated POST /v1/agents updates the SAME active interactive run in place.
    t.advance(TimeDelta::seconds(30));
    t.ok(
        "POST",
        "/v1/agents",
        Some(T3_AGENT),
        Some(json!({
            "id": T3_AGENT,
            "runtime": "claude",
            "device": "mac",
            "title": "Renamed cockpit thread",
            "repo": REPO,
            "branch": "feat/cockpit-v2",
            "cwd": "/Users/dev/work/app-wt",
        })),
    )
    .await;
    let t3_runs = t
        .ok("GET", &format!("/v1/runs?agent={T3_AGENT}"), None, None)
        .await;
    assert_eq!(t3_runs.as_array().unwrap().len(), 1);
    assert_eq!(t3_runs[0]["id"], expected_interactive_id(T3_AGENT));
    assert_eq!(t3_runs[0]["title"], "Renamed cockpit thread");
    assert_eq!(t3_runs[0]["worktree_branch"], "feat/cockpit-v2");
    assert_eq!(t3_runs[0]["cwd"], "/Users/dev/work/app-wt");
    // Merged t3_link is preserved on re-registration without native_ids.
    assert_eq!(t3_runs[0]["attach_hint"], t3_link);

    // 3. State lifecycle: running -> waiting_external (idle & paused) -> running -> succeeded (ended).
    t.ok(
        "POST",
        &format!("/v1/agents/{CLAUDE_AGENT}/state"),
        Some(CLAUDE_AGENT),
        Some(json!({ "state": "idle" })),
    )
    .await;
    let claude_id = expected_interactive_id(CLAUDE_AGENT);
    let after_idle = t
        .ok("GET", &format!("/v1/runs/{claude_id}"), None, None)
        .await;
    assert_eq!(after_idle["state"], "waiting_external");
    assert!(after_idle.get("ended_at").is_none());

    t.ok(
        "POST",
        &format!("/v1/agents/{CLAUDE_AGENT}/state"),
        Some(CLAUDE_AGENT),
        Some(json!({ "state": "paused" })),
    )
    .await;
    assert_eq!(
        t.ok("GET", &format!("/v1/runs/{claude_id}"), None, None)
            .await["state"],
        "waiting_external"
    );

    t.ok(
        "POST",
        &format!("/v1/agents/{CLAUDE_AGENT}/state"),
        Some(CLAUDE_AGENT),
        Some(json!({ "state": "running" })),
    )
    .await;
    assert_eq!(
        t.ok("GET", &format!("/v1/runs/{claude_id}"), None, None)
            .await["state"],
        "running"
    );

    t.advance(TimeDelta::seconds(60));
    t.ok(
        "POST",
        &format!("/v1/agents/{CLAUDE_AGENT}/state"),
        Some(CLAUDE_AGENT),
        Some(json!({ "state": "ended" })),
    )
    .await;
    let after_end = t
        .ok("GET", &format!("/v1/runs/{claude_id}"), None, None)
        .await;
    assert_eq!(after_end["state"], "succeeded");
    assert!(after_end["ended_at"].is_string());

    let active_for_claude = t
        .ok(
            "GET",
            &format!("/v1/runs?agent={CLAUDE_AGENT}&active=true"),
            None,
            None,
        )
        .await;
    assert!(active_for_claude.as_array().unwrap().is_empty());

    // 4. Starting a fresh session for the same agent after `ended` preserves the terminal
    //    run immutably and allocates a suffixed fresh interactive run (`-2`).
    t.advance(TimeDelta::seconds(10));
    t.register_as(CLAUDE_AGENT, "claude", Some(REPO), Some("feat/second"))
        .await;
    let all_claude = t
        .ok("GET", &format!("/v1/runs?agent={CLAUDE_AGENT}"), None, None)
        .await;
    let arr = all_claude.as_array().unwrap();
    assert_eq!(arr.len(), 2, "{all_claude}");
    assert_eq!(arr[0]["id"], format!("{claude_id}-2"));
    assert_eq!(arr[0]["state"], "running");
    assert_eq!(arr[0]["worktree_branch"], "feat/second");
    assert_eq!(arr[1]["id"], claude_id);
    assert_eq!(arr[1]["state"], "succeeded");
}

#[tokio::test]
async fn interactive_run_syncs_doing_task_and_closes_on_end_unseen() {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    t.board("other", &[]).await;

    t.ok(
        "POST",
        "/v1/agents",
        Some(T3_AGENT),
        Some(json!({
            "id": T3_AGENT,
            "runtime": "antigravity",
            "device": "mac",
            "repo": REPO,
            "branch": "main",
            "cwd": "/tmp/app",
        })),
    )
    .await;
    let run_id = expected_interactive_id(T3_AGENT);
    let initial = t.ok("GET", &format!("/v1/runs/{run_id}"), None, None).await;
    assert!(initial.get("task_id").is_none());
    assert_eq!(initial["board"], "factory");

    // Claiming a task syncs `task_id` and `board` onto the active interactive run.
    let task1 = t.task(T3_AGENT, "other", "Interactive claimed work").await;
    assert_eq!(t.claim(T3_AGENT, &task1).await.0, 200);
    let after_claim = t.ok("GET", &format!("/v1/runs/{run_id}"), None, None).await;
    assert_eq!(after_claim["task_id"], task1);
    assert_eq!(after_claim["board"], "other");

    // Parking in review clears `task_id` while keeping `board`.
    assert_eq!(
        t.patch(T3_AGENT, &task1, json!({ "status": "review" }))
            .await
            .0,
        200
    );
    let after_review = t.ok("GET", &format!("/v1/runs/{run_id}"), None, None).await;
    assert!(after_review.get("task_id").is_none());
    assert_eq!(after_review["board"], "other");

    // Creating a task with `claim: true` also syncs `task_id` immediately.
    let task2 = t
        .task_with(
            T3_AGENT,
            json!({ "board": "factory", "title": "Direct claimed", "claim": true }),
        )
        .await;
    let after_create_claim = t.ok("GET", &format!("/v1/runs/{run_id}"), None, None).await;
    assert_eq!(after_create_claim["task_id"], task2);
    assert_eq!(after_create_claim["board"], "factory");

    // Completing the task clears `task_id`.
    assert_eq!(t.done(T3_AGENT, &task2, check(0)).await.0, 200);
    let after_done = t.ok("GET", &format!("/v1/runs/{run_id}"), None, None).await;
    assert!(after_done.get("task_id").is_none());

    // Going unseen past `owner_left_secs` causes `end_unseen` to close the interactive run.
    t.advance(TimeDelta::seconds(
        i64::try_from(defaults().owner_left_secs).unwrap() + 10,
    ));
    assert_eq!(t.end_unseen(), 1);
    let after_sweep = t.ok("GET", &format!("/v1/runs/{run_id}"), None, None).await;
    assert_eq!(after_sweep["state"], "succeeded");
    assert!(after_sweep["ended_at"].is_string());

    // Reviving the ended agent via a write call (`touch_agent`) opens a fresh interactive run.
    t.advance(TimeDelta::seconds(5));
    let _task3 = t
        .task_with(
            T3_AGENT,
            json!({ "board": "factory", "title": "After revival", "claim": true }),
        )
        .await;
    let active = t
        .ok(
            "GET",
            &format!("/v1/runs?agent={T3_AGENT}&active=true"),
            None,
            None,
        )
        .await;
    assert_eq!(active.as_array().unwrap().len(), 1, "{active}");
    assert_eq!(active[0]["id"], format!("{run_id}-2"));
    assert_eq!(active[0]["state"], "running");
    assert_eq!(active[0]["task_id"], _task3);
}

#[tokio::test]
async fn managed_runs_never_duplicate_with_interactive_hooks_in_either_order() {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    let creator = "t3:creator";
    t.register(creator, Some(REPO), None).await;
    let task1 = t.task(creator, "factory", "Managed worker 1").await;
    let task2 = t.task(creator, "factory", "Managed worker 2").await;

    // Order A: managed run exists BEFORE `POST /v1/agents` fires.
    let worker_a = "t3:managed-worker-a";
    let run_a = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "task_id": task1,
                "kind": "worker",
                "origin": "managed",
                "device": "mac",
                "runner": "t3",
                "state": "starting"
            })),
        )
        .await;
    let run_a_id = run_a["id"].as_str().unwrap().to_owned();
    // Register agent first? No — attach `agent_id` once registered, or test registering while
    // already attached: create agent on `agents` via register, attach to managed run, then
    // re-register with richer hook metadata.
    t.register(worker_a, Some(REPO), Some("main")).await;
    t.ok(
        "PATCH",
        &format!("/v1/runs/{run_a_id}"),
        Some(DISPATCH_MAC),
        Some(json!({ "agent_id": worker_a, "state": "running" })),
    )
    .await;

    // Re-registering via SessionStart/rename hook syncs metadata onto the managed run and
    // creates ZERO interactive runs.
    let link_a = "https://app.example.test/threads/env-1/managed-worker-a";
    t.ok(
        "POST",
        "/v1/agents",
        Some(worker_a),
        Some(json!({
            "id": worker_a,
            "runtime": "antigravity",
            "device": "mac",
            "title": "Managed title from T3",
            "repo": REPO,
            "branch": "task/factory-1",
            "cwd": "/tmp/wt/factory-1",
            "native_ids": { "t3_link": link_a }
        })),
    )
    .await;

    let runs_a = t
        .ok("GET", &format!("/v1/runs?agent={worker_a}"), None, None)
        .await;
    assert_eq!(
        runs_a.as_array().unwrap().len(),
        1,
        "must not duplicate managed run: {runs_a}"
    );
    assert_eq!(runs_a[0]["id"], run_a_id);
    assert_eq!(runs_a[0]["origin"], "managed");
    assert_eq!(runs_a[0]["worktree_branch"], "task/factory-1");
    assert_eq!(runs_a[0]["cwd"], "/tmp/wt/factory-1");
    assert_eq!(runs_a[0]["title"], "Managed title from T3");
    assert_eq!(runs_a[0]["attach_hint"], link_a);

    // Agent state hook (`idle` / `ended`) must not transition a managed run or spawn an interactive one.
    t.ok(
        "POST",
        &format!("/v1/agents/{worker_a}/state"),
        Some(worker_a),
        Some(json!({ "state": "idle" })),
    )
    .await;
    assert_eq!(
        t.ok("GET", &format!("/v1/runs/{run_a_id}"), None, None)
            .await["state"],
        "running"
    );
    assert_eq!(
        t.ok("GET", &format!("/v1/runs?agent={worker_a}"), None, None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Order B: `SessionStart` hook (`POST /v1/agents`) races AHEAD of `POST /v1/runs` linking `agent_id`.
    let worker_b = "antigravity:acp-worker-b";
    t.ok(
        "POST",
        "/v1/agents",
        Some(worker_b),
        Some(json!({
            "id": worker_b,
            "runtime": "antigravity",
            "device": "mac",
            "title": "ACP worker session",
            "repo": REPO,
            "branch": "task/factory-2",
            "cwd": "/tmp/wt/factory-2",
        })),
    )
    .await;
    // Before daemon links it, an auto-upserted interactive placeholder exists.
    assert_eq!(
        t.ok(
            "GET",
            &format!("/v1/runs?agent={worker_b}&origin=interactive"),
            None,
            None
        )
        .await
        .as_array()
        .unwrap()
        .len(),
        1
    );

    // Daemon links the managed run -> absorbs missing metadata and removes placeholder.
    let run_b = t
        .ok(
            "POST",
            "/v1/runs",
            Some(DISPATCH_MAC),
            Some(json!({
                "task_id": task2,
                "kind": "worker",
                "origin": "managed",
                "device": "mac",
                "runner": "acp",
                "agent_id": worker_b,
                "state": "running"
            })),
        )
        .await;
    assert_eq!(run_b["origin"], "managed");
    assert_eq!(run_b["worktree_branch"], "task/factory-2");
    assert_eq!(run_b["cwd"], "/tmp/wt/factory-2");
    assert_eq!(run_b["title"], "ACP worker session");
    assert_eq!(run_b["attach_hint"], "ssh://mac/tmp/wt/factory-2");

    let runs_b = t
        .ok("GET", &format!("/v1/runs?agent={worker_b}"), None, None)
        .await;
    assert_eq!(
        runs_b.as_array().unwrap().len(),
        1,
        "placeholder interactive run absorbed without leaving duplicate: {runs_b}"
    );
    assert_eq!(runs_b[0]["id"], run_b["id"]);
}
