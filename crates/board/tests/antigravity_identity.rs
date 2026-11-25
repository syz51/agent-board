//! Integration tests for Antigravity ACP / shell identity resolution across `board` CLI,
//! `board mcp` over stdio, and `AcpRunner` child -> `board mcp` grandchild propagation.

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use board_proto::{Handoff, Note, NoteKind, TaskStatus};
use common::*;
use serde_json::{Value, json};

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Mcp {
    fn spawn(home: &Path, cwd: &Path, extra_env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_board"));
        command
            .arg("mcp")
            .current_dir(cwd)
            .env("BOARD_HOME", home)
            .env("BOARD_TOKEN", TOKEN)
            .env("BOARD_KEYCHAIN", "0")
            .env_remove("T3CODE_THREAD_ID")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("OPENCODE_SESSION_ID")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .env_remove("BOARD_AGENT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in extra_env {
            command.env(k, v);
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .expect("an answer from the MCP server");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                assert!(message.get("error").is_none(), "{message}");
                return message["result"].clone();
            }
        }
    }

    fn init(&mut self) {
        self.request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "antigravity-test", "version": "1.0"}
            }),
        );
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    fn call(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        )
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(result: &Value) -> &str {
    assert_ne!(
        result.get("isError"),
        Some(&Value::Bool(true)),
        "expected tool success, got: {result}"
    );
    result["content"][0]["text"].as_str().unwrap()
}

fn stub_mutating_routes(stub: &Stub, agent_id: &str) {
    // Unknown before first call, so `ensure_registered` registers with Runtime::Antigravity.
    stub.on_error(
        &format!("GET /v1/agents/{}", enc(agent_id)),
        404,
        "not_found",
        "agent not found",
        "register first",
    );
    stub.on(
        "POST /v1/agents",
        agent_fixture(agent_id, DEVICE, board_proto::AgentState::Running),
    );
    stub.on("GET /v1/boards", [board_fixture("factory", &[])]);
    stub.on("GET /v1/boards/factory", board_fixture("factory", &[]));
    stub.on(
        "GET /v1/agents",
        [agent_fixture(
            agent_id,
            DEVICE,
            board_proto::AgentState::Running,
        )],
    );

    let mut claimed = task_fixture("factory-12", "Fix Antigravity MCP identity");
    claimed.status = TaskStatus::Doing;
    claimed.owner = Some(agent_id.parse().unwrap());
    stub.on("POST /v1/tasks/factory-12/claim", &claimed);

    let mut updated = claimed.clone();
    updated.status = TaskStatus::Review;
    updated.version = 2;
    stub.on(
        "GET /v1/tasks/factory-12",
        board_proto::TaskDetail {
            task: claimed.clone(),
            notes: vec![],
            evidence: vec![],
            blocks: vec![],
            children: vec![],
            child_tasks: vec![],
            history: vec![],
            handles: Default::default(),
        },
    );
    stub.on("PATCH /v1/tasks/factory-12", &updated);
    stub.on(
        "POST /v1/tasks/factory-12/notes",
        Note {
            id: 7,
            task: "factory-12".parse().unwrap(),
            agent: agent_id.parse().unwrap(),
            kind: NoteKind::Note,
            text: "PR ready".into(),
            at: at(),
        },
    );
    stub.on_error(
        &format!("GET /v1/agents/{}/handoff", enc(agent_id)),
        404,
        "not_found",
        "no handoff",
        "put handoff",
    );
    stub.on(
        &format!("PUT /v1/agents/{}/handoff", enc(agent_id)),
        Handoff {
            agent: agent_id.parse().unwrap(),
            now: "fixing identity".into(),
            next: "open PR".into(),
            blockers: String::new(),
            version: 1,
            updated_at: at(),
            last_change_at: Some(at()),
        },
    );

    stub.on(
        "POST /v1/feedback/board-feedback-1240/resolve",
        task_fixture("board-feedback-1240", "MCP lacks agent id"),
    );
}

