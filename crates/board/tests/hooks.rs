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

/// Sets a file's modification time well before any hook in the test ran.
fn backdate(path: &std::path::Path) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
}

/// Commits everything in `repo`.
fn commit_all(repo: &std::path::Path) {
    for args in [
        &["add", "-A"][..],
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "base",
        ],
    ] {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }
}

#[test]
fn shell_calls_record_the_files_they_changed() {
    let e = env();
    e.agent_ok();
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Alerts, "", 1),
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
    // Tracked and clean before the call.
    std::fs::write(e.repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(e.repo.join(".gitignore"), "target/\n").unwrap();
    commit_all(&e.repo);
    // Changed before the call: dirty in git, but not by this call.
    std::fs::write(e.repo.join("README.md"), "old\n").unwrap();
    backdate(&e.repo.join("README.md"));
    let command = "sed -i.bak s/a/b/ src/lib.rs && rm src/lib.rs.bak";
    let call = |event: &str, id: &str| {
        e.with(
            event,
            json!({
                "tool_name": "Bash",
                "tool_input": {"command": command, "description": "Edit lib.rs"},
                "tool_use_id": id,
            }),
        )
    };

    let (reply, text) = e.hook("pre-tool-use", "claude", call("PreToolUse", "toolu_sh1"));
    assert_eq!(
        (reply, text.as_str()),
        (None, ""),
        "PreToolUse lets the call run"
    );
    assert!(
        e.stub.requests().is_empty(),
        "PreToolUse never calls the server"
    );
    // What the command does: adds files, edits a tracked one, writes an ignored one and
    // one whose path the server would refuse (over 300 characters).
    std::fs::write(e.repo.join("src/lib.rs"), "b\n").unwrap();
    std::fs::create_dir_all(e.repo.join("src/new")).unwrap();
    std::fs::write(e.repo.join("src/new/mod.rs"), "x\n").unwrap();
    std::fs::write(e.repo.join("src/main.rs"), "fn main() { run() }\n").unwrap();
    std::fs::create_dir_all(e.repo.join("target")).unwrap();
    std::fs::write(e.repo.join("target/out"), "x\n").unwrap();
    let deep: PathBuf = (0..4).map(|i| format!("{i}{}", "d".repeat(80))).collect();
    std::fs::create_dir_all(e.repo.join(&deep)).unwrap();
    std::fs::write(e.repo.join(&deep).join("f.txt"), "x\n").unwrap();

    let mut post = call("PostToolUse", "toolu_sh1");
    post["tool_response"] = json!({"stdout": "", "stderr": "", "interrupted": false});
    let (reply, _) = e.hook("post-tool-use", "claude", post.clone());
    let text = context(&reply, "PostToolUse");
    assert!(
        text.contains("! t3:a91c edited \"src/lib.rs\" 3m ago -> re-read before editing"),
        "the same alert as an Edit: {text}"
    );
    let touches = e.stub.requests_to("POST /v1/touches");
    assert_eq!(
        touches[0].body,
        json!({
            "agent": CLAUDE_AGENT,
            "repo": "github.com/syz51/agent-board",
            "paths": ["src/main.rs", "src/lib.rs", "src/new/mod.rs"],
        })
    );

    // The start mark is used once; a shell call without one records nothing.
    e.hook("post-tool-use", "claude", post);
    e.hook("post-tool-use", "claude", call("PostToolUse", "toolu_sh2"));
    assert_eq!(e.stub.requests_to("POST /v1/touches").len(), 1);

    // A command that fails after writing a file: Claude Code sends PostToolUseFailure.
    // (The first call's files are moved out of this call's second.)
    for path in ["src/main.rs", "src/new/mod.rs"] {
        backdate(&e.repo.join(path));
    }
    e.hook("pre-tool-use", "claude", call("PreToolUse", "toolu_sh3"));
    std::fs::write(e.repo.join("src/lib.rs"), "c\n").unwrap();
    let mut failed = call("PostToolUseFailure", "toolu_sh3");
    failed["error"] = json!("Exit code 1");
    let (reply, _) = e.hook("post-tool-use-failure", "claude", failed);
    assert!(
        context(&reply, "PostToolUseFailure").contains("! t3:a91c edited"),
        "{reply:?}"
    );
    let touches = e.stub.requests_to("POST /v1/touches");
    assert_eq!(touches.len(), 2);
    assert_eq!(touches[1].body["paths"], json!(["src/lib.rs"]));
}

