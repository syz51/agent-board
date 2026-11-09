//! `board hook` with recorded Claude Code payloads (shapes from
//! code.claude.com/docs/en/hooks) and Codex payloads (learn.chatgpt.com/docs/hooks.md).

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use board_proto::{
    AgentState, Handoff, Overlap, SAID_CHARS_MAX, SnapshotKind, TaskStatus, TouchResult,
};
use common::*;
use serde_json::{Value, json};

const SESSION: &str = "00893aaf-19fa-41d2-8238-13269b9b3ca0";
const CLAUDE_AGENT: &str = "claude:00893aaf-19fa-41d2-8238-13269b9b3ca0";
const SNAPSHOT: &str =
    "<board agent=\"claude:0089\" board=\"factory\" seq=\"812\">\nyou: claude:0089\n</board>";
/// What Claude's `SessionStart` adds after the snapshot: Claude Code defers MCP tool schemas.
const TOOL_SEARCH: &str = "If the board tools show by name only, load them in one call before the first board call: ToolSearch query \"select:mcp__board__board_snapshot,mcp__board__task_claim,mcp__board__task_add,mcp__board__task_note,mcp__board__handoff_set,mcp__board__task_update,mcp__board__task_done,mcp__board__task_show,mcp__board__lease_paths,mcp__board__feedback,mcp__board__ask_user\" max_results 11";

/// Claude's `SessionStart` context for [`SNAPSHOT`].
fn started() -> String {
    format!("{SNAPSHOT}\n{TOOL_SEARCH}")
}

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
        let text = stdout(&self.hook_output(event, runtime, payload, env));
        (serde_json::from_str(&text).ok(), text)
    }

    /// Runs a hook and returns its output, stderr included.
    fn hook_output(
        &self,
        event: &str,
        runtime: &str,
        payload: Value,
        env: &[(&str, &str)],
    ) -> std::process::Output {
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
        out
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
    assert_eq!(context(&reply, "SessionStart"), started());

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
fn codex_session_start_injects_the_snapshot_without_tool_search() {
    let e = env();
    e.stub.on(
        "POST /v1/agents",
        agent_fixture("codex:thr_9", DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
    );
    let payload = json!({
        "session_id": "thr_9", "transcript_path": null, "cwd": e.repo,
        "hook_event_name": "SessionStart", "model": "gpt-5.5-codex", "source": "startup",
    });
    let (reply, _) = e.hook("session-start", "codex", payload);
    assert_eq!(
        context(&reply, "SessionStart"),
        SNAPSHOT,
        "Codex has no ToolSearch; the MCP instructions name the tools"
    );
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
    let (hint, load) = hint.split_once('\n').unwrap();
    assert!(
        hint.contains("board factory (active)") && hint.contains("board_join factory"),
        "{hint}"
    );
    assert_eq!(
        load,
        TOOL_SEARCH
            .replace("select:", "select:mcp__board__board_join,")
            .replace("max_results 11", "max_results 12"),
        "board_join is loaded first"
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
    let before = e.stub.connections();
    let (reply, _) = e.hook("user-prompt-submit", "claude", prompt.clone());
    assert!(context(&reply, "UserPromptSubmit").contains("alerts: x"));
    // The heartbeat and the snapshot share one connection (one TLS handshake on a server).
    assert_eq!(e.stub.connections() - before, 1);
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
    let (_, text) = e.hook("post-tool-use", "claude", edit);
    assert_eq!(
        text, "",
        "the same overlap is not shown again on the next call"
    );
    assert_eq!(e.stub.requests_to("POST /v1/touches").len(), 2);
    assert_eq!(
        e.stub.requests_to(&state_path()).len(),
        1,
        "heartbeat throttled to once a minute"
    );
    assert_eq!(e.stub.requests_to("GET /v1/snapshot").len(), 1);
}

#[test]
fn post_tool_use_with_nothing_to_send_skips_the_server_and_the_keychain() {
    let e = env();
    e.agent_ok();
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Alerts, "", 20),
    );
    e.stub
        .on("POST /v1/touches", TouchResult { overlaps: vec![] });
    // The token comes from the Keychain, through a `security` that logs each run and finds
    // nothing, then from the file.
    let bin = e.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = e.home.join("security.log");
    let security = bin.join("security");
    common::script::write_script(
        &security,
        &format!("#!/bin/sh\necho run >> '{}'\nexit 44\n", log.display()),
    );
    std::fs::write(e.home.join("token"), format!("{TOKEN}\n")).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let keychain = [
        ("PATH", path.as_str()),
        ("BOARD_TOKEN", ""),
        ("BOARD_KEYCHAIN", "1"),
    ];
    let runs = || {
        std::fs::read_to_string(&log)
            .map(|t| t.lines().count())
            .unwrap_or(0)
    };
    let post = |tool: Value| {
        let (_, text) = e.hook_env(
            "post-tool-use",
            "claude",
            e.with("PostToolUse", tool),
            &keychain,
        );
        text
    };

    // The first call in a minute heartbeats.
    post(json!({"tool_name": "Read", "tool_input": {"file_path": "/x"}}));
    assert_eq!(e.stub.requests_to(&state_path()).len(), 1);
    let looked_up = runs();
    if cfg!(target_os = "macos") {
        assert_eq!(looked_up, 1, "the heartbeat needs the token");
    }

    // Within the minute, a read and a shell call that changed nothing send nothing.
    e.stub.clear();
    assert_eq!(
        post(json!({"tool_name": "Read", "tool_input": {"file_path": "/x"}})),
        ""
    );
    let shell =
        json!({"tool_name": "Bash", "tool_use_id": "toolu_quiet", "tool_input": {"command": "ls"}});
    e.hook_env(
        "pre-tool-use",
        "claude",
        e.with("PreToolUse", shell.clone()),
        &keychain,
    );
    assert_eq!(post(shell), "");
    assert!(e.stub.requests().is_empty(), "{:?}", e.stub.requests());
    assert_eq!(
        runs(),
        looked_up,
        "no Keychain lookup without a server call"
    );

    // An edit still reports its file.
    std::fs::write(e.repo.join("README.md"), "x").unwrap();
    post(json!({"tool_name": "Write", "tool_input": {"file_path": e.repo.join("README.md")}}));
    assert_eq!(e.stub.requests_to("POST /v1/touches").len(), 1);
    assert_eq!(e.stub.requests_to(&state_path()).len(), 0);
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
    // Another peer: the first one's alert was already shown.
    e.stub.on(
        "POST /v1/touches",
        TouchResult {
            overlaps: vec![Overlap {
                path: "src/lib.rs".into(),
                agent: "t3:b22d".parse().unwrap(),
                handle: "t3:b22d".into(),
                via: "touch".into(),
                at: chrono::Utc::now() - chrono::Duration::minutes(1),
            }],
        },
    );
    let mut failed = call("PostToolUseFailure", "toolu_sh3");
    failed["error"] = json!("Exit code 1");
    let (reply, _) = e.hook("post-tool-use-failure", "claude", failed);
    assert!(
        context(&reply, "PostToolUseFailure").contains("! t3:b22d edited"),
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
fn session_start_prunes_agent_files_untouched_for_a_week() {
    let (e, start) = cached_env();
    let backdate = |path: &std::path::Path, days: u64| {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(days * 24 * 3600))
            .unwrap();
    };
    let gone = ["t3_3agone.json", "codex_3agone.json"];
    let recent = "claude_3arecent.json";
    for dir in ["state", "cache"] {
        for name in gone.iter().chain([&recent]) {
            std::fs::write(e.home.join(dir).join(name), "{}").unwrap();
        }
        for name in gone {
            backdate(&e.home.join(dir).join(name), 8);
        }
        backdate(&e.home.join(dir).join(recent), 6);
    }
    // Named state files are not an agent's, however old.
    std::fs::write(e.home.join("state/watch.json"), "{}").unwrap();
    backdate(&e.home.join("state/watch.json"), 30);

    let (reply, _) = e.hook("session-start", "claude", start);
    assert_eq!(context(&reply, "SessionStart"), started());
    for dir in ["state", "cache"] {
        let mut left: Vec<String> = std::fs::read_dir(e.home.join(dir))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".json"))
            .collect();
        left.sort();
        // This agent's own files, written by the SessionStart that pruned.
        let mut want = vec![recent.to_owned(), format!("claude_3a{SESSION}.json")];
        if dir == "state" {
            want.push("watch.json".into());
        }
        want.sort();
        assert_eq!(left, want, "{dir}");
    }
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
    let state = e
        .stub
        .requests_to(&state_path())
        .last()
        .unwrap()
        .body
        .clone();
    assert_eq!(
        (&state["state"], &state["said"]),
        (&json!("idle"), &json!("Done."))
    );
}

