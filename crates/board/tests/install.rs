//! `board install`: merging into existing runtime settings and printing service files.

mod common;

use common::*;
use serde_json::{Value, json};

#[test]
fn install_claude_merges_into_existing_settings_without_losing_keys() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("board-home");
    let settings = dir.path().join("settings.json");
    let claude_json = dir.path().join("claude.json");
    std::fs::write(
        &settings,
        json!({
            "model": "opus",
            "permissions": {"allow": ["Bash(git status)"]},
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "afplay done.aiff"}]}],
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}],
                "SessionStart": [{"hooks": [{"type": "command", "command": "/old/path/board hook session-start --runtime claude"}]}]
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        &claude_json,
        json!({"numStartups": 3, "mcpServers": {"other": {"type": "stdio", "command": "x"}}})
            .to_string(),
    )
    .unwrap();
    let args = [
        "install",
        "claude",
        "--bin",
        "/opt/bin/board",
        "--write",
        settings.to_str().unwrap(),
        "--mcp-config",
        claude_json.to_str().unwrap(),
    ];
    let out = board(&home, dir.path(), &[], &args, "");
    assert!(out.status.success(), "{}", stderr(&out));
    let first = std::fs::read_to_string(&settings).unwrap();
    let out = board(&home, dir.path(), &[], &args, "");
    assert!(out.status.success());
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        first,
        "idempotent"
    );

    let v: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(v["model"], "opus");
    assert_eq!(v["permissions"]["allow"][0], "Bash(git status)");
    assert_eq!(
        v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "guard.sh"
    );
    let stop = v["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2);
    assert_eq!(stop[0]["hooks"][0]["command"], "afplay done.aiff");
    assert_eq!(
        stop[1]["hooks"][0]["command"],
        "/opt/bin/board hook stop --runtime claude"
    );
    let start = v["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(start.len(), 1, "the old board entry is replaced: {start:?}");
    assert_eq!(start[0]["matcher"], "startup|resume|clear|compact");
    for (event, matcher) in [
        ("PostToolUse", Some("*")),
        ("StopFailure", Some("rate_limit")),
        ("PreCompact", Some("manual|auto")),
        ("UserPromptSubmit", None),
        ("SessionEnd", None),
    ] {
        let group = &v["hooks"][event][0];
        assert_eq!(group["matcher"].as_str(), matcher, "{event}");
        assert!(
            group["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("--runtime claude")
        );
    }

    let mcp: Value = serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
    assert_eq!(mcp["numStartups"], 3);
    assert_eq!(mcp["mcpServers"]["other"]["command"], "x");
    assert_eq!(
        mcp["mcpServers"]["board"],
        json!({"type": "stdio", "command": "/opt/bin/board", "args": ["mcp"]})
    );
}

#[test]
fn install_claude_prints_without_write() {
    let dir = tempfile::tempdir().unwrap();
    let out = board(
        &dir.path().join("h"),
        dir.path(),
        &[],
        &["install", "claude", "--bin", "/opt/bin/board"],
        "",
    );
    let text = stdout(&out);
    assert!(text.contains("\"/opt/bin/board hook user-prompt-submit --runtime claude\""));
    assert!(text.contains("claude mcp add --scope user board -- /opt/bin/board mcp"));
}

#[test]
fn install_codex_writes_hooks_and_prints_the_mcp_snippet() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = dir.path().join("hooks.json");
    std::fs::write(
        &hooks,
        json!({"description": "mine", "hooks": {}}).to_string(),
    )
    .unwrap();
    let out = board(
        &dir.path().join("h"),
        dir.path(),
        &[],
        &[
            "install",
            "codex",
            "--bin",
            "/opt/bin/board",
            "--write",
            hooks.to_str().unwrap(),
        ],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(v["description"], "mine");
    assert!(
        v["hooks"].get("StopFailure").is_none(),
        "Codex has no StopFailure"
    );
    assert_eq!(v["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3);
    assert_eq!(
        v["hooks"]["SessionStart"][0]["matcher"],
        "startup|resume|clear|compact|fork"
    );
    let text = stdout(&out);
    assert!(text.contains("[mcp_servers.board]"));
    assert!(text.contains("env_vars = [\"T3CODE_THREAD_ID\"]"));
}

#[test]
fn service_files_print_for_launchd_and_systemd() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    let out = board(
        &home,
        dir.path(),
        &[],
        &["install", "launchd", "--bin", "/opt/bin/board"],
        "",
    );
    let plist = stdout(&out);
    assert!(plist.contains("<string>dev.agent-board.watch</string>"));
    assert!(plist.contains("<string>/opt/bin/board</string>\n    <string>watch</string>"));
    let unit_path = dir.path().join("unit.service");
    let out = board(
        &home,
        dir.path(),
        &[],
        &[
            "install",
            "systemd",
            "--bin",
            "/opt/bin/board",
            "--write",
            unit_path.to_str().unwrap(),
        ],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let unit = std::fs::read_to_string(unit_path).unwrap();
    assert!(unit.contains("ExecStart=\"/opt/bin/board\" watch"));
    assert!(unit.contains("Restart=always"));
}
