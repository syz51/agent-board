//! `board hook` with recorded Claude Code payloads (shapes from
//! code.claude.com/docs/en/hooks) and Codex payloads (learn.chatgpt.com/docs/hooks.md).

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use board_proto::{AgentState, Handoff, Overlap, SnapshotKind, TaskStatus, TouchResult};
use common::*;
use serde_json::{Value, json};

const SESSION: &str = "00893aaf-19fa-41d2-8238-13269b9b3ca0";
const CLAUDE_AGENT: &str = "claude:00893aaf-19fa-41d2-8238-13269b9b3ca0";
const SNAPSHOT: &str =
    "<board agent=\"claude:0089\" board=\"factory\" seq=\"812\">\nyou: claude:0089\n</board>";

struct Env {
    _dir: tempfile::TempDir,
    stub: Stub,
    home: PathBuf,
    repo: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let home = home_for(dir.path(), &stub.url);
    let repo = git_repo(dir.path(), "git@github.com:syz51/agent-board.git");
    Env {
        _dir: dir,
        stub,
        home,
        repo,
    }
}

fn state_path() -> String {
    format!("POST /v1/agents/{}/state", enc(CLAUDE_AGENT))
}

impl Env {
    /// Runs a hook as Claude Code does: JSON on stdin, cwd in the payload.
    fn hook(&self, event: &str, runtime: &str, payload: Value) -> (Option<Value>, String) {
        self.hook_env(event, runtime, payload, &[])
    }

    fn hook_env(
        &self,
        event: &str,
        runtime: &str,
        payload: Value,
        env: &[(&str, &str)],
    ) -> (Option<Value>, String) {
        let out = board(
            &self.home,
            &self.repo,
            env,
            &["hook", event, "--runtime", runtime],
            &payload.to_string(),
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "hooks always exit 0: {}",
            stderr(&out)
        );
        let text = stdout(&out);
        (serde_json::from_str(&text).ok(), text)
    }

    fn base(&self, event: &str) -> Value {
        json!({
            "session_id": SESSION,
            "transcript_path": "/Users/me/.claude/projects/x/00893aaf.jsonl",
            "cwd": self.repo,
            "permission_mode": "default",
            "hook_event_name": event,
        })
    }

    fn with(&self, event: &str, extra: Value) -> Value {
        let mut v = self.base(event);
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    }

    fn agent_ok(&self) {
        self.stub.on(
            &state_path(),
            agent_fixture(CLAUDE_AGENT, DEVICE, AgentState::Running),
        );
    }
}

fn context(reply: &Option<Value>, event: &str) -> String {
    let reply = reply.as_ref().expect("JSON output");
    assert_eq!(reply["hookSpecificOutput"]["hookEventName"], event);
    reply["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn session_start_registers_injects_and_caches_the_snapshot() {
    let e = env();
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(CLAUDE_AGENT, DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
    );
    let payload = e.with(
        "SessionStart",
        json!({"source": "startup", "model": "claude-opus-5-5"}),
    );
    let (reply, _) = e.hook("session-start", "claude", payload);
    assert_eq!(context(&reply, "SessionStart"), SNAPSHOT);

    let register = &e.stub.requests_to("POST /v1/agents")[0];
    assert_eq!(register.agent.as_deref(), Some(CLAUDE_AGENT));
    assert_eq!(register.body["id"], CLAUDE_AGENT);
    assert_eq!(register.body["runtime"], "claude");
    assert_eq!(register.body["device"], DEVICE);
    assert_eq!(register.body["repo"], "github.com/syz51/agent-board");
    assert_eq!(register.body["branch"], "feature/x");
    assert_eq!(register.body["native_ids"]["claude_session"], SESSION);
    let q = e.stub.requests_to("GET /v1/snapshot")[0].query_pairs();
    assert_eq!(
        (q["kind"].as_str(), q["format"].as_str()),
        ("full", "lines")
    );
    assert!(
        !q.contains_key("framing"),
        "the default framing is not sent"
    );

    let cached = std::fs::read_dir(e.home.join("cache")).unwrap().count();
    assert_eq!(cached, 1, "the full snapshot is cached for offline use");
}

#[test]
fn session_start_without_a_board_hints_only_when_the_repo_has_one() {
    let e = env();
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(CLAUDE_AGENT, DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Full, "", 5),
    );
    e.stub.on(
        "GET /v1/boards",
        [board_fixture("other", &["github.com/o/other"])],
    );
    let payload = e.with("SessionStart", json!({"source": "resume"}));
    let (_, text) = e.hook("session-start", "claude", payload.clone());
    assert_eq!(
        text, "",
        "no board for the agent or its repo: inject nothing"
    );

    e.stub.on(
        "GET /v1/boards",
        [board_fixture("factory", &["github.com/syz51/agent-board"])],
    );
    let (reply, _) = e.hook("session-start", "claude", payload);
    let hint = context(&reply, "SessionStart");
    assert_eq!(hint.lines().count(), 1);
    assert!(
        hint.contains("board factory (active)") && hint.contains("board_join factory"),
        "{hint}"
    );
}

