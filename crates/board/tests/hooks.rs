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
        json!({"agent": CLAUDE_AGENT, "repo": "github.com/syz51/agent-board", "paths": ["src/lib.rs"]}),
        "an edit tool's paths are not scanned; a missing file has no time"
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
fn edit_tool_touches_carry_the_files_modification_time() {
    let e = env();
    e.agent_ok();
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Alerts, "", 1),
    );
    e.stub
        .on("POST /v1/touches", TouchResult { overlaps: vec![] });
    std::fs::create_dir_all(e.repo.join("src")).unwrap();
    std::fs::write(e.repo.join("src/lib.rs"), "b\n").unwrap();
    let edit = e.with(
        "PostToolUse",
        json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": e.repo.join("src/lib.rs"), "old_string": "a", "new_string": "b"},
            "tool_use_id": "toolu_ed1",
        }),
    );
    e.hook("post-tool-use", "claude", edit);
    assert_eq!(
        e.stub.requests_to("POST /v1/touches")[0].body,
        json!({
            "agent": CLAUDE_AGENT,
            "repo": "github.com/syz51/agent-board",
            "paths": ["src/lib.rs"],
            "modified": {"src/lib.rs": mtime(&e.repo.join("src/lib.rs"))},
        })
    );
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
    std::fs::create_dir_all(e.repo.join("src")).unwrap();
    std::fs::write(e.repo.join("src/a.rs"), "y\n").unwrap();
    // Run from another directory: the patch's relative paths are the payload cwd's.
    let elsewhere = e.home.parent().unwrap();
    let out = board(
        &e.home,
        elsewhere,
        &[],
        &["hook", "post-tool-use", "--runtime", "codex"],
        &payload.to_string(),
    );
    assert_eq!(stdout(&out), "");
    let touch = &e.stub.requests_to("POST /v1/touches")[0];
    assert_eq!(touch.agent.as_deref(), Some(codex_agent));
    assert_eq!(touch.body["paths"], json!(["src/a.rs", "docs/b.md"]));
    assert_eq!(
        touch.body["modified"],
        json!({"src/a.rs": mtime(&e.repo.join("src/a.rs"))}),
        "the patched file's time; the added one is not on disk here"
    );
}