#[test]
fn stop_sends_the_turns_last_message_cleaned_and_cut() {
    let e = env();
    let mut h = doing_with_handoff(&e, None);
    h.last_change_at = Some(h.updated_at - chrono::Duration::minutes(5));
    e.stub.on(&handoff_route(), &h);
    let said = |message: Value| {
        let stop = e.with(
            "Stop",
            json!({"stop_hook_active": false, "last_assistant_message": message}),
        );
        assert_eq!(e.hook("stop", "claude", stop).0, None);
        e.stub
            .requests_to(&state_path())
            .last()
            .unwrap()
            .body
            .get("said")
            .cloned()
    };
    assert_eq!(
        said(json!("  \u{1b}[1mPR open\u{1b}[0m:\n\t- review it\r\n  ")),
        Some(json!("PR open:\n\t- review it"))
    );
    let long = said(json!("y".repeat(SAID_CHARS_MAX + 10))).unwrap();
    let long = long.as_str().unwrap();
    assert_eq!(long.chars().count(), SAID_CHARS_MAX);
    assert!(long.ends_with("y…"), "{long}");
    assert_eq!(said(json!(" \u{7} ")), None, "nothing left to keep");
    assert_eq!(said(Value::Null), None, "a runtime without the field");
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
    let blocked = e
        .stub
        .requests_to(&state_path())
        .last()
        .unwrap()
        .body
        .clone();
    assert_eq!(
        (blocked["state"].as_str(), blocked["said"].as_str()),
        (Some("idle"), Some("Done.")),
        "the reminder interrupts the turn's real closing message: keep it"
    );
    let reason = reply["reason"].as_str().unwrap();
    assert!(
        reason.contains("factory-12")
            && reason.contains("predates your last board change")
            && reason.contains("handoff_set"),
        "{reason}"
    );
    assert_eq!(stop(&e), None, "one reminder per change");
    let continued = e.with(
        "Stop",
        json!({"stop_hook_active": true, "last_assistant_message": "Handoff updated."}),
    );
    assert_eq!(e.hook("stop", "claude", continued).1, "");
    let after = e
        .stub
        .requests_to(&state_path())
        .last()
        .unwrap()
        .body
        .clone();
    assert_eq!(
        (after["state"].as_str(), after.get("said")),
        (Some("idle"), None),
        "the reply to the reminder is not what the user should read"
    );

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

/// Serves snapshots that say whether a paused board holds the agent.
fn serve_hold(e: &Env, held: bool) {
    let mut snap = snapshot_fixture(SnapshotKind::Delta, "", 2);
    if held {
        snap.text = "<board>\nhold: factory paused -> stop\n</board>".into();
        snap.held_by = vec!["factory".parse().unwrap()];
    }
    e.stub.on("GET /v1/snapshot", snap);
}

/// Serves snapshots that say a board paused at `since` holds the agent.
fn serve_hold_since(e: &Env, since: chrono::DateTime<chrono::Utc>) {
    let mut snap = snapshot_fixture(SnapshotKind::Alerts, "", 2);
    snap.text = "<board>\nhold: factory paused -> stop\n</board>".into();
    snap.held_by = vec!["factory".parse().unwrap()];
    snap.held_since = Some(since);
    e.stub.on("GET /v1/snapshot", snap);
}

#[test]
fn a_turn_begun_after_the_pause_goes_on_when_it_claims_the_boards_work() {
    let e = env();
    let mut h = doing_with_handoff(&e, None);
    h.last_change_at = Some(h.updated_at + chrono::Duration::minutes(1));
    e.stub.on(&handoff_route(), &h);
    // The board was paused an hour ago; the agent works for it only once it claims a task.
    serve_hold(&e, false);
    prompt(&e, "work on the paused board");
    serve_hold_since(&e, chrono::Utc::now() - chrono::Duration::hours(1));
    let (reply, _) = read_tool(&e);
    assert!(
        reply.as_ref().is_none_or(|r| r.get("continue").is_none()),
        "{reply:?}"
    );
    assert_eq!(
        stop(&e).unwrap()["decision"],
        "block",
        "the user's turn gets its reminders"
    );

    // A pause after the turn began still ends it.
    serve_hold(&e, false);
    prompt(&e, "go on");
    serve_hold_since(&e, chrono::Utc::now() + chrono::Duration::seconds(5));
    heartbeat_due_again(&e);
    let (reply, _) = read_tool(&e);
    assert_eq!(reply.expect("a halt")["continue"], false);
}

/// Lets the next `PostToolUse` run its heartbeat, as a minute later.
fn heartbeat_due_again(e: &Env) {
    let path = e.home.join("state").join(format!(
        "{}.json",
        board::state::file_stem(&CLAUDE_AGENT.parse().unwrap())
    ));
    let mut local: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    local.as_object_mut().unwrap().remove("last_heartbeat");
    std::fs::write(&path, local.to_string()).unwrap();
}

fn read_tool(e: &Env) -> (Option<Value>, String) {
    e.hook(
        "post-tool-use",
        "claude",
        e.with(
            "PostToolUse",
            json!({"tool_name": "Read", "tool_input": {"file_path": "/x"}}),
        ),
    )
}

#[test]
fn a_paused_board_stops_a_running_turn_and_only_tells_a_new_one() {
    let e = env();
    let mut h = doing_with_handoff(&e, None);
    h.last_change_at = Some(h.updated_at + chrono::Duration::minutes(1));
    e.stub.on(&handoff_route(), &h);
    serve_hold(&e, false);
    prompt(&e, "go");

    // The board is paused while the turn runs: its next heartbeat ends it.
    serve_hold(&e, true);
    let (reply, _) = read_tool(&e);
    let reply = reply.expect("a halt");
    assert_eq!(reply["continue"], false, "{reply}");
    let reason = reply["stopReason"].as_str().unwrap();
    assert!(
        reason.contains("factory paused") && reason.contains("board resume"),
        "{reason}"
    );
    assert_eq!(
        e.stub.requests_to(&state_path()).last().unwrap().body["state"],
        "idle"
    );
    // A runtime that went on anyway (a subagent's tool call) is halted again.
    heartbeat_due_again(&e);
    let (reply, _) = read_tool(&e);
    assert_eq!(reply.expect("a second halt")["continue"], false);
    assert_eq!(
        stop(&e),
        None,
        "no handoff reminder in a halted turn: it would continue the turn"
    );

    // A turn the user starts while held is told, not stopped.
    let (reply, _) = e.hook(
        "user-prompt-submit",
        "claude",
        e.with("UserPromptSubmit", json!({"prompt": "fix the typo anyway"})),
    );
    assert!(context(&reply, "UserPromptSubmit").contains("hold: factory paused"));
    heartbeat_due_again(&e);
    let (reply, _) = read_tool(&e);
    assert!(
        reply.as_ref().is_none_or(|r| r.get("continue").is_none()),
        "{reply:?}"
    );
    assert_eq!(
        stop(&e).unwrap()["decision"],
        "block",
        "the user's own turn gets its reminders"
    );

    // Resumed: no hold line and no halt.
    serve_hold(&e, false);
    prompt(&e, "go on");
    heartbeat_due_again(&e);
    assert_eq!(read_tool(&e).0, None);
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

/// Marks `id` done as the agent would from its shell (`board done`), with the identity the
/// hooks resolve: `env` names it, or `agent` passes it explicitly.
fn mark_done(e: &Env, id: &str, agent: Option<&str>) {
    let detail = board_proto::TaskDetail {
        task: task_fixture(id, "Do it"),
        notes: vec![],
        evidence: vec![],
        blocks: vec![],
        children: vec![],
        child_tasks: vec![],
        history: vec![],
        handles: Default::default(),
    };
    e.stub.on(&format!("GET /v1/tasks/{id}"), &detail);
    e.stub.on(
        &format!("POST /v1/tasks/{id}/done"),
        task_fixture(id, "Do it"),
    );
    let mut args = vec!["done", id, "--check", "cargo test", "--exit", "0"];
    let env: &[(&str, &str)] = match agent {
        Some(agent) => {
            e.stub.on(
                &format!("GET /v1/agents/{}", enc(agent)),
                agent_fixture(agent, DEVICE, AgentState::Running),
            );
            args.extend(["--agent", agent]);
            &[]
        }
        None => {
            e.stub.on(
                &format!("GET /v1/agents/{}", enc(CLAUDE_AGENT)),
                agent_fixture(CLAUDE_AGENT, DEVICE, AgentState::Running),
            );
            &[("CLAUDE_CODE_SESSION_ID", SESSION)]
        }
    };
    let out = board(&e.home, &e.repo, env, &args, "");
    assert!(out.status.success(), "{}", stderr(&out));
}

/// No task in doing, so only the done reminder can speak.
fn nothing_doing(e: &Env) {
    e.agent_ok();
    e.stub.on("GET /v1/tasks", Vec::<board_proto::Task>::new());
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Delta, "", 1),
    );
}