#[test]
fn user_prompt_submit_injects_the_delta_since_the_last_seq() {
    let e = env();
    e.agent_ok();
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(CLAUDE_AGENT, DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
    );
    e.hook(
        "session-start",
        "claude",
        e.with("SessionStart", json!({"source": "startup"})),
    );

    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Delta, "<board>\nalerts: x\n</board>", 815),
    );
    let prompt = e.with("UserPromptSubmit", json!({"prompt": "continue"}));
    let (reply, _) = e.hook("user-prompt-submit", "claude", prompt.clone());
    assert!(context(&reply, "UserPromptSubmit").contains("alerts: x"));
    let q = e.stub.requests_to("GET /v1/snapshot")[1].query_pairs();
    assert_eq!((q["kind"].as_str(), q["since"].as_str()), ("delta", "812"));
    assert_eq!(
        e.stub.requests_to(&state_path())[0].body["state"],
        "running"
    );

    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Delta, "", 815),
    );
    let (_, text) = e.hook("user-prompt-submit", "claude", prompt);
    assert_eq!(text, "", "nothing changed: inject nothing");
    assert_eq!(
        e.stub.requests_to("GET /v1/snapshot")[2].query_pairs()["since"],
        "815"
    );
}

#[test]
fn post_tool_use_records_touches_warns_on_overlap_and_throttles_heartbeats() {
    let e = env();
    e.agent_ok();
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Alerts, "! factory-9 done -> task_update", 20),
    );
    e.stub.on(
        "POST /v1/touches",
        TouchResult {
            overlaps: vec![Overlap {
                path: "src/lib.rs".into(),
                agent: "t3:a91c".parse().unwrap(),
                handle: "t3:a91c".into(),
                via: "touch".into(),
                at: chrono::Utc::now() - chrono::Duration::minutes(3),
            }],
        },
    );
    let edit = e.with(
        "PostToolUse",
        json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": e.repo.join("src/lib.rs"), "old_string": "a", "new_string": "b"},
            "tool_response": {"filePath": e.repo.join("src/lib.rs"), "success": true},
            "tool_use_id": "toolu_01ABC123",
        }),
    );
    let (reply, _) = e.hook("post-tool-use", "claude", edit.clone());
    let text = context(&reply, "PostToolUse");
    assert!(
        text.contains("! t3:a91c edited \"src/lib.rs\" 3m ago -> re-read before editing"),
        "{text}"
    );
    assert!(
        text.contains("! factory-9 done"),
        "first call in a minute runs the heartbeat: {text}"
    );
    assert_eq!(
        e.stub.requests_to("POST /v1/touches")[0].body,
        json!({"agent": CLAUDE_AGENT, "repo": "github.com/syz51/agent-board", "paths": ["src/lib.rs"]})
    );
    assert_eq!(e.stub.requests_to(&state_path()).len(), 1);
    assert_eq!(
        e.stub.requests_to("GET /v1/snapshot")[0].query_pairs()["kind"],
        "alerts"
    );

    // Within the minute: touches still recorded, no heartbeat or alert check.
    let read = e.with(
        "PostToolUse",
        json!({"tool_name": "Read", "tool_input": {"file_path": "/x"}}),
    );
    let (_, text) = e.hook("post-tool-use", "claude", read);
    assert_eq!(text, "");
    e.hook("post-tool-use", "claude", edit);
    assert_eq!(e.stub.requests_to("POST /v1/touches").len(), 2);
    assert_eq!(
        e.stub.requests_to(&state_path()).len(),
        1,
        "heartbeat throttled to once a minute"
    );
    assert_eq!(e.stub.requests_to("GET /v1/snapshot").len(), 1);
}