/// A file's modification time as the hook reports it.
fn mtime(path: &std::path::Path) -> chrono::DateTime<chrono::Utc> {
    std::fs::symlink_metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .into()
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
    let mut body = touches[0].body.clone();
    let modified = body.as_object_mut().unwrap().remove("modified").unwrap();
    assert_eq!(
        body,
        json!({
            "agent": CLAUDE_AGENT,
            "repo": "github.com/syz51/agent-board",
            "paths": ["src/main.rs", "src/lib.rs", "src/new/mod.rs"],
            "scanned": true,
        })
    );
    // Each path's modification time, so the server can tell a peer's edit from this call's.
    for path in ["src/main.rs", "src/lib.rs", "src/new/mod.rs"] {
        assert_eq!(modified[path], json!(mtime(&e.repo.join(path))), "{path}");
    }

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
fn shell_calls_leave_no_mark_without_login_token_or_agent() {
    let e = env();
    let marks = e.home.join("state/tool-starts");
    let count = || std::fs::read_dir(&marks).map_or(0, |dir| dir.count());
    let call = |event: &str, id: &str| {
        e.with(
            event,
            json!({"tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_use_id": id}),
        )
    };

    // No token: PostToolUse stops before the server but still takes the mark.
    let no_token = [("BOARD_TOKEN", "")];
    e.hook_env(
        "pre-tool-use",
        "claude",
        call("PreToolUse", "toolu_a"),
        &no_token,
    );
    assert_eq!(count(), 1, "logged in: PreToolUse marks the call");
    e.hook_env(
        "post-tool-use",
        "claude",
        call("PostToolUse", "toolu_a"),
        &no_token,
    );
    assert_eq!(count(), 0, "taken without a token");

    // No agent (no session id): the same, for a failed call too.
    let mut no_agent = call("PreToolUse", "toolu_b");
    no_agent.as_object_mut().unwrap().remove("session_id");
    e.hook("pre-tool-use", "claude", no_agent.clone());
    no_agent["hook_event_name"] = json!("PostToolUseFailure");
    e.hook("post-tool-use-failure", "claude", no_agent);
    assert_eq!(count(), 0, "taken without an agent");

    // Logged out: no mark at all.
    std::fs::remove_file(e.home.join("config.toml")).unwrap();
    e.hook("pre-tool-use", "claude", call("PreToolUse", "toolu_c"));
    assert_eq!(count(), 0, "logged out: nothing to report the call to");
    assert!(e.stub.requests().is_empty());
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

/// Runs `git` in `repo`, as a test user.
fn git(repo: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .unwrap()
}

/// Writes `files` in `repo` an hour ago and commits them on its branch.
fn commit_files(repo: &std::path::Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = repo.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        backdate(&path);
    }
    commit_all(repo);
}

/// Runs one shell call through PreToolUse and PostToolUse, with `during` as the command,
/// and returns the paths it reported (`None`: no touch was sent).
fn shell_call(e: &Env, id: &str, during: impl FnOnce()) -> Option<Value> {
    let call = |event: &str| {
        e.with(
            event,
            json!({"tool_name": "Bash", "tool_input": {"command": "git ..."}, "tool_use_id": id}),
        )
    };
    let before = e.stub.requests_to("POST /v1/touches").len();
    e.hook("pre-tool-use", "claude", call("PreToolUse"));
    during();
    e.hook("post-tool-use", "claude", call("PostToolUse"));
    let touches = e.stub.requests_to("POST /v1/touches");
    (touches.len() > before).then(|| touches.last().unwrap().body["paths"].clone())
}

#[test]
fn shell_calls_leave_out_the_files_git_brought_in() {
    let e = env();
    e.agent_ok();
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Alerts, "", 1),
    );
    e.stub
        .on("POST /v1/touches", TouchResult { overlaps: vec![] });
    commit_files(&e.repo, &[("src/a.rs", "a\n"), ("src/b.rs", "b\n")]);
    // Another branch changes a.rs and adds c.rs.
    assert!(
        git(&e.repo, &["switch", "-q", "-c", "other"])
            .status
            .success()
    );
    commit_files(&e.repo, &[("src/a.rs", "a2\n"), ("src/c.rs", "c\n")]);
    assert!(
        git(&e.repo, &["switch", "-q", "feature/x"])
            .status
            .success()
    );
    for path in ["src/a.rs", "src/b.rs"] {
        backdate(&e.repo.join(path));
    }

    // A checkout that merges a local change into a file the other branch changed: HEAD
    // moves, and git writes the conflict into a.rs.
    std::fs::write(e.repo.join("src/a.rs"), "a-local\n").unwrap();
    backdate(&e.repo.join("src/a.rs"));
    let paths = shell_call(&e, "toolu_switch", || {
        let switch = git(&e.repo, &["checkout", "-q", "-m", "other"]);
        assert!(
            switch.status.success(),
            "{}",
            String::from_utf8_lossy(&switch.stderr)
        );
        std::fs::write(e.repo.join("src/mine.rs"), "mine\n").unwrap();
    });
    let status = String::from_utf8(git(&e.repo, &["status", "--porcelain"]).stdout).unwrap();
    assert!(status.contains("UU src/a.rs"), "{status}");
    assert_eq!(
        paths,
        Some(json!(["src/mine.rs"])),
        "HEAD moved: only the call's own file"
    );
    // Back to a clean feature/x.
    assert!(
        git(&e.repo, &["checkout", "-q", "-f", "feature/x"])
            .status
            .success()
    );
    std::fs::remove_file(e.repo.join("src/mine.rs")).unwrap();

    // A conflicted merge stages what it brings in without moving HEAD. This branch changed
    // b.rs too, so b.rs differs from the merged branch, but the merge did not write it.
    commit_files(
        &e.repo,
        &[("src/a.rs", "a-mine\n"), ("src/b.rs", "b-mine\n")],
    );
    let paths = shell_call(&e, "toolu_merge", || {
        let merge = git(&e.repo, &["merge", "--no-edit", "other"]);
        assert!(!merge.status.success(), "the merge stops on a conflict");
        std::fs::write(e.repo.join("src/b.rs"), "b-edited\n").unwrap();
    });
    let status = String::from_utf8(git(&e.repo, &["status", "--porcelain"]).stdout).unwrap();
    assert!(
        status.contains("UU src/a.rs") && status.contains("A  src/c.rs"),
        "{status}"
    );
    assert_eq!(
        paths,
        Some(json!(["src/b.rs"])),
        "neither the conflicted nor the merged file"
    );

    // A merge stopped before the call is the agent's to finish: what it resolves counts.
    let git_dir = e.repo.join(".git");
    for path in [
        git_dir.join("MERGE_HEAD"),
        e.repo.join("src/b.rs"),
        e.repo.join("src/c.rs"),
    ] {
        backdate(&path);
    }
    let paths = shell_call(&e, "toolu_resolve", || {
        std::fs::write(e.repo.join("src/a.rs"), "a-resolved\n").unwrap();
    });
    assert_eq!(paths, Some(json!(["src/a.rs"])));

    // A rebase that stops on a conflict moves HEAD and stages the stopped commit.
    assert!(git(&e.repo, &["reset", "-q", "--hard"]).status.success());
    let paths = shell_call(&e, "toolu_rebase", || {
        assert!(!git(&e.repo, &["rebase", "other"]).status.success());
    });
    let status = String::from_utf8(git(&e.repo, &["status", "--porcelain"]).stdout).unwrap();
    assert!(status.contains("src/a.rs"), "{status}");
    assert_eq!(paths, None, "nothing of the call's own");

    // HEAD moved by the call's own commits: what it committed and then edited counts.
    assert!(git(&e.repo, &["rebase", "--abort"]).status.success());
    let paths = shell_call(&e, "toolu_commit", || {
        std::fs::write(e.repo.join("src/b.rs"), "b-committed\n").unwrap();
        assert!(git(&e.repo, &["commit", "-qam", "mine"]).status.success());
        std::fs::write(e.repo.join("src/b.rs"), "b-amended\n").unwrap();
        assert!(
            git(&e.repo, &["commit", "-qa", "--amend", "--no-edit"])
                .status
                .success()
        );
        std::fs::write(e.repo.join("src/b.rs"), "b-after\n").unwrap();
    });
    assert_eq!(paths, Some(json!(["src/b.rs"])));
}