#[test]
fn stop_reminds_once_per_task_marked_done_this_turn() {
    let e = env();
    nothing_doing(&e);
    prompt(&e, "go");
    assert_eq!(stop(&e), None, "no reminder without a done task");

    prompt(&e, "finish it");
    mark_done(&e, "factory-12", None);
    let reply = stop(&e).expect("a done task gets the reminder");
    assert_eq!(reply["decision"], "block");
    assert_eq!(
        reply["reason"],
        "board: you marked factory-12 done this turn. Before you stop, file anything still to do: sub-tasks for its scope, Proposal: tasks for found work, a blocked task with an ask for the user's part; name them in your reply."
    );
    assert_eq!(stop(&e), None, "once per task");

    // Reopened and done again in a later turn: still once per task. Another task speaks.
    prompt(&e, "redo it");
    mark_done(&e, "factory-12", None);
    mark_done(&e, "factory-13", None);
    let reason = stop(&e).unwrap()["reason"].as_str().unwrap().to_owned();
    assert!(
        reason.starts_with("board: you marked factory-13 done this turn."),
        "{reason}"
    );
}

#[test]
fn stop_stays_quiet_about_dones_while_stop_hook_active_and_from_earlier_turns() {
    let e = env();
    nothing_doing(&e);
    prompt(&e, "go");
    mark_done(&e, "factory-12", None);
    let continued = e.with("Stop", json!({"stop_hook_active": true}));
    assert_eq!(e.hook("stop", "claude", continued).1, "");
    assert_eq!(
        stop(&e),
        None,
        "the continued turn's done is not carried over"
    );

    // A turn whose Stop never ran (interrupted) leaves its mark to the next turn.
    mark_done(&e, "factory-13", None);
    prompt(&e, "next");
    assert_eq!(stop(&e), None, "a done from an earlier turn");
}

#[test]
fn stop_puts_the_done_and_handoff_reminders_in_one_block() {
    let e = env();
    doing_with_handoff(&e, None);
    prompt(&e, "go");
    mark_done(&e, "factory-11", None);
    let reason = stop(&e).unwrap()["reason"].as_str().unwrap().to_owned();
    let (done, handoff) = reason.split_once('\n').expect("two lines");
    assert!(
        done.starts_with("board: you marked factory-11 done"),
        "{reason}"
    );
    assert!(
        handoff.starts_with("board: factory-12 is in doing") && handoff.contains("handoff_set"),
        "{reason}"
    );
}

#[test]
fn stop_reminds_about_a_done_task_while_the_server_fails() {
    let e = env();
    nothing_doing(&e);
    prompt(&e, "go");
    mark_done(&e, "factory-12", None);
    e.stub
        .on_error(&state_path(), 500, "internal", "boom", "retry");
    let reply = stop(&e).expect("the reminder needs no server");
    assert!(
        reply["reason"]
            .as_str()
            .unwrap()
            .starts_with("board: you marked factory-12 done"),
        "{reply}"
    );
}

fn session_start(e: &Env, source: &str) {
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(CLAUDE_AGENT, DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 1),
    );
    let payload = e.with("SessionStart", json!({"source": source}));
    e.hook("session-start", "claude", payload);
    // Later deltas are empty again.
    nothing_doing(e);
}

#[test]
fn done_reminders_keep_turn_boundaries_across_compaction_and_sessions() {
    let e = env();
    nothing_doing(&e);
    prompt(&e, "one");
    mark_done(&e, "factory-9", None); // interrupted: no Stop
    prompt(&e, "two");
    mark_done(&e, "factory-10", None);
    session_start(&e, "compact");
    let reason = stop(&e).unwrap()["reason"].as_str().unwrap().to_owned();
    assert!(
        reason.starts_with("board: you marked factory-10 done this turn."),
        "compaction keeps the turn and its marks: {reason}"
    );

    mark_done(&e, "factory-11", None); // interrupted again
    session_start(&e, "resume");
    assert_eq!(
        stop(&e),
        None,
        "a resumed session drops the earlier turn's marks"
    );
}