#[test]
fn shell_calls_send_many_changed_files_in_batches() {
    let e = env();
    e.agent_ok();
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Alerts, "", 1),
    );
    e.stub
        .on("POST /v1/touches", TouchResult { overlaps: vec![] });
    let call = |event: &str| {
        e.with(
            event,
            json!({"tool_name": "Bash", "tool_input": {"command": "gen"}, "tool_use_id": "toolu_gen"}),
        )
    };
    e.hook("pre-tool-use", "claude", call("PreToolUse"));
    for i in 0..101 {
        std::fs::write(e.repo.join(format!("src/gen_{i:03}.rs")), "x\n").unwrap();
    }
    e.hook("post-tool-use", "claude", call("PostToolUse"));
    let touches = e.stub.requests_to("POST /v1/touches");
    let sizes: Vec<usize> = touches
        .iter()
        .map(|t| t.body["paths"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, [100, 1]);
}

#[test]
fn session_start_prunes_old_shell_marks_outside_t3_and_without_login() {
    let e = env();
    let marks = e.home.join("state/tool-starts");
    let call = |id: &str| {
        e.with(
            "PreToolUse",
            json!({"tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_use_id": id}),
        )
    };
    e.hook("pre-tool-use", "claude", call("toolu_old"));
    e.hook("pre-tool-use", "claude", call("toolu_new"));
    let mut names: Vec<_> = std::fs::read_dir(&marks)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    names.sort();
    assert_eq!(names.len(), 2, "{names:?}");
    let old = names
        .iter()
        .find(|p| p.to_string_lossy().contains("toolu_5fold"))
        .unwrap();
    let file = std::fs::File::options().write(true).open(old).unwrap();
    file.set_modified(std::time::SystemTime::now() - Duration::from_secs(2 * 24 * 3600))
        .unwrap();
    // Logged out: the hook stops before the server, after pruning.
    std::fs::remove_file(e.home.join("config.toml")).unwrap();
    let (reply, _) = e.hook(
        "session-start",
        "claude",
        e.with("SessionStart", json!({"source": "startup"})),
    );
    assert_eq!(reply, None);
    let left: Vec<_> = std::fs::read_dir(&marks)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].to_string_lossy().contains("toolu_5fnew"));
}

#[test]
fn unknown_hook_events_are_ignored() {
    let e = env();
    let (reply, text) = e.hook("some-future-event", "claude", json!({}));
    assert_eq!((reply, text.as_str()), (None, ""));
    assert!(e.stub.requests().is_empty());
}

#[test]
fn codex_shell_calls_record_the_files_they_changed() {
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
    let payload = |event: &str| {
        json!({
            "session_id": "thr_123",
            "transcript_path": null,
            "cwd": e.repo,
            "hook_event_name": event,
            "model": "gpt-5.5-codex",
            "turn_id": "turn_1",
            "permission_mode": "default",
            "tool_name": "Bash",
            "tool_use_id": "call_7",
            "tool_input": {"command": "printf x > docs/notes.md"},
        })
    };
    let (_, text) = e.hook("pre-tool-use", "codex", payload("PreToolUse"));
    assert_eq!(text, "");
    std::fs::create_dir_all(e.repo.join("docs")).unwrap();
    std::fs::write(e.repo.join("docs/notes.md"), "x").unwrap();
    let mut post = payload("PostToolUse");
    post["tool_response"] = json!("x");
    let (_, text) = e.hook("post-tool-use", "codex", post);
    assert_eq!(text, "");
    let touch = &e.stub.requests_to("POST /v1/touches")[0];
    assert_eq!(touch.agent.as_deref(), Some(codex_agent));
    assert_eq!(touch.body["paths"], json!(["docs/notes.md"]));
}