#[test]
fn codex_apply_patch_records_each_patched_file() {
    let e = env();
    let codex_agent = "codex:thr_123";
    e.stub.on(
        &format!("POST /v1/agents/{}/state", enc(codex_agent)),
        agent_fixture(codex_agent, DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Alerts, "", 1),
    );
    e.stub
        .on("POST /v1/touches", TouchResult { overlaps: vec![] });
    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: docs/b.md\n+hi\n*** End Patch\n";
    let payload = json!({
        "session_id": "thr_123",
        "transcript_path": null,
        "cwd": e.repo,
        "hook_event_name": "PostToolUse",
        "model": "gpt-5.5-codex",
        "turn_id": "turn_1",
        "permission_mode": "default",
        "tool_name": "apply_patch",
        "tool_use_id": "call_1",
        "tool_input": {"command": patch},
        "tool_response": "Success",
    });
    let (_, text) = e.hook("post-tool-use", "codex", payload);
    assert_eq!(text, "");
    let touch = &e.stub.requests_to("POST /v1/touches")[0];
    assert_eq!(touch.agent.as_deref(), Some(codex_agent));
    assert_eq!(touch.body["paths"], json!(["src/a.rs", "docs/b.md"]));
}

#[test]
fn stop_reminds_once_about_a_stale_handoff() {
    let e = env();
    e.agent_ok();
    let mut task = task_fixture("factory-12", "Do it");
    task.status = TaskStatus::Doing;
    e.stub.on("GET /v1/tasks", [task]);
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Delta, "", 1),
    );
    e.hook(
        "user-prompt-submit",
        "claude",
        e.with("UserPromptSubmit", json!({"prompt": "go"})),
    );
    let handoff = Handoff {
        agent: CLAUDE_AGENT.parse().unwrap(),
        now: "old".into(),
        next: "x".into(),
        blockers: String::new(),
        version: 2,
        updated_at: chrono::Utc::now() - chrono::Duration::hours(1),
    };
    e.stub.on(
        &format!("GET /v1/agents/{}/handoff", enc(CLAUDE_AGENT)),
        &handoff,
    );

    let stop = e.with(
        "Stop",
        json!({"stop_hook_active": false, "last_assistant_message": "Done."}),
    );
    let (reply, _) = e.hook("stop", "claude", stop.clone());
    let reply = reply.unwrap();
    assert_eq!(reply["decision"], "block");
    assert!(
        reply["reason"].as_str().unwrap().contains("factory-12")
            && reply["reason"].as_str().unwrap().contains("handoff_set")
    );
    assert_eq!(
        e.stub.requests_to(&state_path()).last().unwrap().body["state"],
        "idle"
    );

    let (_, text) = e.hook("stop", "claude", stop);
    assert_eq!(text, "", "one reminder per turn");
    let continued = e.with("Stop", json!({"stop_hook_active": true}));
    let (_, text) = e.hook("stop", "claude", continued);
    assert_eq!(text, "");
}

#[test]
fn codex_stop_always_prints_json() {
    let e = env();
    let codex_agent = "codex:thr_9";
    e.stub.on(
        &format!("POST /v1/agents/{}/state", enc(codex_agent)),
        agent_fixture(codex_agent, DEVICE, AgentState::Idle),
    );
    e.stub.on("GET /v1/tasks", Vec::<board_proto::Task>::new());
    let payload = json!({
        "session_id": "thr_9", "transcript_path": null, "cwd": e.repo, "hook_event_name": "Stop",
        "model": "gpt-5.5-codex", "turn_id": "t", "stop_hook_active": false, "last_assistant_message": "ok",
    });
    let (reply, _) = e.hook("stop", "codex", payload);
    assert_eq!(reply, Some(json!({})));
}

#[test]
fn stop_failure_on_a_usage_limit_pauses_until_the_reset() {
    let e = env();
    e.agent_ok();
    let payload = e.with(
        "StopFailure",
        json!({
            "error": "rate_limit",
            "error_details": "429 Too Many Requests",
            "last_assistant_message": "Claude AI usage limit reached|1762444800",
        }),
    );
    let (_, text) = e.hook("stop-failure", "claude", payload);
    assert_eq!(text, "", "StopFailure output is ignored");
    let body = &e.stub.requests_to(&state_path())[0].body;
    assert_eq!(body["state"], "paused");
    assert_eq!(body["paused_until"], "2025-11-06T16:00:00Z");

    e.stub.clear();
    let other = e.with("StopFailure", json!({"error": "server_error", "error_details": "500", "last_assistant_message": "API Error"}));
    e.hook("stop-failure", "claude", other);
    assert!(
        e.stub.requests_to(&state_path()).is_empty(),
        "only usage limits pause"
    );
}