/// A task in doing, a delta snapshot and a handoff written an hour ago, with the agent's
/// last change dated by the server (`None`: a server that does not date changes).
fn doing_with_handoff(e: &Env, last_change_at: Option<chrono::DateTime<chrono::Utc>>) -> Handoff {
    e.agent_ok();
    let mut task = task_fixture("factory-12", "Do it");
    task.status = TaskStatus::Doing;
    e.stub.on("GET /v1/tasks", [task]);
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Delta, "", 1),
    );
    let handoff = Handoff {
        agent: CLAUDE_AGENT.parse().unwrap(),
        now: "waiting on the user".into(),
        next: "x".into(),
        blockers: String::new(),
        version: 2,
        updated_at: chrono::Utc::now() - chrono::Duration::hours(1),
        last_change_at,
    };
    e.stub.on(&handoff_route(), &handoff);
    handoff
}

fn handoff_route() -> String {
    format!("GET /v1/agents/{}/handoff", enc(CLAUDE_AGENT))
}

fn prompt(e: &Env, text: &str) {
    e.hook(
        "user-prompt-submit",
        "claude",
        e.with("UserPromptSubmit", json!({"prompt": text})),
    );
}

fn stop(e: &Env) -> Option<Value> {
    let stop = e.with(
        "Stop",
        json!({"stop_hook_active": false, "last_assistant_message": "Done."}),
    );
    e.hook("stop", "claude", stop).0
}

#[test]
fn stop_stays_quiet_when_nothing_changed_since_the_handoff() {
    let e = env();
    let mut h = doing_with_handoff(&e, None);
    h.last_change_at = Some(h.updated_at - chrono::Duration::minutes(5));
    e.stub.on(&handoff_route(), &h);
    for text in ["status?", "and now?"] {
        prompt(&e, text);
        assert_eq!(
            stop(&e),
            None,
            "a turn that only read or replied stops quietly"
        );
    }
    assert_eq!(
        e.stub.requests_to(&state_path()).last().unwrap().body["state"],
        "idle"
    );
}