#[test]
fn done_reminders_keep_turn_boundaries_while_the_server_is_unreachable() {
    let e = env();
    nothing_doing(&e);
    prompt(&e, "one");
    mark_done(&e, "factory-9", None); // interrupted: no Stop
    // Hooks on this device skip the server for a while after it failed to answer.
    let until = chrono::Utc::now() + chrono::Duration::minutes(5);
    std::fs::write(
        e.home.join("state/offline.json"),
        json!({"until": until}).to_string(),
    )
    .unwrap();
    prompt(&e, "two");
    assert_eq!(stop(&e), None, "the offline prompt still starts a turn");
    mark_done(&e, "factory-10", None);
    let reply = stop(&e).expect("the reminder needs no server");
    assert!(
        reply["reason"]
            .as_str()
            .unwrap()
            .starts_with("board: you marked factory-10 done this turn."),
        "{reply}"
    );
}

#[test]
fn codex_stop_reminds_after_task_done_in_codex_shape() {
    let e = env();
    let codex_agent = "codex:thr_9";
    e.stub.on(
        &format!("POST /v1/agents/{}/state", enc(codex_agent)),
        agent_fixture(codex_agent, DEVICE, AgentState::Idle),
    );
    e.stub.on("GET /v1/tasks", Vec::<board_proto::Task>::new());
    mark_done(&e, "factory-12", Some(codex_agent));
    let payload = |active: bool| {
        json!({
            "session_id": "thr_9", "transcript_path": null, "cwd": e.repo, "hook_event_name": "Stop",
            "model": "gpt-5.5-codex", "turn_id": "t", "stop_hook_active": active, "last_assistant_message": "ok",
        })
    };
    let (reply, _) = e.hook("stop", "codex", payload(false));
    let reply = reply.expect("JSON output");
    assert_eq!(reply.as_object().unwrap().len(), 2, "{reply}");
    assert_eq!(reply["decision"], "block");
    assert!(
        reply["reason"]
            .as_str()
            .unwrap()
            .starts_with("board: you marked factory-12 done"),
        "{reply}"
    );
    let (reply, _) = e.hook("stop", "codex", payload(true));
    assert_eq!(reply, Some(json!({})), "continued: valid empty Stop output");
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

/// An env whose agent has a cached full snapshot (seq 812) from a first `SessionStart`.
fn cached_env() -> (Env, Value) {
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
    (e, start)
}

fn breaker(e: &Env) -> Value {
    std::fs::read(e.home.join("state/offline.json"))
        .map(|b| serde_json::from_slice(&b).unwrap())
        .unwrap_or(Value::Null)
}

/// Runs a hook and returns its reply and how long it took.
fn timed(e: &Env, event: &str, payload: &Value) -> (Option<Value>, Duration) {
    timed_env(e, event, payload, &[])
}

fn timed_env(
    e: &Env,
    event: &str,
    payload: &Value,
    env: &[(&str, &str)],
) -> (Option<Value>, Duration) {
    let began = Instant::now();
    let (reply, _) = e.hook_env(event, "claude", payload.clone(), env);
    (reply, began.elapsed())
}

// Debug builds of the hook give the server exactly the share in `SHARE_ENV`, from just
// before its first request, however long the local steps and the client build take, so a
// test of the breaker does not depend on the runner's speed.
use board::hook::SHARE_ENV;
/// A fair share: enough to judge the server.
const FAIR_SHARE: [(&str, &str); 1] = [(SHARE_ENV, "1000")];
/// What a hook whose local steps took most of the budget leaves the server: too little to
/// judge it.
const SHORT_SHARE: [(&str, &str); 1] = [(SHARE_ENV, "300")];

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "release builds ignore the server share override"
)]
fn a_slow_server_falls_back_to_the_cache_and_is_tried_again() {
    let (e, start) = cached_env();
    e.stub.hang(Duration::from_secs(10));
    let (reply, _) = timed_env(&e, "session-start", &start, &FAIR_SHARE);
    let text = context(&reply, "SessionStart");
    assert!(
        text.starts_with(
            "board server slow (no answer within the hook's 1.5 s): cached snapshot from "
        ),
        "{text}"
    );
    assert!(
        text.contains(" ago (seq 812)") && text.ends_with(&started()),
        "{text}"
    );
    assert!(!text.contains("fail"), "board tools still work: {text}");
    assert_eq!(
        breaker(&e),
        json!({"strikes": 1}),
        "one slow reply opens nothing"
    );

    // The next hook tries the server again at once.
    let (reply, elapsed) = timed_env(&e, "session-start", &start, &FAIR_SHARE);
    assert!(
        elapsed >= Duration::from_millis(1000),
        "skipped the server: {elapsed:?}"
    );
    assert!(context(&reply, "SessionStart").starts_with("board server slow"));
    assert_eq!(breaker(&e), json!({"strikes": 2}));

    // Three slow replies in a row: hooks skip the server for a few seconds, still "slow".
    timed_env(&e, "session-start", &start, &FAIR_SHARE);
    let open = breaker(&e);
    assert_eq!(open["slow"], true, "{open}");
    let until: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(open["until"].clone()).unwrap();
    let left = until - chrono::Utc::now();
    assert!(
        left > chrono::Duration::zero() && left <= chrono::Duration::seconds(10),
        "{open}"
    );
    e.stub.clear();
    let (reply, _) = timed_env(&e, "session-start", &start, &FAIR_SHARE);
    assert!(context(&reply, "SessionStart").starts_with("board server slow"));
    assert!(e.stub.requests().is_empty(), "{:?}", e.stub.requests());

    // Once it has passed and the server answers in time, the breaker closes.
    std::fs::write(
        e.home.join("state/offline.json"),
        r#"{"until":"2000-01-01T00:00:00Z","slow":true,"strikes":2}"#,
    )
    .unwrap();
    e.stub.hang(Duration::ZERO);
    let (reply, _) = timed_env(&e, "session-start", &start, &FAIR_SHARE);
    assert_eq!(context(&reply, "SessionStart"), started());
    assert_eq!(breaker(&e), json!({}));
}

/// A hook that answers within its budget with the real share, whether the server is slow or
/// its connect hangs.
#[test]
fn a_slow_server_or_a_hung_connect_costs_a_hook_no_more_than_its_budget() {
    let (e, start) = cached_env();
    e.stub.hang(Duration::from_secs(10));
    let (reply, elapsed) = timed(&e, "session-start", &start);
    assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
    // On a very slow runner the hook's own guard may answer first, with nothing.
    if reply.is_some() {
        assert!(context(&reply, "SessionStart").starts_with("board server slow"));
    }

    let _listener = hang_connects(&e);
    let (reply, elapsed) = timed(&e, "session-start", &start);
    assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
    if reply.is_some() {
        let text = context(&reply, "SessionStart");
        assert!(text.starts_with("board server "), "{text}");
    }
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "release builds ignore the server share override"
)]
fn a_slow_reply_with_too_little_server_share_counts_no_strike() {
    let (e, start) = cached_env();
    e.stub.hang(Duration::from_secs(10));
    let (reply, _) = timed_env(&e, "session-start", &start, &SHORT_SHARE);
    assert!(context(&reply, "SessionStart").starts_with("board server slow"));
    assert_eq!(
        breaker(&e),
        Value::Null,
        "the server had too little time to judge it"
    );
}