#[test]
fn session_end_marks_the_agent_ended() {
    let e = env();
    e.agent_ok();
    let (_, text) = e.hook(
        "session-end",
        "claude",
        e.with("SessionEnd", json!({"reason": "prompt_input_exit"})),
    );
    assert_eq!(text, "");
    assert_eq!(
        e.stub.requests_to(&state_path())[0].body,
        json!({"state": "ended", "paused_until": null})
    );
}

#[test]
fn pre_compact_keeps_the_agent_and_task_ids() {
    let e = env();
    let mut task = task_fixture("factory-12", "Do it");
    task.status = TaskStatus::Doing;
    e.stub.on("GET /v1/tasks", [task]);
    let payload = e.with(
        "PreCompact",
        json!({"trigger": "auto", "custom_instructions": null}),
    );
    let (_, text) = e.hook("pre-compact", "claude", payload);
    assert!(
        text.contains(CLAUDE_AGENT) && text.contains("factory-12"),
        "{text}"
    );
    assert!(
        serde_json::from_str::<Value>(&text).is_err(),
        "plain stdout becomes compaction instructions"
    );

    let codex = json!({"session_id": "thr_1", "cwd": e.repo, "hook_event_name": "PreCompact", "trigger": "auto"});
    let (_, text) = e.hook("pre-compact", "codex", codex);
    assert_eq!(
        text, "",
        "Codex has no compaction instructions; SessionStart(compact) re-injects instead"
    );
}

#[test]
fn unreachable_server_falls_back_to_the_cached_snapshot_within_budget() {
    let e = env();
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(CLAUDE_AGENT, DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
    );
    let start = e.with("SessionStart", json!({"source": "startup"}));
    e.hook("session-start", "claude", start.clone());

    e.stub.hang(Duration::from_secs(10));
    let began = Instant::now();
    let (reply, _) = e.hook("session-start", "claude", start.clone());
    let elapsed = began.elapsed();
    assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
    let text = context(&reply, "SessionStart");
    assert!(
        text.starts_with("board server unreachable: cached snapshot from "),
        "{text}"
    );
    assert!(
        text.contains(" ago (seq 812)") && text.ends_with(SNAPSHOT),
        "{text}"
    );

    // A closed port fails fast and falls back the same way.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    std::fs::write(
        e.home.join("config.toml"),
        format!("server = \"{url}\"\ndevice = \"{DEVICE}\"\n"),
    )
    .unwrap();
    let (reply, _) = e.hook("session-start", "claude", start);
    assert!(context(&reply, "SessionStart").contains("cached snapshot"));
    let (_, text) = e.hook(
        "post-tool-use",
        "claude",
        e.with(
            "PostToolUse",
            json!({"tool_name": "Read", "tool_input": {}}),
        ),
    );
    assert_eq!(text, "", "mid-turn hooks stay quiet offline");
}

#[test]
fn hooks_without_an_agent_or_login_do_nothing() {
    let e = env();
    let (_, text) = e.hook(
        "session-start",
        "claude",
        json!({"cwd": e.repo, "hook_event_name": "SessionStart"}),
    );
    assert_eq!(text, "");
    let (_, text) = e.hook("session-start", "claude", Value::String("not json".into()));
    assert_eq!(text, "");
    std::fs::remove_file(e.home.join("config.toml")).unwrap();
    let (_, text) = e.hook("session-start", "claude", e.base("SessionStart"));
    assert_eq!(text, "");
    assert!(e.stub.requests().is_empty());
}

#[test]
fn t3_thread_id_wins_over_the_hook_session() {
    let e = env();
    let t3 = "t3:thread-7";
    e.stub.on(
        &format!("POST /v1/agents/{}/state", enc(t3)),
        agent_fixture(t3, DEVICE, AgentState::Ended),
    );
    e.hook_env(
        "session-end",
        "claude",
        e.base("SessionEnd"),
        &[("T3CODE_THREAD_ID", "thread-7")],
    );
    assert_eq!(e.stub.requests()[0].agent.as_deref(), Some(t3));
}

#[test]
fn experimental_style_reaches_the_snapshot_request() {
    let e = env();
    let config = e.home.join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[experimental]\ntext_format = \"toon\"\npeer_framing = \"datamarked\"\n");
    std::fs::write(&config, text).unwrap();
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(CLAUDE_AGENT, DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
    );
    let payload = e.with("SessionStart", json!({"source": "compact"}));
    let (reply, _) = e.hook("session-start", "claude", payload);
    assert_eq!(context(&reply, "SessionStart"), SNAPSHOT);
    let q = e.stub.requests_to("GET /v1/snapshot")[0].query_pairs();
    assert_eq!(
        (q["format"].as_str(), q["framing"].as_str()),
        ("toon", "datamarked")
    );
}