#[test]
fn stop_reminds_once_per_change_made_after_the_handoff() {
    let e = env();
    let mut h = doing_with_handoff(&e, None);
    h.last_change_at = Some(h.updated_at + chrono::Duration::minutes(1));
    e.stub.on(&handoff_route(), &h);
    prompt(&e, "go");
    let reply = stop(&e).expect("a change after the handoff gets the demand");
    assert_eq!(reply["decision"], "block");
    let reason = reply["reason"].as_str().unwrap();
    assert!(
        reason.contains("factory-12")
            && reason.contains("predates your last board change")
            && reason.contains("handoff_set"),
        "{reason}"
    );
    assert_eq!(stop(&e), None, "one reminder per change");
    let continued = e.with("Stop", json!({"stop_hook_active": true}));
    assert_eq!(e.hook("stop", "claude", continued).1, "");

    // A later change in the same turn (say, after a background task woke the agent).
    h.last_change_at = Some(h.updated_at + chrono::Duration::minutes(2));
    e.stub.on(&handoff_route(), &h);
    assert_eq!(stop(&e).unwrap()["decision"], "block");
}

#[test]
fn stop_asks_for_a_first_handoff() {
    let e = env();
    doing_with_handoff(&e, None);
    e.stub.on_error(
        &handoff_route(),
        404,
        "not_found",
        "no handoff",
        "write one",
    );
    prompt(&e, "go");
    let reply = stop(&e).unwrap();
    assert!(
        reply["reason"]
            .as_str()
            .unwrap()
            .contains("you have no handoff"),
        "{reply}"
    );
    assert_eq!(stop(&e), None, "one reminder per turn");
}

#[test]
fn stop_falls_back_to_the_turn_against_a_server_without_change_dates() {
    let e = env();
    doing_with_handoff(&e, None);
    prompt(&e, "go");
    let reply = stop(&e).unwrap();
    assert!(
        reply["reason"]
            .as_str()
            .unwrap()
            .contains("older than this turn"),
        "{reply}"
    );
    assert_eq!(stop(&e), None, "one reminder per turn");
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

#[cfg(unix)]
#[test]
fn a_delegated_t3_thread_registers_its_parent() {
    use std::os::unix::fs::PermissionsExt as _;
    const CHILD: &str = "thread:delegated-task:command%3Amcp%3Ac1%3Adelegate-task%3At1";
    let e = env();
    // A `t3` that answers `thread show` as T3 does and logs each call.
    let log = e.home.join("t3.log");
    let t3 = e.home.join("t3");
    std::fs::write(
        &t3,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nprintf '%s' '{}'\n",
            log.display(),
            json!({"thread": {
                "threadId": CHILD,
                "parentThreadId": "parent-1",
                "relationshipToParent": "subagent",
            }})
        ),
    )
    .unwrap();
    std::fs::set_permissions(&t3, std::fs::Permissions::from_mode(0o755)).unwrap();
    let t3 = t3.to_str().unwrap();
    // The child starts twice (a resume): the second start reuses the parent it found.
    let starts = [
        (CHILD, Some("parent-1")),
        (CHILD, Some("parent-1")),
        ("thread-7", None),
    ];
    for (thread, parent) in starts {
        let agent = format!("t3:{thread}");
        e.stub.on(
            "POST /v1/agents",
            agent_fixture(&agent, DEVICE, AgentState::Running),
        );
        e.stub.on(
            "GET /v1/snapshot",
            snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
        );
        e.hook_env(
            "session-start",
            "claude",
            e.with("SessionStart", json!({"source": "startup"})),
            &[("T3CODE_THREAD_ID", thread), ("T3CODE_CLI_PATH", t3)],
        );
        let register = e.stub.requests_to("POST /v1/agents").pop().unwrap();
        assert_eq!(register.body["id"], agent);
        assert_eq!(register.body["native_ids"]["t3_thread"], thread);
        assert_eq!(
            register.body["native_ids"]["t3_parent"].as_str(),
            parent,
            "{thread}"
        );
    }
    // Only the delegated thread asked T3, and only once.
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls, format!("thread show --json --messages 0 {CHILD}\n"));
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