/// Points the hook at a closed port, so its connection is refused at once.
fn refuse_connects(e: &Env) {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    std::fs::write(
        e.home.join("config.toml"),
        format!("server = \"{url}\"\ndevice = \"{DEVICE}\"\n"),
    )
    .unwrap();
}

/// A refused connection is unreachable however little time the local steps left.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "release builds ignore the server share override"
)]
fn a_refused_connect_with_too_little_server_share_still_backs_off() {
    let (e, start) = cached_env();
    refuse_connects(&e);
    let (reply, _) = timed_env(&e, "session-start", &start, &SHORT_SHARE);
    let text = context(&reply, "SessionStart");
    assert!(text.starts_with("board server unreachable"), "{text}");
    let open = breaker(&e);
    assert!(open["until"].is_string(), "{open}");
}

#[test]
fn an_unreachable_server_falls_back_to_the_cache_and_backs_off_a_minute() {
    let (e, start) = cached_env();
    refuse_connects(&e);
    let (reply, elapsed) = timed(&e, "session-start", &start);
    assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
    let text = context(&reply, "SessionStart");
    assert!(
        text.starts_with("board server unreachable: cached snapshot from ")
            && text.contains(" ago (seq 812); claims and updates fail until it is back.")
            && text.ends_with(&started()),
        "{text}"
    );
    let open = breaker(&e);
    assert!(open.get("slow").is_none(), "{open}");
    let until: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(open["until"].clone()).unwrap();
    let left = until - chrono::Utc::now();
    assert!(
        left > chrono::Duration::seconds(50) && left <= chrono::Duration::seconds(60),
        "{open}"
    );

    // Later hooks on this device skip the server while the backoff lasts.
    std::fs::write(
        e.home.join("config.toml"),
        format!("server = \"{}\"\ndevice = \"{DEVICE}\"\n", e.stub.url),
    )
    .unwrap();
    e.stub.clear();
    let (reply, _) = timed(&e, "session-start", &start);
    assert!(context(&reply, "SessionStart").starts_with("board server unreachable"));
    let (_, text) = e.hook(
        "post-tool-use",
        "claude",
        e.with(
            "PostToolUse",
            json!({"tool_name": "Read", "tool_input": {}}),
        ),
    );
    assert_eq!(text, "", "mid-turn hooks stay quiet offline");
    assert!(e.stub.requests().is_empty(), "{:?}", e.stub.requests());

    // A breaker file from before slow replies were told apart still reads as unreachable.
    let until = chrono::Utc::now() + chrono::Duration::minutes(1);
    std::fs::write(
        e.home.join("state/offline.json"),
        json!({"until": until}).to_string(),
    )
    .unwrap();
    let (reply, _) = timed(&e, "session-start", &start);
    assert!(context(&reply, "SessionStart").starts_with("board server unreachable"));
}

/// Points the hook at a local port that takes connections but never answers TLS, so its
/// connection (TLS included) never opens, as to a tailnet peer that is asleep. The listener
/// must live for the test.
fn hang_connects(e: &Env) -> std::net::TcpListener {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(
        e.home.join("config.toml"),
        format!(
            "server = \"https://{}\"\ndevice = \"{DEVICE}\"\n",
            listener.local_addr().unwrap()
        ),
    )
    .unwrap();
    listener
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "release builds ignore the server share override"
)]
fn a_connect_that_hangs_counts_as_unreachable() {
    let (e, start) = cached_env();
    let _listener = hang_connects(&e);
    let (reply, _) = timed_env(&e, "session-start", &start, &FAIR_SHARE);
    let text = context(&reply, "SessionStart");
    assert!(text.starts_with("board server unreachable"), "{text}");
    let open = breaker(&e);
    assert!(open.get("slow").is_none(), "{open}");
    let until: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(open["until"].clone()).unwrap();
    assert!(
        until - chrono::Utc::now() > chrono::Duration::seconds(50),
        "{open}"
    );
}

/// A connect that hangs while the hook's local steps took most of the budget had too little
/// time to fail on its own deadline: it ends with the request, as slow, and the breaker stays
/// as it was. The connect's deadline never races the request's, so the outcome is fixed.
#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "release builds ignore the server share override"
)]
fn a_connect_that_hangs_with_too_little_server_share_reads_as_slow() {
    let (e, start) = cached_env();
    let _listener = hang_connects(&e);
    let (reply, _) = timed_env(&e, "session-start", &start, &SHORT_SHARE);
    let text = context(&reply, "SessionStart");
    assert!(text.starts_with("board server slow"), "{text}");
    assert_eq!(
        breaker(&e),
        Value::Null,
        "the server had too little time to judge it"
    );
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

/// A slow `t3`, so every answer comes from a background read and no step depends on how
/// fast the machine runs.
#[cfg(unix)]
#[test]
fn a_delegated_t3_thread_registers_its_parent() {
    const CHILD: &str = "thread:delegated-task:command%3Amcp%3Ac1%3Adelegate-task%3At1";
    let e = env();
    let (t3, log, answer) = fake_t3(&e.home, true);
    let files = SessionFiles::of(&e);
    // T3 names a parent for any thread asked about.
    let body = json!({"thread": {
        "threadId": CHILD, "parentThreadId": "parent-1", "relationshipToParent": "subagent",
    }});
    std::fs::write(&answer, body.to_string()).unwrap();
    let start = |thread: &str| {
        let agent = format!("t3:{thread}");
        e.stub.clear();
        e.stub.on(
            "POST /v1/agents",
            agent_fixture(&agent, DEVICE, AgentState::Running),
        );
        e.stub.on(
            "GET /v1/snapshot",
            snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
        );
        // At debug level the hook logs why it has no parent: T3 over budget, a background
        // read that did not start, or T3's error; the background read inherits the level.
        let out = e.hook_output(
            "session-start",
            "claude",
            e.with("SessionStart", json!({"source": "startup"})),
            &[
                ("T3CODE_THREAD_ID", thread),
                ("T3CODE_CLI_PATH", &t3),
                ("BOARD_LOG", "board=debug"),
                ("NO_COLOR", "1"),
            ],
        );
        let register = e.stub.requests_to("POST /v1/agents").pop().unwrap().body;
        assert_eq!(register["id"], agent);
        assert_eq!(register["native_ids"]["t3_thread"], thread);
        let parent = register["native_ids"]["t3_parent"]
            .as_str()
            .map(str::to_owned);
        (parent, stderr(&out))
    };
    let calls = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count()
    };

    // The child's first start registers no parent: T3 answers in the background.
    let (parent, hook) = start(CHILD);
    assert_eq!(
        parent, None,
        "T3 answered within the hook's budget:\n{hook}"
    );
    files.read_done_after(&hook);
    let answer = std::fs::read_to_string(&files.read).unwrap();
    // A resume registers the parent the read found, then keeps it from the cache without
    // asking T3: one that asked would start a read.
    let (parent, hook) = start(CHILD);
    assert_eq!(
        parent.as_deref(),
        Some("parent-1"),
        "the resume dropped the background read; T3's answer as read: {answer}\nthe hook's log:\n{hook}"
    );
    assert!(!files.read.exists());
    files.backdate("thread_read_at");
    let started = files.cached()["thread_read_at"].clone();
    let (parent, hook) = start(CHILD);
    assert_eq!(
        parent.as_deref(),
        Some("parent-1"),
        "the session cache lost the parent: {}\nthe hook's log:\n{hook}",
        files.cached()
    );
    assert_eq!(files.cached()["thread_read_at"], started);
    assert_eq!(calls(), 1);

    // A thread no parent delegated keeps none, whatever T3 says.
    let (parent, hook) = start("thread-7");
    assert_eq!(parent, None);
    files.read_done_after(&hook);
    assert_eq!(start("thread-7").0, None);
    assert!(
        !files.read.exists(),
        "a read that tells nothing new is dropped"
    );
    assert_eq!(calls(), 2);
}