#[test]
fn stop_reminds_once_about_a_stale_handoff() {
    let e = env();
    e.agent_ok();
    let mut task = task_fixture("factory-12", "Do it");
    task.status = TaskStatus::Doing;
    // The task changed after the handoff, say through `board update` in a shell call.
    task.updated_at = chrono::Utc::now();
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

/// A doing task last changed before a handoff an hour old, and that handoff.
fn quiet_doing_task(e: &Env) {
    e.agent_ok();
    let mut task = task_fixture("factory-12", "Do it");
    task.status = TaskStatus::Doing;
    e.stub.on("GET /v1/tasks", [task]);
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Delta, "", 1),
    );
    e.stub
        .on("POST /v1/touches", TouchResult { overlaps: vec![] });
    let handoff = Handoff {
        agent: CLAUDE_AGENT.parse().unwrap(),
        now: "waiting on the user".into(),
        next: "x".into(),
        blockers: String::new(),
        version: 2,
        updated_at: chrono::Utc::now() - chrono::Duration::hours(1),
    };
    e.stub.on(
        &format!("GET /v1/agents/{}/handoff", enc(CLAUDE_AGENT)),
        &handoff,
    );
}

#[test]
fn stop_skips_the_handoff_demand_when_nothing_changed_since_it() {
    let e = env();
    quiet_doing_task(&e);
    let stop = e.with(
        "Stop",
        json!({"stop_hook_active": false, "last_assistant_message": "Still waiting."}),
    );
    for prompt in ["status?", "and now?"] {
        e.hook(
            "user-prompt-submit",
            "claude",
            e.with("UserPromptSubmit", json!({"prompt": prompt})),
        );
        let read = e.with(
            "PostToolUse",
            json!({"tool_name": "mcp__board__task_show", "tool_input": {"id": "factory-12"}, "tool_use_id": "toolu_1"}),
        );
        e.hook("post-tool-use", "claude", read);
        let (_, text) = e.hook("stop", "claude", stop.clone());
        assert_eq!(text, "", "a turn that only read and replied stops quietly");
    }
}