#[test]
fn mcp_resolves_board_agent_env_and_mutates_without_explicit_agent() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let agent_id = "antigravity:run-acp-1";
    stub_mutating_routes(&stub, agent_id);
    let home = home_for(dir.path(), &stub.url);
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    // Mirrors dispatcher-launched Antigravity ACP: `board mcp` inherits `BOARD_AGENT` from
    // `AcpRunner`, with NO `ANTIGRAVITY_CONVERSATION_ID` and NO `--agent`.
    let mut mcp = Mcp::spawn(&home, &cwd, &[("BOARD_AGENT", agent_id)]);
    mcp.init();

    let status = mcp.call(2, "board_status", json!({"board": "factory"}));
    let status_text = text(&status);
    assert!(
        status_text.contains(&format!("you: {agent_id}")),
        "board_status must report pre-assigned antigravity id: {status_text}"
    );
    assert!(
        !status_text.contains("no agent id"),
        "board_status must not report device-only: {status_text}"
    );

    // Every mutating tool succeeds without an explicit `agent` parameter.
    text(&mcp.call(3, "task_claim", json!({"id": "factory-12"})));
    text(&mcp.call(
        4,
        "task_update",
        json!({"id": "factory-12", "status": "review"}),
    ));
    text(&mcp.call(
        5,
        "task_note",
        json!({"id": "factory-12", "text": "PR ready"}),
    ));
    text(&mcp.call(
        6,
        "handoff_set",
        json!({"now": "fixing identity", "next": "open PR"}),
    ));
    text(&mcp.call(
        7,
        "feedback_resolve",
        json!({
            "id": "board-feedback-1240",
            "kind": "fixed",
            "task": "factory-12",
            "reason": "resolved",
        }),
    ));

    let reg = stub.requests_to("POST /v1/agents");
    assert_eq!(reg.len(), 1, "agent registered once on first call");
    assert_eq!(reg[0].body["id"], agent_id);
    assert_eq!(reg[0].body["runtime"], "antigravity");
    assert_eq!(reg[0].agent.as_deref(), Some(agent_id));

    for route in [
        "POST /v1/tasks/factory-12/claim",
        "PATCH /v1/tasks/factory-12",
        "POST /v1/tasks/factory-12/notes",
        &format!("PUT /v1/agents/{}/handoff", enc(agent_id)),
        "POST /v1/feedback/board-feedback-1240/resolve",
    ] {
        let reqs = stub.requests_to(route);
        assert_eq!(reqs.len(), 1, "expected 1 request to {route}");
        assert_eq!(
            reqs[0].agent.as_deref(),
            Some(agent_id),
            "{route} must carry X-Board-Agent"
        );
    }
}

#[test]
fn cli_and_mcp_resolve_antigravity_conversation_id_and_keep_precedence() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let standalone_id = "antigravity:conv-shell-1";
    let preassigned_id = "antigravity:run-acp-2";
    let t3_id = "t3:thread-1";

    for id in [standalone_id, preassigned_id, t3_id] {
        stub.on(
            &format!("GET /v1/agents/{}", enc(id)),
            agent_fixture(id, DEVICE, board_proto::AgentState::Running),
        );
    }
    stub.on("GET /v1/boards", [board_fixture("factory", &[])]);
    stub.on("GET /v1/boards/factory", board_fixture("factory", &[]));
    stub.on("GET /v1/agents", Vec::<board_proto::Agent>::new());
    let mut claimed = task_fixture("factory-12", "Shell task");
    claimed.status = TaskStatus::Doing;
    claimed.owner = Some(standalone_id.parse().unwrap());
    stub.on("POST /v1/tasks/factory-12/claim", &claimed);

    let home = home_for(dir.path(), &stub.url);
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    // 1. Standalone Antigravity shell: ANTIGRAVITY_CONVERSATION_ID resolves antigravity:<id>.
    let out = board(
        &home,
        &cwd,
        &[("ANTIGRAVITY_CONVERSATION_ID", "conv-shell-1")],
        &["status", "--board", "factory"],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("you: {standalone_id}")),
        "{}",
        stdout(&out)
    );
    let out = board(
        &home,
        &cwd,
        &[("ANTIGRAVITY_CONVERSATION_ID", "conv-shell-1")],
        &["claim", "factory-12"],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stub.requests_to("POST /v1/tasks/factory-12/claim")[0]
            .agent
            .as_deref(),
        Some(standalone_id)
    );

    // 2. Dispatched ACP shell: BOARD_AGENT wins over ANTIGRAVITY_CONVERSATION_ID so shell CLI
    //    and `board mcp` share one registered identity.
    let out = board(
        &home,
        &cwd,
        &[
            ("BOARD_AGENT", preassigned_id),
            ("ANTIGRAVITY_CONVERSATION_ID", "conv-acp-uuid"),
        ],
        &["status", "--board", "factory"],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("you: {preassigned_id}")),
        "{}",
        stdout(&out)
    );

    // 3. T3 thread hosting Antigravity: T3CODE_THREAD_ID wins over both.
    let out = board(
        &home,
        &cwd,
        &[
            ("T3CODE_THREAD_ID", "thread-1"),
            ("BOARD_AGENT", preassigned_id),
            ("ANTIGRAVITY_CONVERSATION_ID", "conv-shell-1"),
        ],
        &["status", "--board", "factory"],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("you: {t3_id}")),
        "{}",
        stdout(&out)
    );

    // 4. Standalone MCP with ANTIGRAVITY_CONVERSATION_ID also resolves automatically.
    let mut mcp = Mcp::spawn(
        &home,
        &cwd,
        &[("ANTIGRAVITY_CONVERSATION_ID", "conv-shell-1")],
    );
    mcp.init();
    let status = mcp.call(2, "board_status", json!({"board": "factory"}));
    assert!(
        text(&status).contains(&format!("you: {standalone_id}")),
        "{}",
        text(&status)
    );

    // 5. Missing agent id error text mentions Antigravity.
    let out = board(&home, &cwd, &[], &["claim", "factory-12"], "");
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("no agent id") && err.contains("Antigravity"),
        "{err}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn acp_runner_propagates_board_agent_to_board_mcp_grandchild_and_scrubs_ambient_env() {
    use std::os::unix::fs::PermissionsExt as _;

    use board::acp_runner::AcpRunner;
    use board::config::{AcpRunnerConfig, Home};
    use board::local_db::LocalDb;
    use board::runner::{RunSpec, Runner as _};

    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let agent_id = "antigravity:run-acp-e2e";
    stub_mutating_routes(&stub, agent_id);
    let home_path = home_for(dir.path(), &stub.url);
    let home = Home::at(home_path.clone());
    let db = LocalDb::open_for_home(&home).unwrap();
    let workdir = dir.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();

    // Mock ACP agent that spawns the real `board mcp` binary without `--agent` or
    // `ANTIGRAVITY_CONVERSATION_ID` (matching Antigravity ACP's startup MCP spawn) and runs
    // `board_status` + `task_claim` over stdio.
    let board_bin = env!("CARGO_BIN_EXE_board");
    let mcp_out = dir.path().join("mcp-out.jsonl");
    let env_check = dir.path().join("env-check.txt");
    let script_path = dir.path().join("mock-acp-mcp.sh");
    let script = format!(
        r#"#!/bin/sh
printf 'T3=%s|AG=%s|BA=%s' "${{T3CODE_THREAD_ID:-}}" "${{ANTIGRAVITY_CONVERSATION_ID:-}}" "${{BOARD_AGENT:-}}" > "{env_check}"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"agentCapabilities":{{"loadSession":true}}}}}}'
      ;;
    *'"method":"session/new"'*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"acp-conv-99"}}}}'
      ;;
    *'"method":"session/prompt"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      {{
        printf '%s\n' '{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"acp","version":"1"}}}}}}'
        printf '%s\n' '{{"jsonrpc":"2.0","method":"notifications/initialized"}}'
        printf '%s\n' '{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"board_status","arguments":{{"board":"factory"}}}}}}'
        printf '%s\n' '{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"task_claim","arguments":{{"id":"factory-12"}}}}}}'
        printf '%s\n' '{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"feedback_resolve","arguments":{{"id":"board-feedback-1240","kind":"fixed","task":"factory-12","reason":"resolved"}}}}}}'
      }} | "{board_bin}" mcp > "{mcp_out}" 2>/dev/null
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":${{id}},\"result\":{{\"stopReason\":\"end_turn\"}}}}"
      ;;
  esac