/// A `t3` that answers `thread show` with the JSON in `answer` and logs each call to `log`.
/// A `slow` one answers after 0.7 seconds, as T3 does on a busy Mac: always past the hooks'
/// own budget for T3 (0.4 seconds), so only a background read gets its answer. It logs a
/// call once it answers, so a call the hook gave up on is not logged.
#[cfg(unix)]
fn fake_t3(dir: &std::path::Path, slow: bool) -> (String, PathBuf, PathBuf) {
    let log = dir.join("t3.log");
    let answer = dir.join("t3-answer.json");
    let t3 = dir.join("t3");
    let wait = if slow { "sleep 0.7\n" } else { "" };
    common::script::write_script(
        &t3,
        &format!(
            "#!/bin/sh\n{wait}echo \"$@\" >> '{}'\ncat '{}'\n",
            log.display(),
            answer.display()
        ),
    );
    (t3.to_str().unwrap().to_owned(), log, answer)
}

/// The session's cached decisions and its background read of the T3 thread, as files.
struct SessionFiles {
    cache: PathBuf,
    read: PathBuf,
    read_log: PathBuf,
}

impl SessionFiles {
    fn of(e: &Env) -> Self {
        let state = e.home.join("state");
        let name = format!("claude-{SESSION}.json");
        Self {
            cache: state.join("sessions").join(&name),
            read: state.join("thread-reads").join(name),
            read_log: state.join(board::hook::READ_LOG),
        }
    }