#[test]
fn stop_demands_a_handoff_after_board_changes_or_file_edits() {
    for edits_a_file in [false, true] {
        let e = env();
        quiet_doing_task(&e);
        let call = if edits_a_file {
            json!({
                "tool_name": "Edit",
                "tool_input": {"file_path": e.repo.join("src/lib.rs"), "old_string": "a", "new_string": "b"},
                "tool_use_id": "toolu_3",
            })
        } else {
            json!({"tool_name": "mcp__board__task_note", "tool_input": {"id": "factory-12", "text": "note: x"}, "tool_use_id": "toolu_2"})
        };
        e.hook(
            "user-prompt-submit",
            "claude",
            e.with("UserPromptSubmit", json!({"prompt": "go"})),
        );
        e.hook(
            "post-tool-use",
            "claude",
            e.with("PostToolUse", call.clone()),
        );
        let stop = e.with(
            "Stop",
            json!({"stop_hook_active": false, "last_assistant_message": "Done."}),
        );
        let (reply, _) = e.hook("stop", "claude", stop);
        let reply = reply.unwrap_or_else(|| panic!("no demand after {call}"));
        assert_eq!(reply["decision"], "block", "{call}");
        assert!(reply["reason"].as_str().unwrap().contains("factory-12"));
    }
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

    // Later hooks on this device skip the server for a while instead of waiting again.
    let began = Instant::now();
    let (reply, _) = e.hook("session-start", "claude", start.clone());
    let elapsed = began.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "waited again: {elapsed:?}"
    );
    assert!(context(&reply, "SessionStart").contains("cached snapshot"));
    let marked: Value =
        serde_json::from_slice(&std::fs::read(e.home.join("state/offline.json")).unwrap()).unwrap();
    assert!(marked["until"].is_string(), "{marked}");

    // Once the backoff has passed, the server is tried again.
    std::fs::write(
        e.home.join("state/offline.json"),
        r#"{"until":"2000-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    let began = Instant::now();
    e.hook("session-start", "claude", start.clone());
    assert!(
        began.elapsed() >= Duration::from_millis(900),
        "an expired backoff waits on the server again"
    );

    // A closed port fails fast and falls back the same way.
    std::fs::remove_file(e.home.join("state/offline.json")).unwrap();
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

/// Runs a hook two `claude` processes deep, as a `claude -p` started from a T3 thread's
/// shell runs it: board <- claude <- claude <- this test. Both `claude`s are bash under
/// that name, so the real process table shows the chain.
#[cfg(unix)]
fn nested_hook(e: &Env, event: &str, payload: &Value) -> String {
    use std::io::Write as _;
    let bin = e.home.parent().unwrap().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    if !claude.exists() {
        std::os::unix::fs::symlink("/bin/bash", &claude).unwrap();
    }
    // `; exit $?` keeps bash from replacing itself with the last command.
    let inner = r#""$BOARD_BIN" hook "$EVENT" --runtime claude; exit $?"#;
    let mut child = std::process::Command::new(&claude)
        .args(["-c", r#""$CLAUDE_BIN" -c "$INNER"; exit $?"#])
        .current_dir(&e.repo)
        .env("CLAUDE_BIN", &claude)
        .env("INNER", inner)
        .env("BOARD_BIN", env!("CARGO_BIN_EXE_board"))
        .env("EVENT", event)
        .env("BOARD_HOME", &e.home)
        .env("BOARD_TOKEN", TOKEN)
        .env("BOARD_KEYCHAIN", "0")
        .env("T3CODE_THREAD_ID", "thread-7")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("BOARD_LOG")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    stdout(&out)
}

#[cfg(unix)]
#[test]
fn a_session_nested_in_a_t3_thread_keeps_its_own_identity() {
    let e = env();
    let t3 = "t3:thread-7";
    for agent in [CLAUDE_AGENT, t3] {
        e.stub.on(
            &format!("POST /v1/agents/{}/state", enc(agent)),
            agent_fixture(agent, DEVICE, AgentState::Running),
        );
    }
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(CLAUDE_AGENT, DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
    );
    let t3_env = [("T3CODE_THREAD_ID", "thread-7")];
    let cache = e
        .home
        .join(format!("state/sessions/claude-{}.json", SESSION));

    // Nested: the session registers as itself, without the thread it runs inside.
    nested_hook(&e, "session-start", &e.base("SessionStart"));
    let register = &e.stub.requests_to("POST /v1/agents")[0];
    assert_eq!(register.body["id"], CLAUDE_AGENT);
    assert!(register.body["native_ids"].get("t3_thread").is_none());
    assert_eq!(register.body["native_ids"]["claude_session"], SESSION);
    let cached: Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
    assert_eq!(cached, json!({"nested": true}));

    // Later hooks of the session use the cached decision, wherever they run.
    e.stub.clear();
    e.hook_env("stop", "claude", e.base("Stop"), &t3_env);
    assert_eq!(e.stub.requests()[0].agent.as_deref(), Some(CLAUDE_AGENT));

    // SessionEnd drops the cache.
    e.stub.clear();
    e.hook_env("session-end", "claude", e.base("SessionEnd"), &t3_env);
    assert_eq!(e.stub.requests()[0].agent.as_deref(), Some(CLAUDE_AGENT));
    assert!(!cache.exists());

    // SessionStart decides again: run directly, the session is the T3 thread.
    e.stub.clear();
    e.hook_env("session-start", "claude", e.base("SessionStart"), &t3_env);
    let register = &e.stub.requests_to("POST /v1/agents")[0];
    assert_eq!(register.body["id"], t3);
    assert_eq!(register.body["native_ids"]["t3_thread"], "thread-7");
    let cached: Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
    assert_eq!(cached, json!({"nested": false}));
}