done
"#,
        env_check = env_check.display(),
        board_bin = board_bin,
        mcp_out = mcp_out.display(),
    );
    std::fs::write(&script_path, script).unwrap();
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut cfg = AcpRunnerConfig {
        bin: Some(script_path),
        ..AcpRunnerConfig::default()
    };
    cfg.env.insert(
        "BOARD_HOME".to_owned(),
        home_path.to_string_lossy().into_owned(),
    );
    cfg.env.insert("BOARD_TOKEN".to_owned(), TOKEN.to_owned());
    cfg.env.insert("BOARD_KEYCHAIN".to_owned(), "0".to_owned());

    let runner = AcpRunner::new(cfg, home, db);
    let spec = RunSpec {
        run_id: "run-acp-e2e".to_owned(),
        title: "agent-board-694 e2e".to_owned(),
        prompt: "verify mcp".to_owned(),
        model: None,
        worktree: None,
        metadata: BTreeMap::from([("agent_id".to_owned(), agent_id.to_owned())]),
    };

    let handle = runner.spawn(&spec, &workdir).await.unwrap();
    assert_eq!(handle.resolve_agent_id().unwrap().as_str(), agent_id);

    for _ in 0..100 {
        if !runner.status(&handle).await.unwrap().busy {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(
        std::fs::read_to_string(&env_check).unwrap(),
        format!("T3=|AG=|BA={agent_id}"),
        "AcpRunner must scrub ambient T3/Antigravity env and inject BOARD_AGENT"
    );

    let mcp_lines = std::fs::read_to_string(&mcp_out).unwrap();
    assert!(
        mcp_lines.contains(&format!("you: {agent_id}")),
        "grandchild board mcp must report pre-assigned identity: {mcp_lines}"
    );
    assert!(
        !mcp_lines.contains("no agent id"),
        "grandchild board mcp must not report no agent id: {mcp_lines}"
    );
    assert_eq!(
        stub.requests_to("POST /v1/tasks/factory-12/claim")[0]
            .agent
            .as_deref(),
        Some(agent_id)
    );
    assert_eq!(
        stub.requests_to("POST /v1/feedback/board-feedback-1240/resolve")[0]
            .agent
            .as_deref(),
        Some(agent_id)
    );
}