    fn cached(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.cache).unwrap()).unwrap()
    }

    fn set(&self, field: &str, at: chrono::DateTime<chrono::Utc>) {
        let mut cached = self.cached();
        cached[field] = json!(at);
        std::fs::write(&self.cache, cached.to_string()).unwrap();
    }

    /// Dates `field` an hour back.
    fn backdate(&self, field: &str) {
        self.set(field, chrono::Utc::now() - chrono::Duration::minutes(61));
    }

    /// Leaves a finished background read of `thread` that asked T3 at `at`.
    fn write_read(&self, thread: &str, at: chrono::DateTime<chrono::Utc>, title: &str) {
        std::fs::create_dir_all(self.read.parent().unwrap()).unwrap();
        let read = json!({"thread": thread, "at": at, "title": title});
        std::fs::write(&self.read, read.to_string()).unwrap();
    }

    /// Waits, bounded, for the background read a hook started to leave its answer.
    fn read_done(&self) {
        self.read_done_after("not kept");
    }

    /// [`Self::read_done`] after a hook that logged `hook`. A read that never answers fails
    /// with both logs: the hook's says whether the read started, the read's own
    /// ([`board::hook::READ_LOG`]) whether T3 failed or missed the read's budget.
    fn read_done_after(&self, hook: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self.read.exists() {
            if Instant::now() >= deadline {
                let read_log = std::fs::read_to_string(&self.read_log)
                    .unwrap_or_else(|err| format!("none ({err})"));
                panic!(
                    "no background read answered\nthe hook's log:\n{hook}\nthe read's log:\n{read_log}"
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Every step takes T3's answer from a background read, which a slow `t3` makes the only
/// path, so no step depends on how fast the machine runs.
#[cfg(unix)]
#[test]
fn a_t3_thread_registers_its_title_and_link_and_follows_a_rename() {
    const LINK: &str = "https://app.example.test/threads/env-1/thread-7";
    let e = env();
    let (t3, log, answer) = fake_t3(&e.home, true);
    let files = SessionFiles::of(&e);
    let titled = |title: &str| {
        let body = json!({"thread": {"threadId": "thread-7", "title": title, "link": LINK}});
        std::fs::write(&answer, body.to_string()).unwrap();
    };
    let answered = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let agent = "t3:thread-7";
    let t3_env = [("T3CODE_THREAD_ID", "thread-7"), ("T3CODE_CLI_PATH", &t3)];
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(agent, DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
    );
    e.stub.on(
        &format!("POST /v1/agents/{}/state", enc(agent)),
        agent_fixture(agent, DEVICE, AgentState::Idle),
    );
    e.stub.on("GET /v1/tasks", Vec::<board_proto::Task>::new());
    let start = |source: &str| {
        let payload = e.with("SessionStart", json!({"source": source}));
        e.hook_env("session-start", "claude", payload, &t3_env);
    };
    let prompt = || {
        let payload = e.with("UserPromptSubmit", json!({"prompt": "Go on."}));
        e.hook_env("user-prompt-submit", "claude", payload, &t3_env);
    };
    let stop = || {
        let payload = e.with(
            "Stop",
            json!({"stop_hook_active": false, "last_assistant_message": "Done."}),
        );
        e.hook_env("stop", "claude", payload, &t3_env);
    };
    let registered = || {
        e.stub
            .requests_to("POST /v1/agents")
            .into_iter()
            .map(|r| r.body)
            .collect::<Vec<_>>()
    };

    // SessionStart does not wait for a slow T3: it registers the thread without its title
    // or link and leaves them to a background read.
    titled("New thread");
    start("startup");
    let first = registered().pop().unwrap();
    assert_eq!(first["native_ids"]["t3_thread"], "thread-7");
    assert!(first["title"].is_null(), "{first}");
    assert!(first["native_ids"].get("t3_link").is_none(), "{first}");

    // The next hook registers what the read found, and drops it.
    files.read_done();
    assert_eq!(answered(), 1);
    e.stub.clear();
    prompt();
    let read = registered().pop().unwrap();
    assert_eq!(read["title"], "New thread");
    assert_eq!(read["native_ids"]["t3_link"], LINK);
    assert_eq!(read["repo"], first["repo"]);
    assert!(!files.read.exists());

    // T3 renames the thread after its first turn. The session's first Stop asks, but T3 is
    // too slow and the read SessionStart started may still run: the check stays due.
    titled("Fix the parser");
    files.set("thread_read_at", chrono::Utc::now());
    let started = files.cached()["thread_read_at"].clone();
    e.stub.clear();
    stop();
    assert!(registered().is_empty());
    assert!(files.cached()["title_checked_at"].is_null());
    assert_eq!(files.cached()["thread_read_at"], started);

    // Once that read is done with, the next Stop starts a new one and counts the check.
    files.backdate("thread_read_at");
    stop();
    assert!(registered().is_empty());
    assert!(!files.cached()["title_checked_at"].is_null());
    files.read_done();
    assert_eq!(answered(), 2);

    // The next Stop marks the agent idle with its message first, then registers the new
    // title, and marks it idle again, since registering marks it running. The server
    // refuses the registration this time, so the read is kept.
    e.stub.clear();
    e.stub
        .once_error("POST /v1/agents", 500, "internal", "try again");
    stop();
    let routes = || {
        e.stub
            .requests()
            .iter()
            .filter(|r| r.method != "GET")
            .map(|r| format!("{} {}", r.method, r.path))
            .collect::<Vec<_>>()
    };
    let state = format!("POST /v1/agents/{}/state", enc(agent));
    assert_eq!(routes(), [state.clone(), "POST /v1/agents".to_owned()]);
    assert_eq!(e.stub.requests_to(&state)[0].body["said"], "Done.");
    assert!(files.read.exists());

    // The next Stop registers it again.
    e.stub.clear();
    stop();
    assert_eq!(
        routes(),
        [state.clone(), "POST /v1/agents".to_owned(), state.clone()]
    );
    let again = registered().pop().unwrap();
    assert_eq!(again["title"], "Fix the parser");
    assert_eq!(again["native_ids"]["t3_link"], LINK);
    assert_eq!(again["repo"], first["repo"]);
    assert_eq!(again["branch"], first["branch"]);
    let states = e.stub.requests_to(&state);
    assert_eq!(states[1].body["state"], "idle");
    assert_eq!(states[1].body["said"], Value::Null);
    assert!(!files.read.exists());
    assert_eq!(answered(), 2);

    // Within the hour, no Stop asks T3 again: one that asked would start a read.
    titled("Renamed again");
    files.backdate("thread_read_at");
    let started = files.cached()["thread_read_at"].clone();
    e.stub.clear();
    stop();
    assert!(registered().is_empty());
    assert_eq!(files.cached()["thread_read_at"], started);

    // An hour later a Stop asks again; an unchanged title sends nothing.
    titled("Fix the parser");
    files.backdate("title_checked_at");
    stop();
    assert_ne!(files.cached()["thread_read_at"], started, "no read started");
    files.read_done();
    assert_eq!(answered(), 3);
    stop();
    assert!(registered().is_empty(), "the title did not change");
    assert!(!files.read.exists());

    // Compaction starts the session again: the cached title, without asking T3.
    titled("Something else");
    files.backdate("thread_read_at");
    let started = files.cached()["thread_read_at"].clone();
    start("compact");
    assert_eq!(registered().pop().unwrap()["title"], "Fix the parser");
    assert_eq!(files.cached()["thread_read_at"], started);
}

/// A slow-T3 thread whose SessionStart registered it without a title, for the tests of
/// what later hooks do with background reads.
#[cfg(unix)]
struct SlowThread {
    e: Env,
    files: SessionFiles,
    answer: PathBuf,
    t3: String,
}

#[cfg(unix)]
impl SlowThread {
    const AGENT: &str = "t3:thread-7";

    fn start() -> Self {
        let e = env();
        let (t3, _, answer) = fake_t3(&e.home, true);
        let files = SessionFiles::of(&e);
        e.stub.on(
            "POST /v1/agents",
            agent_fixture(Self::AGENT, DEVICE, AgentState::Running),
        );
        e.stub.on(
            "GET /v1/snapshot",
            snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
        );
        e.stub.on(
            &format!("POST /v1/agents/{}/state", enc(Self::AGENT)),
            agent_fixture(Self::AGENT, DEVICE, AgentState::Idle),
        );
        e.stub.on("GET /v1/tasks", Vec::<board_proto::Task>::new());
        let thread = Self {
            e,
            files,
            answer,
            t3,
        };
        thread.titled("New thread");
        thread.hook("session-start", json!({"source": "startup"}));
        thread
    }

    fn titled(&self, title: &str) {
        let body = json!({"thread": {"threadId": "thread-7", "title": title}});
        std::fs::write(&self.answer, body.to_string()).unwrap();
    }

    fn hook(&self, event: &str, extra: Value) {
        let name = match event {
            "session-start" => "SessionStart",
            "user-prompt-submit" => "UserPromptSubmit",
            _ => "Stop",
        };
        let env = [
            ("T3CODE_THREAD_ID", "thread-7"),
            ("T3CODE_CLI_PATH", self.t3.as_str()),
        ];
        self.e
            .hook_env(event, "claude", self.e.with(name, extra), &env);
    }

    fn prompt(&self) {
        self.hook("user-prompt-submit", json!({"prompt": "Go on."}));
    }

    fn stop(&self) {
        let extra = json!({"stop_hook_active": false, "last_assistant_message": "Done."});
        self.hook("stop", extra);
    }

    fn titles(&self) -> Vec<Value> {
        self.e
            .stub
            .requests_to("POST /v1/agents")
            .into_iter()
            .map(|r| r.body["title"].clone())
            .collect()
    }
}

#[cfg(unix)]
#[test]
fn background_reads_older_than_the_registered_facts_or_refused_are_dropped() {
    let t = SlowThread::start();
    t.files.read_done();
    t.e.stub.clear();
    t.prompt();
    assert_eq!(t.titles(), ["New thread"]);
    let facts_at: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(t.files.cached()["t3_facts_at"].clone()).unwrap();

    // A read that asked T3 before the registered facts were read is older news.
    let before = facts_at - chrono::Duration::seconds(1);
    t.files.write_read("thread-7", before, "Older title");
    t.e.stub.clear();
    t.prompt();
    assert!(t.titles().is_empty());
    assert!(!t.files.read.exists());

    // So is one over an hour old, such as one a session that ended left behind.
    let stale = chrono::Utc::now() - chrono::Duration::minutes(61);
    t.files
        .set("t3_facts_at", stale - chrono::Duration::minutes(1));
    t.files.write_read("thread-7", stale, "Stale title");
    t.prompt();
    assert!(t.titles().is_empty());
    assert!(!t.files.read.exists());

    // A read of another thread says nothing of this one.
    t.files
        .write_read("thread-8", chrono::Utc::now(), "Another thread");
    t.prompt();
    assert!(t.titles().is_empty());
    assert!(!t.files.read.exists());

    // A server that refuses a read as invalid would refuse it again: it is dropped.
    t.files
        .write_read("thread-7", chrono::Utc::now(), "Refused title");
    t.e.stub
        .once_error("POST /v1/agents", 400, "invalid", "bad native id");
    t.prompt();
    assert_eq!(t.titles(), ["Refused title"]);
    assert!(!t.files.read.exists());
    t.e.stub.clear();
    t.prompt();
    assert!(t.titles().is_empty());
}

#[cfg(unix)]
#[test]
fn a_due_title_check_asks_t3_before_an_older_read_counts() {
    // The read SessionStart started finishes during a first turn that uses no tool, and
    // T3 renames the thread meanwhile.
    let t = SlowThread::start();
    t.files.read_done();
    t.titled("Fix the parser");
    t.files.backdate("thread_read_at");

    // The first Stop's check asks T3 first. Too slow, it starts a new read, and registers
    // the older one meanwhile.
    t.e.stub.clear();
    t.stop();
    assert_eq!(t.titles(), ["New thread"]);
    let checked = t.files.cached()["title_checked_at"].clone();
    assert!(!checked.is_null());

    // The next Stop registers the new read; the check it already counted stays as it was.
    t.files.read_done();
    t.e.stub.clear();
    t.stop();
    assert_eq!(t.titles(), ["Fix the parser"]);
    assert_eq!(t.files.cached()["title_checked_at"], checked);
}

#[cfg(unix)]
#[test]
fn a_slow_t3_brings_a_delegated_threads_title_link_and_parent_to_a_tool_call() {
    const CHILD: &str = "thread:delegated-task:command%3Amcp%3Ac1%3Adelegate-task%3At1";
    const LINK: &str = "https://app.example.test/threads/env-1/child";
    let e = env();
    let (t3, log, answer) = fake_t3(&e.home, true);
    let files = SessionFiles::of(&e);
    let body = json!({"thread": {
        "threadId": CHILD, "title": "Child work", "link": LINK,
        "parentThreadId": "parent-1", "relationshipToParent": "subagent",
    }});
    std::fs::write(&answer, body.to_string()).unwrap();
    let agent = format!("t3:{CHILD}");
    let t3_env = [
        ("T3CODE_THREAD_ID", CHILD),
        ("T3CODE_CLI_PATH", t3.as_str()),
    ];
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(&agent, DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
    );
    e.stub.on(
        &format!("POST /v1/agents/{}/state", enc(&agent)),
        agent_fixture(&agent, DEVICE, AgentState::Running),
    );
    let registered = || {
        e.stub
            .requests_to("POST /v1/agents")
            .into_iter()
            .map(|r| r.body)
            .collect::<Vec<_>>()
    };
    let read_tool = || {
        let payload = e.with(
            "PostToolUse",
            json!({
                "tool_name": "Read",
                "tool_input": {"file_path": e.repo.join("src/lib.rs")},
                "tool_response": {"type": "text"},
                "tool_use_id": "toolu_01READ",
            }),
        );
        e.hook_env("post-tool-use", "claude", payload, &t3_env);
    };

    let payload = e.with("SessionStart", json!({"source": "startup"}));
    e.hook_env("session-start", "claude", payload, &t3_env);
    let first = registered().pop().unwrap();
    assert!(first["title"].is_null(), "{first}");
    assert!(first["native_ids"].get("t3_parent").is_none(), "{first}");

    // The first tool call after the read registers all three, the agent still running.
    files.read_done();
    e.stub.clear();
    read_tool();
    let read = registered().pop().unwrap();
    assert_eq!(read["title"], "Child work");
    assert_eq!(read["native_ids"]["t3_link"], LINK);
    assert_eq!(read["native_ids"]["t3_parent"], "parent-1");
    assert_eq!(read["native_ids"]["t3_thread"], CHILD);
    assert!(
        e.stub
            .requests_to(&format!("POST /v1/agents/{}/state", enc(&agent)))
            .iter()
            .all(|r| r.body["state"] != "idle")
    );
    assert!(!files.read.exists());

    // Later tool calls register nothing more, and T3 was asked once to the end.
    e.stub.clear();
    read_tool();
    assert!(registered().is_empty());
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
}

#[cfg(unix)]
#[test]
fn a_delegated_thread_t3_missed_at_start_registers_its_parent_with_its_title() {
    const CHILD: &str = "thread:delegated-task:command%3Amcp%3Ac1%3Adelegate-task%3At1";
    let e = env();
    // A slow `t3`, so no step depends on how fast the machine runs.
    let (t3, _, answer) = fake_t3(&e.home, true);
    let files = SessionFiles::of(&e);
    let agent = format!("t3:{CHILD}");
    let t3_env = [
        ("T3CODE_THREAD_ID", CHILD),
        ("T3CODE_CLI_PATH", t3.as_str()),
    ];
    e.stub.on(
        "POST /v1/agents",
        agent_fixture(&agent, DEVICE, AgentState::Running),
    );
    e.stub.on(
        "GET /v1/snapshot",
        snapshot_fixture(SnapshotKind::Full, SNAPSHOT, 812),
    );
    e.stub.on(
        &format!("POST /v1/agents/{}/state", enc(&agent)),
        agent_fixture(&agent, DEVICE, AgentState::Idle),
    );
    e.stub.on("GET /v1/tasks", Vec::<board_proto::Task>::new());
    let start = |source: &str| {
        let payload = e.with("SessionStart", json!({"source": source}));
        e.hook_env("session-start", "claude", payload, &t3_env);
    };
    let stop = || {
        let payload = e.with(
            "Stop",
            json!({"stop_hook_active": false, "last_assistant_message": "Done."}),
        );
        e.hook_env("stop", "claude", payload, &t3_env);
    };
    let last_registered = || e.stub.requests_to("POST /v1/agents").pop().unwrap().body;

    // T3 has neither title nor parent for the thread at SessionStart, and answers too late.
    std::fs::write(&answer, json!({"thread": {"threadId": CHILD}}).to_string()).unwrap();
    start("startup");
    assert!(last_registered()["native_ids"].get("t3_parent").is_none());
    // Its background read finds nothing new, which the next hook drops.
    files.read_done();

    // The first Stop's check starts a read that finds the title, and the parent with it;
    // the next Stop registers both.
    let body = json!({"thread": {
        "threadId": CHILD, "title": "Child work",
        "parentThreadId": "parent-1", "relationshipToParent": "subagent",
    }});
    std::fs::write(&answer, body.to_string()).unwrap();
    files.backdate("thread_read_at");
    e.stub.clear();
    stop();
    assert!(e.stub.requests_to("POST /v1/agents").is_empty());
    files.read_done();
    stop();
    let renamed = last_registered();
    assert_eq!(renamed["title"], "Child work");
    assert_eq!(renamed["native_ids"]["t3_parent"], "parent-1");

    // Compaction registers from the cache, parent included, without asking T3: one that
    // asked would start a read.
    files.backdate("thread_read_at");
    let started = files.cached()["thread_read_at"].clone();
    e.stub.clear();
    start("compact");
    assert_eq!(files.cached()["thread_read_at"], started);
    assert_eq!(last_registered()["native_ids"]["t3_parent"], "parent-1");
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
    // No `t3` there: the device's own T3 never hears of this thread.
    let no_t3 = e.home.join("no-t3");
    let t3_env = [
        ("T3CODE_THREAD_ID", "thread-7"),
        ("T3CODE_CLI_PATH", no_t3.to_str().unwrap()),
    ];
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
